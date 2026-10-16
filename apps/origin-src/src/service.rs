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
//! Every method takes the [`Caller`]. Nothing is checked yet: every caller
//! may read and write everything.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use origin_platform::Native;
use origin_shared::api::{
    Annotation, ErrorCode, EventsQuery, FetchSnapshot, Inventory, InventoryRequest, LandRequest,
    Landings, LogEntry, LogQuery, NewTarget, RevisionEntry, Revisions, Stored, TargetInfo,
};
use origin_shared::event::{Filter, Page};
use origin_shared::landing::{Pending, Proposal};
use origin_shared::object::DecodeError;
use origin_shared::pack::Pack;
use origin_shared::{
    Base, ChangeId, EncodedObject, Head, NodeKey, Object, ObjectId, ObjectKind, Principal,
    PrincipalName, RecordId, StatementId, TargetName, TreeId,
};

use crate::annotations::{AnnotateError, Annotations};
use crate::events::{Events, Wakeup};
use crate::index::Index;
use crate::policy::{Policy, PolicyError};
use crate::queue::{LandError, Queue};
use crate::store::{Store, StoreError};

/// Who calls the service. Permissions will decide what each may do by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The node's own user, through the command line.
    Owner,
    /// A client of a server that didn't authenticate.
    Anonymous,
}

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
    wakeup: Wakeup,
}

impl OriginService {
    /// The most events a page holds, and how many by default.
    const MAX_EVENTS: u32 = 1024;

    /// The longest an event long-poll waits.
    pub const MAX_WAIT: Duration = Duration::from_mins(1);

    /// Serves the repository in `store` as `node`.
    #[must_use]
    pub fn new(store: Store, node: Node) -> Self {
        Self {
            store,
            node,
            index: Mutex::default(),
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

    /// The records `ids` name, in their order, as a pack: revisions,
    /// statements and acceptances, each followed by the blob it carries, if
    /// the repository has it: a revision's description, a statement's
    /// payload. Snapshots only leave through [`OriginService::snapshot`].
    ///
    /// # Errors
    ///
    /// Fails if the repository doesn't have one, one is a blob or a tree, or
    /// it cannot read one.
    pub fn records(&self, _caller: &Caller, ids: &[ObjectId]) -> Result<Pack, ServiceError> {
        let mut objects = Vec::with_capacity(ids.len());
        for &id in ids {
            let object = self.store.read(id)?;
            let carried = match object
                .decode()
                .map_err(|source| StoreError::Decode { id, source })?
            {
                Object::Blob(_) | Object::Tree(_) => return Err(ServiceError::NotARecord(id)),
                Object::Revision(revision) => Some(revision.description.into()),
                Object::Statement(statement) => statement.payload.map(Into::into),
                Object::Acceptance(_) => None,
            };
            objects.push(object);
            objects.extend(
                carried
                    .map(|id| self.store.try_read(id))
                    .transpose()?
                    .flatten(),
            );
        }
        Ok(Pack { objects })
    }

    /// The objects of the snapshot the request names that the caller lacks,
    /// as a pack, parents before children: those none of the snapshots it
    /// has holds at the same path, under the paths it asks for. See
    /// [`Store::added`].
    ///
    /// # Errors
    ///
    /// Fails if the repository doesn't have the snapshot, or cannot read
    /// it.
    pub fn snapshot(
        &self,
        _caller: &Caller,
        request: &FetchSnapshot,
    ) -> Result<Pack, ServiceError> {
        let is_tree = |id: TreeId| -> Result<bool, StoreError> {
            let object = self.store.try_read(id.into())?;
            Ok(object.is_some_and(|object| object.kind().ok() == Some(ObjectKind::Tree)))
        };
        if !is_tree(request.snapshot)? {
            return Err(ServiceError::UnknownSnapshot(request.snapshot));
        }
        let mut have = Vec::new();
        for &id in &request.have {
            if is_tree(id)? {
                have.push(id);
            }
        }
        let added = self.store.added(request.snapshot, &have, &request.paths)?;
        let ids: Vec<ObjectId> = added.iter().map(|placed| placed.entry.id).collect();
        Ok(Pack {
            objects: self.store.read_all(&ids)?,
        })
    }

    /// What the repository holds of each change: its revisions and heads,
    /// and the statements about them, by id. A change it never heard of
    /// has none.
    ///
    /// # Errors
    ///
    /// Fails if the index cannot be read.
    pub fn inventory(
        &self,
        _caller: &Caller,
        request: &InventoryRequest,
    ) -> Result<Vec<Inventory>, ServiceError> {
        self.annotations(|annotations| {
            let index = annotations.index();
            Ok(request
                .changes
                .iter()
                .map(|&change| {
                    let revisions = index.revisions(change).to_vec();
                    let mut subjects: Vec<_> =
                        revisions.iter().copied().map(RecordId::Revision).collect();
                    let mut statements = Vec::new();
                    while let Some(subject) = subjects.pop() {
                        for &statement in index.statements(subject) {
                            statements.push(statement);
                            subjects.push(RecordId::Statement(statement));
                        }
                    }
                    Inventory {
                        change,
                        heads: index.heads(change),
                        revisions,
                        statements,
                    }
                })
                .collect())
        })
    }

    /// Writes the objects of `pack`: snapshots, revisions and statements.
    /// Each must be well formed, and the repository must have, or the
    /// pack hold, everything it needs: a tree's entries, a revision's
    /// snapshot, description, base and dependencies, a statement's subject
    /// and the statements it supersedes or promotes. So the repository
    /// never holds a record it cannot use. Acceptances are
    /// refused: only a target's authority writes them, by landing. The
    /// blobs and trees are durable before the records that refer to them,
    /// and promotions come last, each checked against its original by
    /// [`Store::claim`], once the original and both subjects are stored.
    /// Issues `core.divergent` for the changes the revisions made diverge.
    ///
    /// # Errors
    ///
    /// Fails if an object is malformed, an acceptance, or needs an object
    /// neither the pack nor the repository has, if a promotion doesn't
    /// match its original, which leaves the other records stored, or if
    /// the objects cannot be written.
    pub fn put_records(&self, _caller: &Caller, pack: Pack) -> Result<Stored, ServiceError> {
        // Objects this finds stored stay until what refers to them is.
        let _held = self.store.hold_objects()?;
        let held: HashSet<ObjectId> = pack.objects.iter().map(EncodedObject::id).collect();
        // Makes what the repository has of the dependencies durable before
        // anything refers to them: their writers may not have yet.
        let dependencies = self.store.batch();
        let mut snapshots = Vec::new();
        let mut records = Vec::new();
        let mut promotions = Vec::new();
        let mut changes = HashSet::new();
        for object in pack.objects {
            let id = object.id();
            let decoded = object
                .decode()
                .map_err(|source| ServiceError::Malformed { id, source })?;
            for needed in Dependencies::of(&decoded).all() {
                if !held.contains(&needed) && !dependencies.depend(needed)? {
                    return Err(ServiceError::Missing { id, needed });
                }
            }
            match decoded {
                Object::Acceptance(_) => return Err(ServiceError::Acceptance(id.into())),
                Object::Blob(_) | Object::Tree(_) => snapshots.push(object),
                Object::Revision(revision) => {
                    if !revision.predecessors.is_empty() {
                        changes.insert(revision.change);
                    }
                    records.push(object);
                }
                Object::Statement(statement) if statement.promoted_from.is_some() => {
                    promotions.push((object, statement));
                }
                Object::Statement(_) => records.push(object),
            }
        }
        dependencies.commit()?;
        let mut new = 0;
        for layer in [snapshots, records] {
            if !layer.is_empty() {
                new += self.store.write_all(&layer)?;
            }
        }
        // An original is never a promotion, so it's stored by now.
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
        if !checked.is_empty() {
            new += self.store.write_all(&checked)?;
        }
        if !changes.is_empty() {
            self.annotations(|annotations| {
                for &change in &changes {
                    annotations.flag_divergence(change, self.node.principal())?;
                }
                Ok(())
            })?;
        }
        self.wakeup.notify();
        Ok(Stored { new: new as u64 })
    }

    /// The statements about `subject`, in the order the repository received
    /// them.
    ///
    /// # Errors
    ///
    /// Fails if the index or a statement cannot be read.
    pub fn statements(
        &self,
        _caller: &Caller,
        subject: RecordId,
    ) -> Result<Vec<Annotation>, ServiceError> {
        self.annotations(|annotations| Ok(annotations.about(subject)?))
    }

    /// The revisions of `change`, and its heads.
    ///
    /// # Errors
    ///
    /// Fails if the repository has no revision of the change, or cannot
    /// read one.
    pub fn revisions(&self, _caller: &Caller, change: ChangeId) -> Result<Revisions, ServiceError> {
        let (ids, heads) = self.annotations(|annotations| {
            let index = annotations.index();
            Ok((index.revisions(change).to_vec(), index.heads(change)))
        })?;
        if ids.is_empty() {
            return Err(ServiceError::UnknownChange(change));
        }
        let revisions = ids
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
    /// stack is on.
    ///
    /// # Errors
    ///
    /// Fails if the change or a target doesn't exist, a target was given
    /// no target to land on, this node isn't the target's authority, the
    /// change can't land as it is (see [`LandError`]), or the records
    /// cannot be read or written.
    pub fn land(&self, _caller: &Caller, request: &LandRequest) -> Result<Landings, ServiceError> {
        let queue = Queue::new(&self.store, &self.node.key, self.node.name.clone());
        let landed = self.annotations(|annotations| {
            let landed = match &request.proposal {
                Proposal::Change(change) => {
                    let onto = match &request.onto {
                        Some(onto) => onto.clone(),
                        None => self.target_of(annotations.index(), *change)?,
                    };
                    let landings = queue.land_change(annotations, *change, &onto)?;
                    Landings { onto, landings }
                }
                Proposal::Target(source) => {
                    let onto = request.onto.clone().ok_or(ServiceError::NoTarget)?;
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

    /// Every landing that waits, on every target, and why.
    ///
    /// # Errors
    ///
    /// Fails if the queues cannot be read.
    pub fn landings(&self, _caller: &Caller) -> Result<Vec<Pending>, ServiceError> {
        Ok(Queue::pending(&self.store)?)
    }

    /// Creates a target at the tip of another, with this node as its
    /// authority.
    ///
    /// # Errors
    ///
    /// Fails if the name is taken, the other target doesn't exist, the
    /// policy wouldn't make this node the new target's authority, or the
    /// records cannot be written.
    pub fn create_target(
        &self,
        _caller: &Caller,
        request: &NewTarget,
    ) -> Result<Head, ServiceError> {
        let queue = Queue::new(&self.store, &self.node.key, self.node.name.clone());
        let head = queue.create_target(&request.name, &request.from)?;
        self.wakeup.notify();
        Ok(head)
    }

    /// The events after the query's cursor that pass its filter, waiting
    /// up to its `wait`, at most [`OriginService::MAX_WAIT`], for some to
    /// come. Without a cursor, only events from now on come.
    ///
    /// # Errors
    ///
    /// Fails if the op log no longer keeps the events after the cursor, or
    /// it or a record cannot be read.
    pub fn events(&self, _caller: &Caller, query: &EventsQuery) -> Result<Page, ServiceError> {
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
        Ok(events.wait(cursor, &filter, limit, wait.min(Self::MAX_WAIT))?)
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
            Object::Blob(_) | Object::Acceptance(_) => Self::default(),
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
    /// A blob or a tree was fetched as a record.
    #[error("{0} is not a record: fetch snapshots by walking them")]
    NotARecord(ObjectId),
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

impl ServiceError {
    /// What kind of failure it is, for clients to tell apart.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::UnknownRepository(_)
            | Self::UnknownChange(_)
            | Self::UnknownTarget(_)
            | Self::UnknownSnapshot(_) => ErrorCode::NotFound,
            Self::NoTarget
            | Self::NotARecord(_)
            | Self::Malformed { .. }
            | Self::Acceptance(_)
            | Self::Missing { .. } => ErrorCode::BadRequest,
            Self::Statement { source, .. } => match &**source {
                AnnotateError::Store(error) => Self::store_code(error),
                _ => ErrorCode::BadRequest,
            },
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
