//! `OriginService`: everything a repository can be asked, independent of
//! transport.
//!
//! It is the one API over a repository. The HTTP routes only translate
//! requests into its methods, and the command line calls them in-process
//! for the repository it runs in, so a local repository and a remote one
//! answer the same way. Plugins use it the same two ways.
//!
//! A service is meant to live as long as its process: it keeps the record
//! index open, catching it up with the op log before each use instead of
//! reading it whole, and wakes the event long-polls waiting on it whenever
//! it writes.
//!
//! Every method takes the [`Caller`], and does only what their [`Rights`]
//! allow, as [`permissions`](crate::permissions) explains: what each
//! caller reads is filtered, and what they write is checked.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use origin_platform::Native;
use origin_shared::access::{PathAccess, Rights, Token};
use origin_shared::api::{
    Annotation, At, BrowseQuery, ErrorCode, EventsQuery, FetchSnapshot, Inventory,
    InventoryRequest, IssuedToken, LandRequest, Landings, Listed, Listing, LogEntry, LogQuery,
    NewTarget, RevisionEntry, Revisions, Stored, TargetInfo, TokenRequest,
};
use origin_shared::event::{EventKind, Filter, Page};
use origin_shared::landing::{Blocked, Pending, Proposal};
use origin_shared::object::DecodeError;
use origin_shared::pack::Pack;
use origin_shared::{
    AcceptanceId, Base, ChangeId, EncodedObject, Head, NodeKey, Object, ObjectId, ObjectKind,
    Principal, PrincipalName, RecordId, RepoPath, Statement, StatementId, TargetName, TreeEntry,
    TreeId,
};

use crate::annotations::{AnnotateError, Annotations};
use crate::config::{ConfigError, UserConfig};
use crate::events::{Events, Wakeup};
use crate::index::Index;
pub use crate::permissions::Caller;
use crate::permissions::{Forbidden, PutError, Reader, Writer};
use crate::policy::{Policy, PolicyError};
use crate::queue::{LandError, Queue, Waiting};
use crate::store::{Store, StoreError};
use crate::tree::Snapshots;

/// A node: its name, and the key it signs acceptances with as the
/// authority of the targets its repositories' policies name it for. A
/// server's repositories share their node.
#[derive(Clone, Debug)]
pub struct Node {
    /// The node's name.
    pub name: PrincipalName,
    /// Its key.
    pub key: Arc<NodeKey>,
}

impl Node {
    /// The node of the user's configuration, with its key, generated the
    /// first time.
    ///
    /// # Errors
    ///
    /// Fails if the key cannot be read, or generated and saved.
    pub fn of(config: &UserConfig) -> Result<Self, ConfigError> {
        Ok(Self {
            name: config.node().clone(),
            key: Arc::new(config.node_key()?),
        })
    }

    /// The node as a principal, which issues the statements it makes.
    fn principal(&self) -> Principal {
        Principal::Node(self.name.clone())
    }
}

/// One repository, served by its node.
#[derive(Debug)]
pub struct OriginService {
    store: Store,
    node: Node,
    /// The record index, once read: see the module.
    index: Mutex<Option<Index>>,
    /// The policy in force, and the acceptance of `policy` holding it, once
    /// read.
    policy: Mutex<Option<(AcceptanceId, Arc<Policy>)>>,
    wakeup: Wakeup,
}

impl OriginService {
    /// The most events a page holds, and how many by default.
    const MAX_EVENTS: u32 = 1024;

    /// The longest a token issued lasts.
    pub const MAX_TOKEN_TTL: Duration = Duration::from_hours(7 * 24);

    /// The longest an event long-poll waits.
    pub const MAX_WAIT: Duration = Duration::from_mins(1);

    /// Serves the repository in `store` as `node`.
    #[must_use]
    pub fn new(store: Store, node: Node) -> Self {
        Self {
            store,
            node,
            index: Mutex::default(),
            policy: Mutex::default(),
            wakeup: Wakeup::default(),
        }
    }

    /// The service, with `index`, just read, as its index.
    #[must_use]
    pub fn with_index(self, index: Index) -> Self {
        Self {
            index: Mutex::new(Some(index)),
            ..self
        }
    }

    /// Creates a repository without a working copy, as a server hosts them,
    /// in `root`, which must not exist or be empty: its store, and the
    /// targets `policy`, holding `policy` as its `policy.toml`, and `main`.
    ///
    /// # Errors
    ///
    /// Fails if `root` isn't empty, the policy doesn't make `node` the
    /// authority of both targets, or the repository cannot be written.
    pub fn create(root: PathBuf, node: Node, policy: &str) -> Result<Self, ServiceError> {
        let not_empty = std::fs::read_dir(&root).is_ok_and(|mut entries| entries.next().is_some());
        if not_empty {
            return Err(ServiceError::NotEmpty(root));
        }
        let store = Store::new(root, Native::default());
        store.init()?;
        Queue::new(&store, &node.key, node.name.clone()).init(policy)?;
        Ok(Self::new(store, node))
    }

    /// Opens the repository without a working copy in `root`, which
    /// [`OriginService::create`] made.
    ///
    /// # Errors
    ///
    /// Fails if `root` has no repository, or is on a network file system.
    pub fn open(root: PathBuf, node: Node) -> Result<Self, ServiceError> {
        let store = Store::open_writable(root, Native::default())?;
        if store.head(&TargetName::POLICY)?.is_none() {
            return Err(ServiceError::NotARepository(store.root().to_owned()));
        }
        Ok(Self::new(store, node))
    }

    /// The repository's storage.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Runs `call` with the statements of the repository, through the
    /// index kept open, caught up first.
    fn annotations<'s, T>(
        &'s self,
        call: impl FnOnce(&mut Annotations<'s>) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        let mut kept = self.index.lock().unwrap_or_else(PoisonError::into_inner);
        // A failed refresh leaves no index, to read whole next time.
        let index = match kept.take() {
            Some(mut index) => {
                index.refresh(&self.store)?;
                index
            }
            None => Index::open(&self.store)?,
        };
        let mut annotations = Annotations::new(&self.store, index);
        let result = call(&mut annotations);
        *kept = Some(annotations.into_index());
        result
    }

    /// The policy in force, read again only once `policy` moved.
    fn policy(&self) -> Result<Arc<Policy>, ServiceError> {
        let mut kept = self.policy.lock().unwrap_or_else(PoisonError::into_inner);
        let head = self.store.head(&TargetName::POLICY)?;
        if let (Some(head), Some((at, policy))) = (head, &*kept)
            && head.acceptance == *at
        {
            return Ok(Arc::clone(policy));
        }
        let (at, policy) = match Policy::in_force(&self.store) {
            Ok((at, policy)) => (at, Arc::new(policy)),
            Err(PolicyError::Missing) => return Ok(Arc::new(Policy::default())),
            Err(error) => return Err(error.into()),
        };
        *kept = Some((at, Arc::clone(&policy)));
        Ok(policy)
    }

    /// What `caller` may do, under the policy in force.
    ///
    /// # Errors
    ///
    /// Fails if the policy cannot be read.
    pub fn rights(&self, caller: &Caller) -> Result<Rights, ServiceError> {
        Ok(match caller {
            Caller::Owner => Rights::all(),
            Caller::Anonymous => self.policy()?.rights(None),
            Caller::Principal { principal, scope } => {
                let rights = self.policy()?.rights(Some(principal));
                match scope {
                    Some(scope) => rights.narrowed(scope.clone()),
                    None => rights,
                }
            }
        })
    }

    /// `whoami`: what the caller may do, and who they are.
    ///
    /// # Errors
    ///
    /// Fails if the policy cannot be read.
    pub fn whoami(&self, caller: &Caller) -> Result<Rights, ServiceError> {
        self.rights(caller)
    }

    /// Issues a token acting for the caller, with the request's scope, which
    /// their rights must cover, for its `ttl`, at most
    /// [`OriginService::MAX_TOKEN_TTL`].
    ///
    /// # Errors
    ///
    /// Fails if the caller isn't signed in, or the scope reaches beyond
    /// their rights.
    pub fn issue_token(
        &self,
        caller: &Caller,
        request: &TokenRequest,
    ) -> Result<IssuedToken, ServiceError> {
        let Some(principal) = caller.principal() else {
            return Err(Forbidden::NoPrincipal.into());
        };
        let rights = self.rights(caller)?;
        if let Some(beyond) = request
            .scope
            .iter()
            .find(|&capability| !rights.cover(std::slice::from_ref(capability)))
        {
            return Err(Forbidden::Scope(beyond.clone()).into());
        }
        let token = Token::new(
            principal.clone(),
            request.scope.clone(),
            request.ttl.0.min(Self::MAX_TOKEN_TTL),
        );
        Ok(IssuedToken {
            token: token.sign(&self.node.key),
            principal: token.principal,
            expires: token.expires,
        })
    }

    /// The records `ids` name, in their order, as a pack: revisions,
    /// statements and acceptances the caller sees, the others left out,
    /// each followed by the attachment it carries, if the repository has
    /// it: a revision's description, a statement's payload. Snapshots only
    /// leave through [`OriginService::snapshot`].
    ///
    /// # Errors
    ///
    /// Fails if the repository doesn't have one, one is a blob, a tree or
    /// an attachment, or it cannot read one.
    pub fn records(&self, caller: &Caller, ids: &[ObjectId]) -> Result<Pack, ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights);
        let mut objects = Vec::with_capacity(ids.len());
        for &id in ids {
            let object = self.store.read(id)?;
            let (record, carried) = match object
                .decode()
                .map_err(|source| StoreError::Decode { id, source })?
            {
                Object::Blob(_) | Object::Tree(_) | Object::Attachment(_) => {
                    return Err(ServiceError::NotARecord(id));
                }
                Object::Revision(revision) => {
                    (RecordId::Revision(id.into()), Some(revision.description))
                }
                Object::Statement(statement) => (RecordId::Statement(id.into()), statement.payload),
                Object::Acceptance(_) => (RecordId::Acceptance(id.into()), None),
            };
            if !reader.sees(record)? {
                continue;
            }
            objects.push(object);
            // Only an attachment goes with a record: never a blob, a file's
            // content, whose id a record names instead.
            let carried = carried
                .map(|id| self.store.try_read(id.into()))
                .transpose()?
                .flatten()
                .filter(|object| object.kind().ok() == Some(ObjectKind::Attachment));
            objects.extend(carried);
        }
        Ok(Pack { objects })
    }

    /// The snapshot of a record the caller sees, a revision or an
    /// acceptance, if it is one.
    fn snapshot_of(
        &self,
        reader: &mut Reader<'_, Native>,
        record: RecordId,
    ) -> Result<TreeId, ServiceError> {
        let unknown = || ServiceError::UnknownRecord(record);
        let snapshot = match record {
            RecordId::Revision(id) => self.store.get(id).map(|revision| revision.snapshot),
            RecordId::Acceptance(id) => {
                self.store.get(id).map(|acceptance| acceptance.body().state)
            }
            // A landing's merged snapshot, for checks to read: never a
            // tree within one, whose paths would be read as others.
            RecordId::Snapshot(id) => {
                let waiting = Queue::pending(&self.store)?;
                if !waiting.iter().any(|pending| pending.subject == record) {
                    return Err(ServiceError::NotASnapshot(record));
                }
                Ok(id)
            }
            RecordId::Statement(_) => return Err(ServiceError::NotASnapshot(record)),
        };
        let snapshot = match snapshot {
            // Not a record of the kind it's named as.
            Err(StoreError::NotFound(_) | StoreError::Decode { .. }) => return Err(unknown()),
            snapshot => snapshot?,
        };
        if !reader.sees(record)? {
            return Err(unknown());
        }
        Ok(snapshot)
    }

    /// The objects of the snapshot of the record the request names that the
    /// caller lacks, as a pack, parents before children: those none of the
    /// snapshots it has holds at the same path, under the paths it asks
    /// for. The content of paths the caller may not read is withheld. See
    /// [`Store::added`].
    ///
    /// # Errors
    ///
    /// Fails if the record isn't a revision or an acceptance the caller
    /// sees, or the repository cannot read its snapshot.
    pub fn snapshot(&self, caller: &Caller, request: &FetchSnapshot) -> Result<Pack, ServiceError> {
        let rights = self.rights(caller)?;
        let snapshot = self.snapshot_of(&mut Reader::new(&self.store, &rights), request.of)?;
        let mut have = Vec::new();
        for &id in &request.have {
            let object = self.store.try_read(id.into())?;
            if object.is_some_and(|object| object.kind().ok() == Some(ObjectKind::Tree)) {
                have.push(id);
            }
        }
        let added = self.store.added(
            snapshot,
            &have,
            &request.paths,
            self.filter(&rights, request.of)?,
        )?;
        let ids: Vec<ObjectId> = added.iter().map(|placed| placed.entry.id).collect();
        Ok(Pack {
            objects: self.store.read_all(&ids)?,
        })
    }

    /// What a walk of the snapshot of `record` withholds from a caller with
    /// `rights`, if anything. Everyone who sees the repository reads the
    /// snapshots of `policy`: what each may do, and the keys that sign
    /// acceptances, so they can verify every target.
    fn filter<'r>(
        &self,
        rights: &'r Rights,
        record: RecordId,
    ) -> Result<Option<&'r PathAccess>, ServiceError> {
        if rights.paths.reads_all() {
            return Ok(None);
        }
        if let RecordId::Acceptance(id) = record
            && self.store.get(id)?.body().target == TargetName::POLICY.id()
        {
            return Ok(None);
        }
        Ok(Some(&rights.paths))
    }

    /// What `query` names, a directory or file of a snapshot the caller
    /// sees, the snapshot, and what it withholds from the caller.
    fn browse(
        &self,
        caller: &Caller,
        query: &BrowseQuery,
    ) -> Result<(Option<PathAccess>, TreeId, TreeEntry), ServiceError> {
        let rights = self.rights(caller)?;
        let record = match &query.at {
            At::Target(target) => {
                let head = self
                    .store
                    .head(target)?
                    .ok_or_else(|| ServiceError::UnknownTarget(target.clone()))?;
                RecordId::Acceptance(head.acceptance)
            }
            &At::Record(record) => record,
        };
        let snapshot = self.snapshot_of(&mut Reader::new(&self.store, &rights), record)?;
        let withheld = self.filter(&rights, record)?.cloned();
        let entry = Snapshots::new(&self.store)
            .entry_at(snapshot, &query.path)?
            .ok_or_else(|| ServiceError::NoSuchPath(query.path.clone()))?;
        if withheld
            .as_ref()
            .is_some_and(|access| !access.reads_entry(&query.path, &entry))
        {
            return Err(Forbidden::Read(query.path.clone()).into());
        }
        Ok((withheld, snapshot, entry))
    }

    /// Lists a directory of a snapshot: of a target's tip, or of a revision
    /// or an acceptance the caller sees. Entries whose content is withheld
    /// from the caller keep their place, marked.
    ///
    /// # Errors
    ///
    /// Fails if the target or record doesn't exist or isn't seen, the path
    /// isn't a directory, the caller reads nothing under it, or a tree
    /// cannot be read.
    pub fn tree(&self, caller: &Caller, query: &BrowseQuery) -> Result<Listing, ServiceError> {
        let (withheld, snapshot, entry) = self.browse(caller, query)?;
        let tree = entry
            .subtree()
            .ok_or_else(|| ServiceError::NotADirectory(query.path.clone()))?;
        let entries = self
            .store
            .get(tree)?
            .iter()
            .map(|(name, &entry)| Listed {
                name: name.clone(),
                kind: entry.kind,
                id: entry.id,
                withheld: withheld
                    .as_ref()
                    .is_some_and(|access| !access.reads_entry(&query.path.join(name), &entry)),
            })
            .collect();
        Ok(Listing {
            snapshot,
            path: query.path.clone(),
            entries,
        })
    }

    /// The content of a file of a snapshot, as [`OriginService::tree`]
    /// finds it, which the caller must read.
    ///
    /// # Errors
    ///
    /// Fails like [`OriginService::tree`], or if the path isn't a file.
    pub fn file(&self, caller: &Caller, query: &BrowseQuery) -> Result<Vec<u8>, ServiceError> {
        let (_, _, entry) = self.browse(caller, query)?;
        if entry.kind.is_tree() {
            return Err(ServiceError::NotAFile(query.path.clone()));
        }
        Ok(self
            .store
            .try_blob(entry.id)?
            .ok_or(StoreError::NotFound(entry.id))?)
    }

    /// What the repository holds of each change that the caller sees: its
    /// revisions and heads, and the statements about them, by id. A change
    /// it never heard of has none.
    ///
    /// # Errors
    ///
    /// Fails if the index or a record cannot be read.
    pub fn inventory(
        &self,
        caller: &Caller,
        request: &InventoryRequest,
    ) -> Result<Vec<Inventory>, ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights);
        self.annotations(|annotations| {
            let index = annotations.index();
            let mut inventories = Vec::new();
            for &change in &request.changes {
                let mut revisions = Vec::new();
                for &id in index.revisions(change) {
                    if reader.sees(RecordId::Revision(id))? {
                        revisions.push(id);
                    }
                }
                let mut heads = index.heads(change);
                heads.retain(|head| revisions.contains(head));
                let mut subjects: Vec<_> =
                    revisions.iter().copied().map(RecordId::Revision).collect();
                let mut statements = Vec::new();
                while let Some(subject) = subjects.pop() {
                    for &statement in index.statements(subject) {
                        statements.push(statement);
                        subjects.push(RecordId::Statement(statement));
                    }
                }
                inventories.push(Inventory {
                    change,
                    heads,
                    revisions,
                    statements,
                });
            }
            Ok(inventories)
        })
    }

    /// Writes the objects of `pack`: snapshots, attachments, revisions and
    /// statements. Each must be well formed, and the repository must have,
    /// or the pack hold, everything it needs: a tree's entries, a
    /// revision's snapshot, description, base and dependencies, a
    /// statement's subject and the statements it supersedes or promotes.
    /// So the repository never holds a record it cannot use. Descriptions
    /// and payloads must be attachments. Acceptances are refused: only a
    /// target's authority writes them, by landing. The records a caller
    /// other than the owner puts must be theirs, and may only change what
    /// they write, as [`permissions`](crate::permissions) explains. The
    /// blobs and trees are durable before the records that refer to them,
    /// and promotions come last, each checked against its original by
    /// [`Store::claim`], once the original and both subjects are stored.
    /// Issues `core.divergent` for the changes the revisions made diverge,
    /// and retries the landings waiting for evidence about what the
    /// statements are about.
    ///
    /// # Errors
    ///
    /// Fails if the caller is anonymous, an object is malformed, an
    /// acceptance, or needs an object neither the pack nor the repository
    /// has, if the caller may not put a record, if a promotion doesn't
    /// match its original, which leaves the other records stored, or if
    /// the objects cannot be written.
    pub fn put_records(&self, caller: &Caller, pack: Pack) -> Result<Stored, ServiceError> {
        if *caller == Caller::Anonymous {
            return Err(Forbidden::Anonymous.into());
        }
        // Objects this finds stored stay until what refers to them is.
        let _held = self.store.hold_objects()?;
        let held: HashMap<ObjectId, Option<ObjectKind>> = pack
            .objects
            .iter()
            .map(|object| (object.id(), object.kind().ok()))
            .collect();
        let kind = |id: ObjectId| -> Result<Option<ObjectKind>, StoreError> {
            match held.get(&id) {
                Some(&kind) => Ok(kind),
                None => Ok(self
                    .store
                    .try_read(id)?
                    .and_then(|object| object.kind().ok())),
            }
        };
        // Makes what the repository has of the dependencies durable before
        // anything refers to them: their writers may not have yet.
        let dependencies = self.store.batch();
        let mut snapshots = Vec::new();
        let mut records = Vec::new();
        let mut promotions = Vec::new();
        let mut decoded = Vec::new();
        let mut changes = HashSet::new();
        let mut subjects = HashSet::new();
        for object in pack.objects {
            let id = object.id();
            let record = object
                .decode()
                .map_err(|source| ServiceError::Malformed { id, source })?;
            for needed in Dependencies::of(&record).all() {
                if !held.contains_key(&needed) && !dependencies.depend(needed)? {
                    return Err(ServiceError::Missing { id, needed });
                }
            }
            Self::check_attachment(id, &record, kind)?;
            match record {
                Object::Acceptance(_) => return Err(ServiceError::Acceptance(id.into())),
                Object::Blob(_) | Object::Tree(_) | Object::Attachment(_) => snapshots.push(object),
                Object::Revision(ref revision) => {
                    if !revision.predecessors.is_empty() {
                        changes.insert(revision.change);
                    }
                    records.push(object);
                    decoded.push((id, record));
                }
                Object::Statement(ref statement) if statement.promoted_from.is_some() => {
                    subjects.insert(statement.subject);
                    promotions.push((object, statement.clone()));
                    decoded.push((id, record));
                }
                Object::Statement(ref statement) => {
                    subjects.insert(statement.subject);
                    records.push(object);
                    decoded.push((id, record));
                }
            }
        }
        dependencies.commit()?;
        let mut new = 0;
        if !snapshots.is_empty() {
            new += self.store.write_all(&snapshots)?;
        }
        if let Caller::Principal { principal, .. } = caller {
            let rights = self.rights(caller)?;
            let supplied: HashSet<ObjectId> = held.keys().copied().collect();
            let writer = Writer::new(&self.store, principal, &rights, &supplied);
            for (id, record) in &decoded {
                // Re-putting a record changes nothing.
                if self.store.contains(*id)? {
                    continue;
                }
                match record {
                    Object::Revision(revision) => writer.revision((*id).into(), revision)?,
                    Object::Statement(statement) => writer.statement(statement)?,
                    _ => {}
                }
            }
        }
        if !records.is_empty() {
            new += self.store.write_all(&records)?;
        }
        new += self.put_promotions(promotions)?;
        if !changes.is_empty() {
            self.annotations(|annotations| {
                for &change in &changes {
                    annotations.flag_divergence(change, self.node.principal())?;
                }
                Ok(())
            })?;
        }
        self.wakeup.notify();
        self.resume(&subjects)?;
        Ok(Stored { new: new as u64 })
    }

    /// Checks that the description or payload `record`, `id`, refers to,
    /// if any, is an attachment, as far as `kind` knows the object.
    fn check_attachment(
        id: ObjectId,
        record: &Object,
        kind: impl Fn(ObjectId) -> Result<Option<ObjectKind>, StoreError>,
    ) -> Result<(), ServiceError> {
        let attachment = match record {
            Object::Revision(revision) => Some(revision.description),
            Object::Statement(statement) => statement.payload,
            _ => None,
        };
        if let Some(attachment) = attachment
            && kind(attachment.into())?.is_some_and(|kind| kind != ObjectKind::Attachment)
        {
            return Err(ServiceError::NotAnAttachment {
                id,
                attachment: attachment.into(),
            });
        }
        Ok(())
    }

    /// Retries the landings waiting for evidence about one of `subjects`,
    /// which statements were just issued about, each for its submitter:
    /// the queue continues when the evidence arrives. A retry the policy in
    /// force refuses, or that fails, leaves the landing as it was, and is
    /// logged: the statements that triggered it stand.
    ///
    /// # Errors
    ///
    /// Fails if the waiting landings cannot be read.
    fn resume(&self, subjects: &HashSet<RecordId>) -> Result<(), ServiceError> {
        if subjects.is_empty() {
            return Ok(());
        }
        for Waiting { pending, submitter } in Queue::waiting(&self.store)? {
            if !matches!(pending.blocked, Blocked::Evidence(_))
                || !subjects.contains(&pending.subject)
            {
                continue;
            }
            let request = LandRequest {
                proposal: pending.proposal,
                onto: Some(pending.onto),
            };
            if let Err(error) = self.land(&submitter, &request) {
                tracing::warn!(
                    error = %anyhow::Error::from(error),
                    "a waiting landing cannot continue"
                );
            }
        }
        Ok(())
    }

    /// Writes the `promotions` [`OriginService::put_records`] puts, once
    /// [`Store::claim`] accepts each, returning how many were new. Their
    /// originals are never promotions, so they're stored by now.
    fn put_promotions(
        &self,
        promotions: Vec<(EncodedObject, Statement)>,
    ) -> Result<usize, ServiceError> {
        let mut checked = Vec::new();
        for (object, statement) in promotions {
            let id = object.id().into();
            self.store
                .claim(id, statement)?
                .map_err(|invalid| ServiceError::Statement {
                    id,
                    source: Box::new(invalid.into()),
                })?;
            checked.push(object);
        }
        if checked.is_empty() {
            return Ok(0);
        }
        Ok(self.store.write_all(&checked)?)
    }

    /// Promotes results to `subject`, a revision, an acceptance or a
    /// snapshot the caller sees, from the records with the same input
    /// digest, as statements this node issues, and returns them: what a
    /// check that would run again can skip. A promotion claims nothing new,
    /// so any signed-in caller may ask for one. Landings waiting for
    /// evidence about the subject are retried.
    ///
    /// # Errors
    ///
    /// Fails if the caller is anonymous, or a record cannot be read or
    /// written.
    pub fn promote(
        &self,
        caller: &Caller,
        subject: RecordId,
    ) -> Result<Vec<Annotation>, ServiceError> {
        if *caller == Caller::Anonymous {
            return Err(Forbidden::Anonymous.into());
        }
        let rights = self.rights(caller)?;
        if !Reader::new(&self.store, &rights).sees(subject)? {
            return Err(ServiceError::UnknownRecord(subject));
        }
        let promoted = self
            .annotations(|annotations| Ok(annotations.promote(subject, &self.node.principal())?))?;
        if !promoted.is_empty() {
            self.wakeup.notify();
            self.resume(&HashSet::from([subject]))?;
        }
        Ok(promoted
            .into_iter()
            .map(|(id, statement)| Annotation {
                id,
                statement,
                superseded_by: None,
            })
            .collect())
    }

    /// The statements about `subject`, in the order the repository received
    /// them: none if the caller doesn't see it.
    ///
    /// # Errors
    ///
    /// Fails if the index or a statement cannot be read.
    pub fn statements(
        &self,
        caller: &Caller,
        subject: RecordId,
    ) -> Result<Vec<Annotation>, ServiceError> {
        let rights = self.rights(caller)?;
        if !Reader::new(&self.store, &rights).sees(subject)? {
            return Ok(Vec::new());
        }
        self.annotations(|annotations| Ok(annotations.about(subject)?))
    }

    /// The revisions of `change` the caller sees, and its heads among them.
    ///
    /// # Errors
    ///
    /// Fails if the repository has no revision of the change the caller
    /// sees, or cannot read one.
    pub fn revisions(&self, caller: &Caller, change: ChangeId) -> Result<Revisions, ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights);
        let (mut ids, mut heads) = self.annotations(|annotations| {
            let index = annotations.index();
            Ok((index.revisions(change).to_vec(), index.heads(change)))
        })?;
        let mut seen = Vec::with_capacity(ids.len());
        for id in ids.drain(..) {
            if reader.sees(RecordId::Revision(id))? {
                seen.push(id);
            }
        }
        heads.retain(|head| seen.contains(head));
        if seen.is_empty() {
            return Err(ServiceError::UnknownChange(change));
        }
        let revisions = seen
            .into_iter()
            .map(|id| {
                Ok(RevisionEntry {
                    id,
                    revision: self.store.get(id)?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(Revisions { revisions, heads })
    }

    /// Every target, by name, with its tip, and its authority and
    /// requirements under the policy in force.
    ///
    /// # Errors
    ///
    /// Fails if the targets or the policy cannot be read.
    pub fn targets(&self, _caller: &Caller) -> Result<Vec<TargetInfo>, ServiceError> {
        let policy = Policy::load(&self.store)?;
        let mut names = self.store.targets()?;
        names.sort();
        let mut targets = Vec::new();
        for name in names {
            let Some(head) = self.store.head(&name)? else {
                continue;
            };
            targets.push(TargetInfo {
                authority: policy.authority(&name).cloned(),
                requirements: policy.requirements(&name).to_vec(),
                head,
                name,
            });
        }
        Ok(targets)
    }

    /// The acceptances of the query's target from position `since` on,
    /// oldest first, up to its `limit`.
    ///
    /// # Errors
    ///
    /// Fails if the target doesn't exist, or its log or an acceptance
    /// cannot be read.
    pub fn log(&self, _caller: &Caller, query: &LogQuery) -> Result<Vec<LogEntry>, ServiceError> {
        let target = &query.target;
        if self.store.head(target)?.is_none() {
            return Err(ServiceError::UnknownTarget(target.clone()));
        }
        let mut ids = self.store.log_since(target, query.since)?;
        if let Some(limit) = query.limit {
            ids.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        ids.into_iter()
            .map(|id| {
                Ok(LogEntry {
                    id,
                    acceptance: self.store.get(id)?,
                })
            })
            .collect()
    }

    /// Lands what `request` proposes, with this node as the authority: a
    /// change with the changes below it the target doesn't have yet, or
    /// another target's tip. A change lands by default on the target its
    /// stack is on. The caller must be signed in, may land on the target,
    /// and must write every path the landing changes on it.
    ///
    /// # Errors
    ///
    /// Fails if the change or a target doesn't exist, a target was given
    /// no target to land on, the caller may not land there, this node isn't
    /// the target's authority, the change can't land as it is (see
    /// [`LandError`]), or the records cannot be read or written.
    pub fn land(&self, caller: &Caller, request: &LandRequest) -> Result<Landings, ServiceError> {
        if *caller == Caller::Anonymous {
            return Err(Forbidden::Anonymous.into());
        }
        let rights = self.rights(caller)?;
        let may_land = |onto: &TargetName| {
            if rights.may_land(onto) {
                Ok(())
            } else {
                Err(ServiceError::from(Forbidden::Land(onto.clone())))
            }
        };
        let queue = Queue::new(&self.store, &self.node.key, self.node.name.clone())
            .submitted_by(caller.clone(), rights.paths.clone());
        let landed = self.annotations(|annotations| {
            let landed = match &request.proposal {
                Proposal::Change(change) => {
                    let onto = match &request.onto {
                        Some(onto) => onto.clone(),
                        None => self.target_of(annotations.index(), *change)?,
                    };
                    may_land(&onto)?;
                    let landings = queue.land_change(annotations, *change, &onto)?;
                    Landings { onto, landings }
                }
                Proposal::Target(source) => {
                    let onto = request.onto.clone().ok_or(ServiceError::NoTarget)?;
                    may_land(&onto)?;
                    let landing = queue.land_target(annotations, source, &onto)?;
                    Landings {
                        onto,
                        landings: vec![landing],
                    }
                }
            };
            Ok(landed)
        });
        // Even a failed landing may have written records, such as a
        // conflict.
        self.wakeup.notify();
        landed
    }

    /// The target the stack of `change` is on: that of its head's.
    fn target_of(&self, index: &Index, change: ChangeId) -> Result<TargetName, ServiceError> {
        let head = match index.heads(change)[..] {
            [head] => head,
            [] => return Err(ServiceError::UnknownChange(change)),
            ref heads => {
                return Err(LandError::Divergent {
                    change,
                    heads: heads.to_vec(),
                }
                .into());
            }
        };
        Ok(self.store.stack_target(head)?)
    }

    /// Every landing that waits, on every target, and why: those about what
    /// the caller sees.
    ///
    /// # Errors
    ///
    /// Fails if the queues or a record cannot be read.
    pub fn landings(&self, caller: &Caller) -> Result<Vec<Pending>, ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights);
        let mut seen = Vec::new();
        for pending in Queue::pending(&self.store)? {
            if reader.sees(pending.subject)? {
                seen.push(pending);
            }
        }
        Ok(seen)
    }

    /// Creates a target at the tip of another, with this node as its
    /// authority. The caller must be signed in, and may land on it.
    ///
    /// # Errors
    ///
    /// Fails if the caller may not land on the target, the name is taken,
    /// the other target doesn't exist, the policy wouldn't make this node
    /// the new target's authority, or the records cannot be written.
    pub fn create_target(
        &self,
        caller: &Caller,
        request: &NewTarget,
    ) -> Result<Head, ServiceError> {
        if *caller == Caller::Anonymous {
            return Err(Forbidden::Anonymous.into());
        }
        if !self.rights(caller)?.may_land(&request.name) {
            return Err(Forbidden::Land(request.name.clone()).into());
        }
        let queue = Queue::new(&self.store, &self.node.key, self.node.name.clone());
        let head = queue.create_target(&request.name, &request.from)?;
        self.wakeup.notify();
        Ok(head)
    }

    /// The events after the query's cursor that pass its filter, about what
    /// the caller sees, waiting up to its `wait`, at most
    /// [`OriginService::MAX_WAIT`], for some to come. Without a cursor, only
    /// events from now on come.
    ///
    /// # Errors
    ///
    /// Fails if the op log no longer keeps the events after the cursor, or
    /// it or a record cannot be read.
    pub fn events(&self, caller: &Caller, query: &EventsQuery) -> Result<Page, ServiceError> {
        let rights = self.rights(caller)?;
        let mut reader = Reader::new(&self.store, &rights);
        let events = Events::new(&self.store).waking_on(&self.wakeup);
        let cursor = match query.cursor {
            Some(cursor) => cursor,
            None => events.end()?,
        };
        let filter = Filter {
            kinds: query.kind.0.clone(),
            subjects: query.subject.0.clone(),
        };
        let limit = query
            .limit
            .unwrap_or(Self::MAX_EVENTS)
            .min(Self::MAX_EVENTS) as usize;
        let wait = query.wait.map_or(Duration::ZERO, |wait| wait.0);
        let accept = |event: &EventKind| Ok(filter.matches(event) && reader.sees_event(event)?);
        Ok(events.wait(cursor, accept, limit, wait.min(Self::MAX_WAIT))?)
    }
}

/// What an object needs the repository to have for it to be usable, as
/// [`OriginService::put_records`] lists: the records it refers to, and the
/// content, snapshots and blobs. A revision's predecessors and a
/// statement's payload are history and details: the repository may lack
/// them, and retention may have dropped a payload. An acceptance needs
/// nothing here: its chain brings what it refers to.
#[derive(Debug, Default)]
pub(crate) struct Dependencies {
    /// Revisions, statements and acceptances.
    pub records: Vec<ObjectId>,
    /// Trees and blobs.
    pub content: Vec<ObjectId>,
}

impl Dependencies {
    pub(crate) fn of(object: &Object) -> Self {
        match object {
            Object::Blob(_) | Object::Attachment(_) | Object::Acceptance(_) => Self::default(),
            Object::Tree(tree) => Self {
                records: Vec::new(),
                content: tree.iter().map(|(_, entry)| entry.id).collect(),
            },
            Object::Revision(revision) => {
                let base = match revision.base {
                    Base::Acceptance(id) => id.into(),
                    Base::Revision(id) => id.into(),
                };
                let mut records = vec![base];
                records.extend(revision.deps.iter().map(|&id| ObjectId::from(id)));
                Self {
                    records,
                    content: vec![revision.snapshot.into(), revision.description.into()],
                }
            }
            Object::Statement(statement) => {
                let mut dependencies = Self::default();
                match statement.subject {
                    RecordId::Snapshot(id) => dependencies.content.push(id.into()),
                    subject => dependencies.records.push(subject.into()),
                }
                dependencies.records.extend(
                    [statement.supersedes, statement.promoted_from]
                        .into_iter()
                        .flatten()
                        .map(ObjectId::from),
                );
                dependencies
            }
        }
    }

    /// Every object needed.
    pub(crate) fn all(&self) -> impl Iterator<Item = ObjectId> {
        self.records.iter().chain(&self.content).copied()
    }
}

/// A failed service call.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The server hosts no such repository.
    #[error("repository `{0}` not found")]
    UnknownRepository(origin_shared::api::RepoName),
    /// A directory holds no repository.
    #[error("{} is not an origin repository", .0.display())]
    NotARepository(PathBuf),
    /// `create` was pointed at a directory with files in it.
    #[error("{} is not empty", .0.display())]
    NotEmpty(PathBuf),
    /// No revision of the change is known.
    #[error("change {} has no revisions", .0.short())]
    UnknownChange(ChangeId),
    /// No target has this name.
    #[error("no target is named `{0}`")]
    UnknownTarget(TargetName),
    /// The repository has no such snapshot.
    #[error("snapshot {0} not found")]
    UnknownSnapshot(TreeId),
    /// A blob, a tree or an attachment was fetched as a record.
    #[error("{0} is not a record: fetch snapshots by walking them")]
    NotARecord(ObjectId),
    /// The repository has no such record, or the caller doesn't see it.
    #[error("record {0} not found")]
    UnknownRecord(RecordId),
    /// A snapshot was asked of a record that has none.
    #[error("{0} has no snapshot: name a revision or an acceptance")]
    NotASnapshot(RecordId),
    /// A snapshot has nothing at a path.
    #[error("nothing is at `{0}`")]
    NoSuchPath(RepoPath),
    /// A path listed is a file.
    #[error("`{0}` is not a directory")]
    NotADirectory(RepoPath),
    /// A path read is a directory.
    #[error("`{0}` is not a file")]
    NotAFile(RepoPath),
    /// A record put names an object that isn't an attachment as its
    /// description or payload.
    #[error("record {id} names {attachment} as its attachment, which is not one")]
    NotAnAttachment {
        /// The record.
        id: ObjectId,
        /// What it names.
        attachment: ObjectId,
    },
    /// The caller may not do this.
    #[error(transparent)]
    Forbidden(#[from] Forbidden),
    /// A target was proposed for landing without a target to land on.
    #[error("landing a target needs a target to land on")]
    NoTarget,
    /// An object put isn't a well-formed object.
    #[error("object {id} is malformed")]
    Malformed {
        /// The object.
        id: ObjectId,
        /// What is wrong.
        source: DecodeError,
    },
    /// An acceptance was put.
    #[error(
        "acceptance {0} cannot be put: only a target's authority writes acceptances, by landing"
    )]
    Acceptance(origin_shared::AcceptanceId),
    /// An object put needs one neither the batch nor the repository has.
    #[error("object {id} needs object {needed}, which neither the batch nor the repository has")]
    Missing {
        /// The object put.
        id: ObjectId,
        /// What it needs.
        needed: ObjectId,
    },
    /// A statement put isn't one the repository accepts: a promotion that
    /// doesn't match its original.
    #[error("statement {} cannot be put", .id.short())]
    Statement {
        /// The statement.
        id: StatementId,
        /// Why, boxed: it's large, and rare.
        source: Box<AnnotateError>,
    },
    /// A landing failed.
    #[error(transparent)]
    Land(#[from] LandError),
    /// The policy cannot be read.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// Records cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<PutError> for ServiceError {
    fn from(error: PutError) -> Self {
        match error {
            PutError::Forbidden(error) => Self::Forbidden(error),
            PutError::Store(error) => Self::Store(error),
        }
    }
}

impl ServiceError {
    /// What kind of failure it is, for clients to tell apart.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::UnknownRepository(_)
            | Self::UnknownChange(_)
            | Self::UnknownTarget(_)
            | Self::UnknownSnapshot(_)
            | Self::UnknownRecord(_)
            | Self::NoSuchPath(_) => ErrorCode::NotFound,
            Self::NoTarget
            | Self::NotARecord(_)
            | Self::NotASnapshot(_)
            | Self::NotADirectory(_)
            | Self::NotAFile(_)
            | Self::NotAnAttachment { .. }
            | Self::Malformed { .. }
            | Self::Acceptance(_)
            | Self::Missing { .. } => ErrorCode::BadRequest,
            Self::Statement { source, .. } => match &**source {
                AnnotateError::Store(error) => Self::store_code(error),
                _ => ErrorCode::BadRequest,
            },
            Self::Forbidden(_) => ErrorCode::Forbidden,
            Self::Land(error) => match error {
                LandError::UnknownTarget(_) | LandError::UnknownChange(_) => ErrorCode::NotFound,
                LandError::TargetExists(_)
                | LandError::SameTarget(_)
                | LandError::NoAuthority(_)
                | LandError::NotAuthority { .. }
                | LandError::WrongKey(_)
                | LandError::Divergent { .. }
                | LandError::Abandoned(_)
                | LandError::Unresolved { .. }
                | LandError::InvalidPolicy(_) => ErrorCode::Conflict,
                LandError::Forbidden(_) => ErrorCode::Forbidden,
                LandError::Store(error) => Self::store_code(error),
                LandError::Policy(_) | LandError::Annotate(_) => ErrorCode::Internal,
            },
            Self::Store(error) => Self::store_code(error),
            Self::NotARepository(_) | Self::NotEmpty(_) | Self::Policy(_) => ErrorCode::Internal,
        }
    }

    fn store_code(error: &StoreError) -> ErrorCode {
        match error {
            StoreError::NotFound(_) => ErrorCode::NotFound,
            StoreError::Expired { .. } => ErrorCode::Expired,
            StoreError::Conflict { .. } => ErrorCode::Conflict,
            _ => ErrorCode::Internal,
        }
    }
}
