//! Enforcing what callers may do, as their [`Rights`] say: what they read
//! of a repository's records, and what the records they put may hold.
//!
//! **Reading.** A caller who reads no path doesn't see the repository at
//! all. Otherwise they see its targets and acceptances, every revision that
//! changes a path they read, or changes nothing, and the statements about
//! what they see. Snapshots withhold the content of the paths they don't
//! read (see [`Store::added`]).
//!
//! **Writing.** Anonymous callers only read. A record put is attributed to
//! its caller: a revision's author and a statement's issuer must be them.
//! A statement's kind must be one they annotate, a promotion's too, but
//! for the reserved kinds, whose authors the [policy](crate::policy::Policy::authorizes)
//! names for every caller; and they must see what it's about, and for a promotion, its original: so no
//! one copies a result they can't read.
//!
//! A revision must extend its change's history: only a new change gets a
//! root, and a successor evolves only from revisions of its change, which
//! they see. So no one makes a change diverge with a root of their own,
//! or becomes one of its creators, who [abandon](crate::annotations::RevisionView::authorizes) it.
//! Where they may not write, a revision has an *effect*: each file it
//! changes there, from its base's entry to its own. A root has none. A
//! successor has exactly each predecessor's, on the same change below if
//! it's on one, and lists exactly their unresolved conflicts there in its
//! `core.conflict` (see [`conflict`](crate::conflict)), which only a
//! `core.conflict` supersedes; a root lists those of the revision it's on.
//! So whatever base it picks, a successor never reverts, replaces or
//! resolves someone else's edits where its writer may not, nor adds a
//! conflict there, nor brings a stack that would land with it; a rebase
//! onto a newer tip keeps the effect, and one who takes a change over edits
//! what they write and hands it on. A revision may only hold
//! content they supply or that a snapshot it refers to holds at the same
//! path (see [`Store::unsupplied`]): so no one reads a file by placing its
//! id where they may read. The records of a batch are read before the store's (see
//! [`Batch`]), so a batch may hold a stack of revisions and statements
//! about them. Landings check what they change on the target, in the
//! [queue](crate::queue).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::iter;

use origin_platform::Platform;
use origin_shared::access::{Capability, PathAccess, Rights};
use origin_shared::event::EventKind;
use origin_shared::{
    Base, ChangeId, Kind, Object, ObjectId, Principal, RecordId, RepoPath, Revision, RevisionId,
    Statement, StatementId, TargetName, TreeId,
};

use crate::annotations::{Annotations, Claim, RevisionView};
use crate::conflict::Conflicts;
use crate::store::{Store, StoreError};
use crate::tree::FileChange;

/// Who calls the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The node's own user, through the command line or a remote on a
    /// local path: they may do everything.
    Owner,
    /// A client of a server that didn't authenticate.
    Anonymous,
    /// An authenticated client of a server.
    Principal {
        /// Who they are.
        principal: Principal,
        /// What their token narrows their rights to, if it's a scoped one.
        scope: Option<Scope>,
    },
}

/// What a scoped token narrows its bearer's rights to, and until when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// The capabilities it keeps of its bearer's rights.
    pub capabilities: Vec<Capability>,
    /// When it expires, in seconds since the Unix epoch: a token issued
    /// with it expires no later.
    pub expires: u64,
}

impl Caller {
    /// When the caller's token expires, in seconds since the Unix epoch, if
    /// it's a scoped one.
    #[must_use]
    pub fn expires(&self) -> Option<u64> {
        match self {
            Self::Principal {
                scope: Some(scope), ..
            } => Some(scope.expires),
            Self::Principal { scope: None, .. } | Self::Owner | Self::Anonymous => None,
        }
    }

    /// Who the caller is, unless the owner or anonymous.
    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        match self {
            Self::Principal { principal, .. } => Some(principal),
            Self::Owner | Self::Anonymous => None,
        }
    }
}

/// What a caller sees of a repository's records, remembering what it
/// decided.
pub struct Reader<'a, P> {
    store: &'a Store<P>,
    /// What the caller may do with each path.
    paths: &'a PathAccess,
    seen: HashMap<RecordId, bool>,
}

impl<'a, P: Platform> Reader<'a, P> {
    /// What a caller who may do `paths` sees of `store`.
    pub fn new(store: &'a Store<P>, paths: &'a PathAccess) -> Self {
        Self {
            store,
            paths,
            seen: HashMap::new(),
        }
    }

    /// Whether the caller sees the repository: they read some path.
    #[must_use]
    pub fn sees_repository(&self) -> bool {
        self.paths.reads_under(&RepoPath::root())
    }

    /// Whether the caller sees every record.
    fn sees_all(&self) -> bool {
        self.paths.reads_all()
    }

    /// Whether the caller sees `record`, as the module explains. A record
    /// this node lacks, or whose base it lacks, is not seen.
    ///
    /// # Errors
    ///
    /// Fails if a record or tree cannot be read.
    pub fn sees(&mut self, record: RecordId) -> Result<bool, StoreError> {
        if self.sees_all() {
            return Ok(true);
        }
        if !self.sees_repository() {
            return Ok(false);
        }
        if let Some(&seen) = self.seen.get(&record) {
            return Ok(seen);
        }
        let seen = match record {
            RecordId::Revision(id) => self.sees_revision(id),
            RecordId::Statement(id) => match self.store.get(id) {
                Ok(statement) => self.sees(statement.subject),
                Err(error) => Err(error),
            },
            RecordId::Acceptance(_) | RecordId::Snapshot(_) => Ok(true),
        };
        let seen = match seen {
            Err(StoreError::NotFound(_)) => false,
            seen => seen?,
        };
        self.seen.insert(record, seen);
        Ok(seen)
    }

    fn sees_revision(&self, id: RevisionId) -> Result<bool, StoreError> {
        let revision: Revision = self.store.get(id)?;
        let changes = self
            .store
            .diff(self.store.base_snapshot(revision.base)?, revision.snapshot)?;
        Ok(changes.is_empty() || changes.iter().any(|change| self.paths.reads(&change.path)))
    }

    /// Whether the caller sees what `event` is about.
    ///
    /// # Errors
    ///
    /// Fails like [`Reader::sees`].
    pub fn sees_event(&mut self, event: &EventKind) -> Result<bool, StoreError> {
        match event {
            EventKind::CheckRequested { subject, .. } => self.sees(*subject),
            event => self.sees(event.record()),
        }
    }
}

/// The records a batch puts, read before those of the store it's put in,
/// so a record may refer to another of the same batch.
pub struct Batch<'a, P> {
    store: &'a Store<P>,
    /// Every object of the batch.
    supplied: HashSet<ObjectId>,
    revisions: HashMap<RevisionId, &'a Revision>,
    /// How many revisions of each change evolve from none.
    roots: HashMap<ChangeId, usize>,
    statements: HashMap<StatementId, &'a Statement>,
    /// The statements about each record, in batch order.
    about: HashMap<RecordId, Vec<&'a Statement>>,
}

impl<'a, P: Platform> Batch<'a, P> {
    /// The batch of `supplied`, whose records `decoded` are, put in `store`.
    pub fn new(
        store: &'a Store<P>,
        supplied: HashSet<ObjectId>,
        decoded: &'a [(ObjectId, Object)],
    ) -> Self {
        let mut batch = Self {
            store,
            supplied,
            revisions: HashMap::new(),
            roots: HashMap::new(),
            statements: HashMap::new(),
            about: HashMap::new(),
        };
        for (id, object) in decoded {
            match object {
                Object::Revision(revision) => {
                    batch.revisions.insert((*id).into(), revision);
                    if revision.predecessors.is_empty() {
                        *batch.roots.entry(revision.change).or_default() += 1;
                    }
                }
                Object::Statement(statement) => {
                    batch.statements.insert((*id).into(), statement);
                    batch
                        .about
                        .entry(statement.subject)
                        .or_default()
                        .push(statement);
                }
                _ => {}
            }
        }
        batch
    }

    /// Whether the batch holds `record`.
    fn holds(&self, record: RecordId) -> bool {
        self.supplied.contains(&record.into())
    }

    /// The statement `id`.
    fn statement(&self, id: StatementId) -> Result<Statement, StoreError> {
        match self.statements.get(&id) {
            Some(&statement) => Ok(statement.clone()),
            None => self.store.get(id),
        }
    }
}

impl<P: Platform> RevisionView for Batch<'_, P> {
    fn revision(&self, id: RevisionId) -> Result<Revision, StoreError> {
        match self.revisions.get(&id) {
            Some(&revision) => Ok(revision.clone()),
            None => self.store.get(id),
        }
    }
}

/// Checks the records of a batch an authenticated caller puts, as the
/// module explains.
pub struct Writer<'a, P> {
    batch: &'a Batch<'a, P>,
    /// The statements and revisions of the store, as its index knows them.
    annotations: &'a Annotations<'a, P>,
    principal: &'a Principal,
    rights: &'a Rights,
    reader: Reader<'a, P>,
    effects: Effects<'a, P, Batch<'a, P>>,
}

impl<'a, P: Platform> Writer<'a, P> {
    /// Checks `batch` for `principal`, with `rights`, finding what the
    /// store has through `annotations`.
    #[must_use]
    pub fn new(
        batch: &'a Batch<'a, P>,
        annotations: &'a Annotations<'a, P>,
        principal: &'a Principal,
        rights: &'a Rights,
    ) -> Self {
        Self {
            batch,
            annotations,
            principal,
            rights,
            reader: Reader::new(batch.store, &rights.paths),
            effects: Effects::new(batch.store, batch, &rights.paths),
        }
    }

    /// Whether the caller sees `record`: the batch's records are theirs.
    fn sees(&mut self, record: RecordId) -> Result<bool, StoreError> {
        Ok(self.batch.holds(record) || self.reader.sees(record)?)
    }

    /// Whether the caller may write every path, so no effect is checked.
    fn writes_all(&self) -> bool {
        self.rights.paths.writes_all()
    }

    /// Checks a statement put.
    ///
    /// # Errors
    ///
    /// Fails if the caller isn't its issuer, may not issue its kind, or
    /// doesn't see what it's about or the original it promotes; if it
    /// replaces a `core.conflict` about a revision, by being one or
    /// superseding one, that doesn't list exactly the conflicts the
    /// revision carries where they may not write; or if a record cannot be
    /// read.
    pub fn statement(&mut self, statement: &Statement) -> Result<(), PutError> {
        if statement.issuer != *self.principal {
            return Err(Forbidden::NotYours {
                record: "statement",
                by: statement.issuer.clone(),
                caller: self.principal.clone(),
            }
            .into());
        }
        // Reserved kinds have authors of their own, which the service
        // checks for every caller.
        if !statement.kind.is_reserved() && !self.rights.may_annotate(&statement.kind) {
            return Err(Forbidden::Annotate(statement.kind.clone()).into());
        }
        let original = statement.promoted_from.map(RecordId::Statement);
        for record in iter::once(statement.subject).chain(original) {
            if !self.sees(record)? {
                return Err(PutError::Unseen(record));
            }
        }
        if !self.writes_all() {
            self.keeps_listing(statement)?;
        }
        Ok(())
    }

    /// Checks that `statement`, if it replaces its issuer's `core.conflict`
    /// about a revision, by being one or superseding one, lists exactly the
    /// conflicts the revision carries where the caller may not write.
    fn keeps_listing(&self, statement: &Statement) -> Result<(), PutError> {
        let superseded = match statement.supersedes {
            Some(id) => match self.batch.statement(id) {
                Ok(superseded) => Some(superseded),
                Err(StoreError::NotFound(_)) => None,
                Err(error) => return Err(error.into()),
            },
            None => None,
        };
        let replaced = iter::once(statement)
            .chain(&superseded)
            .filter(|replaced| replaced.kind == Kind::CONFLICT);
        for replaced in replaced {
            let RecordId::Revision(id) = replaced.subject else {
                continue;
            };
            let revision = self.batch.revision(id)?;
            let listed =
                if statement.kind == Kind::CONFLICT && statement.subject == replaced.subject {
                    Conflicts::latest(self.batch.store, [statement], &revision)?
                } else {
                    Conflicts::default()
                };
            self.keeps_conflicts(&revision, &listed)?;
        }
        Ok(())
    }

    /// Checks a revision put, whose objects are written.
    ///
    /// # Errors
    ///
    /// Fails if the caller isn't its author; it adds a root to a change
    /// that has revisions, or evolves from a revision of another change;
    /// they don't see a revision it evolves from; it has an effect they may
    /// not have; or it holds content they neither supplied nor share with
    /// a snapshot it refers to; or if a tree cannot be read.
    pub fn revision(&mut self, id: RevisionId, revision: &Revision) -> Result<(), PutError> {
        if revision.author != *self.principal {
            return Err(Forbidden::NotYours {
                record: "revision",
                by: revision.author.clone(),
                caller: self.principal.clone(),
            }
            .into());
        }
        let roots = self.batch.roots.get(&revision.change).copied();
        let stored = !self
            .annotations
            .index()
            .revisions(revision.change)
            .is_empty();
        if revision.predecessors.is_empty() && (stored || roots > Some(1)) {
            return Err(Forbidden::History(revision.change).into());
        }
        let mut references = vec![self.effects.base_snapshot(revision.base)?];
        for &id in &revision.predecessors {
            if !self.sees(RecordId::Revision(id))? {
                return Err(PutError::Unseen(RecordId::Revision(id)));
            }
            let predecessor = self.batch.revision(id)?;
            if predecessor.change != revision.change {
                return Err(Forbidden::History(revision.change).into());
            }
            references.push(predecessor.snapshot);
        }
        if let Some(forbidden) = self.effects.check(revision)? {
            return Err(forbidden.into());
        }
        if !self.writes_all() {
            let listed = self.conflicts(id, revision)?;
            self.keeps_conflicts(revision, &listed)?;
        }
        if !self.rights.paths.reads_all() {
            let store = self.batch.store;
            let unsupplied =
                store.unsupplied(revision.snapshot, &references, &self.batch.supplied)?;
            if let Some(path) = unsupplied {
                return Err(Forbidden::Unsupplied { revision: id, path }.into());
            }
        }
        Ok(())
    }

    /// The unresolved conflicts of `revision`, `id`, as its author's latest
    /// `core.conflict` about it lists them, the store's then the batch's.
    fn conflicts(&self, id: RevisionId, revision: &Revision) -> Result<Conflicts, StoreError> {
        let subject = RecordId::Revision(id);
        let stored = self.annotations.claims(subject)?;
        let batched = self
            .batch
            .about
            .get(&subject)
            .into_iter()
            .flatten()
            .copied();
        let statements = stored.iter().map(Claim::statement).chain(batched);
        Conflicts::latest(self.batch.store, statements, revision)
    }

    /// Checks that `listed`, the unresolved conflicts of `revision`, are
    /// exactly those it carries where the caller may not write: its
    /// predecessors', or its base revision's for a root, where `revision`
    /// still has the entry written. Only a writer of a path resolves its
    /// conflict, or adds one.
    fn keeps_conflicts(&self, revision: &Revision, listed: &Conflicts) -> Result<(), PutError> {
        let from = match (&revision.predecessors[..], revision.base) {
            ([], Base::Revision(base)) => vec![base],
            (predecessors, _) => predecessors.to_vec(),
        };
        let unwritable = |path: &RepoPath| !self.rights.paths.writes(path);
        let mut carried = BTreeSet::new();
        for id in from {
            let from = self.batch.revision(id)?;
            let kept = self
                .conflicts(id, &from)?
                .only(unwritable)
                .remaining(self.batch.store, revision.snapshot)?;
            carried.extend(kept.paths());
        }
        let listed: BTreeSet<RepoPath> = listed
            .clone()
            .only(unwritable)
            .paths()
            .into_iter()
            .collect();
        if let Some(path) = carried.difference(&listed).next() {
            return Err(Forbidden::Resolve(path.clone()).into());
        }
        if let Some(path) = listed.difference(&carried).next() {
            return Err(Forbidden::Conflict(path.clone()).into());
        }
        Ok(())
    }
}

/// What revisions do where their writer may not write, as the module
/// explains: the service checks it of every revision a signed-in caller
/// puts, and a client of those it pushes, before sending anything.
pub struct Effects<'a, P, V> {
    store: &'a Store<P>,
    /// Where revisions are read: a batch's, then the store's.
    revisions: &'a V,
    /// What the writer may do with each path.
    paths: &'a PathAccess,
}

impl<'a, P: Platform, V: RevisionView> Effects<'a, P, V> {
    /// The effects of revisions of `store`, read from `revisions`, for a
    /// writer who may do `paths`.
    pub fn new(store: &'a Store<P>, revisions: &'a V, paths: &'a PathAccess) -> Self {
        Self {
            store,
            revisions,
            paths,
        }
    }

    /// The snapshot of `base`.
    fn base_snapshot(&self, base: Base) -> Result<TreeId, StoreError> {
        match base {
            Base::Revision(id) => Ok(self.revisions.revision(id)?.snapshot),
            Base::Acceptance(_) => self.store.base_snapshot(base),
        }
    }

    /// The effect of `revision`: each file it changes where the writer
    /// may not write, from its base's entry to its own, sorted by path.
    fn effect(&self, revision: &Revision) -> Result<Vec<FileChange>, StoreError> {
        let base = self.base_snapshot(revision.base)?;
        let mut changes = self.store.diff(base, revision.snapshot)?;
        changes.retain(|change| !self.paths.writes(&change.path));
        Ok(changes)
    }

    /// What the writer may not do of what `revision` does, if anything: a
    /// root may have no effect, and a successor has exactly each
    /// predecessor's, on the same change below, if it's on one.
    ///
    /// # Errors
    ///
    /// Fails if a revision or tree cannot be read.
    pub fn check(&self, revision: &Revision) -> Result<Option<Forbidden>, StoreError> {
        if self.paths.writes_all() {
            return Ok(None);
        }
        let ours = self.effect(revision)?;
        if revision.predecessors.is_empty() {
            return Ok(ours
                .first()
                .map(|change| Forbidden::Write(change.path.clone())));
        }
        let below = match revision.base {
            Base::Revision(below) => Some(self.revisions.revision(below)?.change),
            Base::Acceptance(_) => None,
        };
        for &id in &revision.predecessors {
            let predecessor = self.revisions.revision(id)?;
            if let Some(below) = below {
                let theirs = match predecessor.base {
                    Base::Revision(theirs) => Some(self.revisions.revision(theirs)?.change),
                    Base::Acceptance(_) => None,
                };
                if theirs != Some(below) {
                    return Ok(Some(Forbidden::Below(below)));
                }
            }
            let theirs = self.effect(&predecessor)?;
            // Both are sorted by path: the first that differs is a path one
            // lacks, or changes otherwise.
            let differs = ours
                .iter()
                .zip(&theirs)
                .find(|(ours, theirs)| ours != theirs)
                .map(|(ours, theirs)| ours.path.clone().min(theirs.path.clone()))
                .or_else(|| {
                    let longer = if ours.len() > theirs.len() {
                        &ours
                    } else {
                        &theirs
                    };
                    let path = longer.get(ours.len().min(theirs.len()));
                    path.map(|change| change.path.clone())
                });
            if let Some(path) = differs {
                return Ok(Some(Forbidden::Write(path)));
            }
        }
        Ok(None)
    }
}

/// Something a caller may not do.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Forbidden {
    /// An anonymous caller tried to write.
    #[error("anonymous callers only read: sign in with a token")]
    Anonymous,
    /// The owner has no principal to issue a token for.
    #[error("only a signed-in caller gets a token, for themselves")]
    NoPrincipal,
    /// A record put is attributed to someone else.
    #[error("the {record} is by {by}, but you are {caller}")]
    NotYours {
        /// The kind of record.
        record: &'static str,
        /// Who it names.
        by: Principal,
        /// The caller.
        caller: Principal,
    },
    /// A statement of a kind the caller may not issue.
    #[error("you may not issue `{0}` statements")]
    Annotate(Kind),
    /// A change to a path the caller may not write.
    #[error("you may not change `{0}`")]
    Write(RepoPath),
    /// A conflict at a path the caller may not write, which a revision or
    /// statement of theirs would resolve.
    #[error("you may not resolve the conflict at `{0}`")]
    Resolve(RepoPath),
    /// A conflict at a path the caller may not write, which a revision or
    /// statement of theirs would list though none it evolves from does.
    #[error("you may not add a conflict at `{0}`")]
    Conflict(RepoPath),
    /// A revision that would add a root to a change with revisions, or
    /// evolve from a revision of another change.
    #[error(
        "a new revision of change {} must evolve from its revisions, and only from them",
        .0.short()
    )]
    History(ChangeId),
    /// A successor moved onto a change it wasn't on, which would then land
    /// with it.
    #[error("you may not move a change you took over onto change {}", .0.short())]
    Below(ChangeId),
    /// A path the caller may not read.
    #[error("you may not read `{0}`")]
    Read(RepoPath),
    /// A revision holds content its writer didn't supply, at a path where
    /// no snapshot it refers to has it.
    #[error(
        "revision {} holds content at `{path}` that it neither brings nor shares with a \
         snapshot it refers to",
        revision.short()
    )]
    Unsupplied {
        /// The revision.
        revision: RevisionId,
        /// The path.
        path: RepoPath,
    },
    /// A target the caller may not land on.
    #[error("you may not land on `{0}`")]
    Land(TargetName),
    /// A token's scope reaches beyond its issuer's rights.
    #[error("a token may only narrow your rights, which don't include `{0}`")]
    Scope(Capability),
}

/// A failure to check a record put.
#[derive(Debug, thiserror::Error)]
pub enum PutError {
    /// The caller may not put it.
    #[error(transparent)]
    Forbidden(#[from] Forbidden),
    /// A statement is about a record the caller doesn't see, or promotes
    /// one: to them, it doesn't exist.
    #[error("record {0} not found")]
    Unseen(RecordId),
    /// A record or tree cannot be read.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use origin_platform::Native;
    use origin_shared::api::{FetchSnapshot, LandRequest};
    use origin_shared::landing::{Landing, Proposal};
    use origin_shared::pack::Pack;
    use origin_shared::{
        AttachmentId, Base, ChangeId, EncodedObject, EntryKind, EntryName, NodeKey, ObjectKind,
        Record, Revision, Statement, Tree, TreeEntry, TreeId,
    };

    use super::*;
    use crate::conflict::Conflicts;
    use crate::queue::tests::Repo;
    use crate::service::{Node, OriginService, ServiceError};
    use crate::tree::Snapshots;
    use crate::tree::tests::build;

    /// Ada writes everything and changes the rules. Eve reads all but
    /// `secrets`, writes only `src`, and issues reviews and checks. Both
    /// land on `main`.
    const RULES: &str = r#"
[targets.main]
authority = "node:queue"

[grants]
"user:ada" = ["admin", "land *"]
"user:eve" = ["land *", "annotate review", "annotate ci.*"]

[[paths]]
path = "/"
grants = { all = "read", "user:ada" = "write" }

[[paths]]
path = "src"
grants = { "user:eve" = "write" }

[[paths]]
path = "secrets"
grants = { all = "none", "user:ada" = "write" }
"#;

    /// The repository, with `main` holding a README, a library, a secret
    /// and a protected file, and its service.
    struct Fixture {
        repo: Repo,
        service: OriginService,
    }

    impl Fixture {
        fn new() -> Self {
            let repo = Repo::new(RULES);
            repo.change_on_main(
                1,
                &[
                    ("README.md", "hello"),
                    ("src/lib.rs", "fn a() {}"),
                    ("secrets/key", "hunter2"),
                    ("protected/file", "keep"),
                ],
            );
            repo.land(1);
            let node = Node {
                name: "queue".parse().unwrap(),
                key: Arc::new(NodeKey::from_bytes(&[7; 32])),
            };
            let service = OriginService::new(repo.store.clone(), node);
            Self { repo, service }
        }

        fn tip(&self, target: &TargetName) -> Base {
            Base::Acceptance(self.repo.tip(target).acceptance)
        }

        fn state(&self, target: &TargetName) -> TreeId {
            self.repo.store.base_snapshot(self.tip(target)).unwrap()
        }

        /// The objects of a snapshot of `files`, and its id, without
        /// writing them.
        fn objects(files: &[(&str, &str)]) -> (TreeId, Vec<EncodedObject>) {
            let dir = tempfile::tempdir().unwrap();
            let scratch = Store::new(dir.path().join(".origin"), Native::default());
            scratch.init().unwrap();
            let blobs: Vec<_> = files
                .iter()
                .map(|(_, content)| EncodedObject::blob(content.as_bytes()))
                .collect();
            scratch.write_all(&blobs).unwrap();
            let tree = build(&scratch, files);
            (tree, scratch.read_all(&scratch.ids().unwrap()).unwrap())
        }

        /// A revision of `change` by `author` with `snapshot` on `base`, and
        /// its description.
        fn revision(
            change: u8,
            author: &str,
            base: Base,
            predecessors: Vec<RevisionId>,
            snapshot: TreeId,
        ) -> (RevisionId, Vec<EncodedObject>) {
            let description = EncodedObject::attachment(b"work");
            let revision = Revision {
                change: ChangeId::from_bytes([change; 16]),
                predecessors,
                base,
                deps: Vec::new(),
                snapshot,
                description: description.id().into(),
                author: author.parse().unwrap(),
            }
            .encode();
            (revision.id().into(), vec![description, revision])
        }

        fn put(&self, caller: &Caller, objects: Vec<EncodedObject>) -> Result<(), ServiceError> {
            self.service
                .put_records(caller, Pack { objects })
                .map(|_| ())
        }

        fn land(&self, caller: &Caller, proposal: Proposal) -> Result<Vec<Landing>, ServiceError> {
            let request = LandRequest {
                proposal,
                onto: Some(TargetName::MAIN),
            };
            Ok(self.service.land(caller, &request)?.landings)
        }

        /// What `main` holds at `path`.
        fn at_main(&self, path: &str) -> Option<TreeEntry> {
            Snapshots::new(&self.repo.store)
                .entry_at(self.state(&TargetName::MAIN), &RepoPath::new(path).unwrap())
                .unwrap()
        }
    }

    fn caller(principal: &str) -> Caller {
        Caller::Principal {
            principal: principal.parse().unwrap(),
            scope: None,
        }
    }

    fn forbidden(result: Result<impl std::fmt::Debug, ServiceError>) -> Forbidden {
        match result {
            Err(
                ServiceError::Forbidden(forbidden)
                | ServiceError::Land(crate::queue::LandError::Forbidden(forbidden)),
            ) => forbidden,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn records_never_carry_a_file_as_their_attachment() {
        let fixture = Fixture::new();
        let eve = caller("user:eve");
        let secret = fixture.at_main("secrets/key").unwrap().id;

        // A revision naming the secret's blob as its description is refused:
        // descriptions are attachments, which no file is.
        let base = fixture.tip(&TargetName::MAIN);
        let crafted = Revision {
            change: ChangeId::from_bytes([2; 16]),
            predecessors: Vec::new(),
            base,
            deps: Vec::new(),
            snapshot: fixture.state(&TargetName::MAIN),
            description: AttachmentId::from(secret),
            author: "user:eve".parse().unwrap(),
        }
        .encode();
        let error = fixture.put(&eve, vec![crafted.clone()]).unwrap_err();
        assert!(
            matches!(
                error,
                ServiceError::WrongKind {
                    kind: ObjectKind::Attachment,
                    ..
                }
            ),
            "{error}"
        );
        let statement = Statement {
            subject: RecordId::Revision(fixture.repo.change_on_main(3, &[])),
            kind: Kind::new("review").unwrap(),
            verdict: None,
            issuer: "user:eve".parse().unwrap(),
            inputs: None,
            payload: Some(AttachmentId::from(secret)),
            supersedes: None,
            promoted_from: None,
        }
        .encode();
        let error = fixture.put(&eve, vec![statement]).unwrap_err();
        assert!(
            matches!(
                error,
                ServiceError::WrongKind {
                    kind: ObjectKind::Attachment,
                    ..
                }
            ),
            "{error}"
        );

        // And had one got into the store, fetching it never brings the file.
        fixture
            .repo
            .store
            .write_all(std::slice::from_ref(&crafted))
            .unwrap();
        let fetched = fixture.service.records(&eve, &[crafted.id()]).unwrap();
        assert_eq!(fetched.pack.objects, [crafted]);
    }

    #[test]
    fn pushes_cannot_place_unreadable_objects_where_they_read() {
        let fixture = Fixture::new();
        let eve = caller("user:eve");
        let base = fixture.tip(&TargetName::MAIN);
        let state = fixture.state(&TargetName::MAIN);
        let secret = fixture.at_main("secrets/key").unwrap();
        let store = &fixture.repo.store;

        // `main`, with the secret's blob placed in `src`, which eve writes.
        let mut src: Tree = store
            .get(fixture.at_main("src").unwrap().subtree().unwrap())
            .unwrap();
        src.insert(EntryName::new("stolen").unwrap(), secret);
        let src = src.encode();
        let mut root: Tree = store.get(state).unwrap();
        let entry = TreeEntry {
            kind: EntryKind::Tree,
            id: src.id(),
        };
        root.insert(EntryName::new("src").unwrap(), entry);
        let root = root.encode();
        let (id, records) = Fixture::revision(2, "user:eve", base, Vec::new(), root.id().into());
        let mut objects = vec![src, root];
        objects.extend(records);
        let error = forbidden(fixture.put(&eve, objects));
        assert_eq!(
            error,
            Forbidden::Unsupplied {
                revision: id,
                path: path("src/stolen"),
            }
        );
        assert!(!store.contains(id.into()).unwrap());

        // Nor walk a tree within a snapshot as if it were one.
        let request = FetchSnapshot {
            of: RecordId::Snapshot(fixture.at_main("secrets").unwrap().subtree().unwrap()),
            have: Vec::new(),
            paths: Vec::new(),
            skip: 0,
        };
        assert!(matches!(
            fixture.service.snapshot(&eve, &request),
            Err(ServiceError::NotASnapshot(_))
        ));

        // Content eve brings herself is hers to place.
        let mut files = vec![("README.md", "hello"), ("src/lib.rs", "fn a() {}")];
        files.push(("src/own.rs", "hunter2"));
        let (tree, mut objects) = Fixture::objects(&files);
        // The paths eve can't read stay as they are.
        let (tree, trees) = {
            let mut root: Tree = objects
                .iter()
                .find(|object| object.id() == tree.into())
                .unwrap()
                .decode_as()
                .unwrap();
            for kept in ["secrets", "protected"] {
                root.insert(
                    EntryName::new(kept).unwrap(),
                    fixture.at_main(kept).unwrap(),
                );
            }
            let root = root.encode();
            (TreeId::from(root.id()), vec![root])
        };
        objects.extend(trees);
        let (_, records) = Fixture::revision(3, "user:eve", base, Vec::new(), tree);
        objects.extend(records);
        fixture.put(&eve, objects).unwrap();
    }

    #[test]
    fn landings_never_change_what_the_submitter_cannot_write() {
        let fixture = Fixture::new();
        let (ada, eve) = (caller("user:ada"), caller("user:eve"));
        let first = fixture.tip(&TargetName::MAIN);
        let old_state = fixture.state(&TargetName::MAIN);
        let repo = &fixture.repo;
        repo.change_on_main(2, &[("protected/file", "v2")]);
        repo.land(2);

        // A revision made on an old acceptance doesn't revert what changed
        // since: only its own changes land.
        let mut files = vec![
            ("README.md", "hello"),
            ("src/lib.rs", "fn a() {}"),
            ("secrets/key", "hunter2"),
            ("protected/file", "keep"),
        ];
        files.push(("src/new.rs", "fn b() {}"));
        let (old_base, _) = (repo.revision(3, &[], first, &files), old_state);
        let landed = fixture
            .land(&eve, Proposal::Change(ChangeId::from_bytes([3; 16])))
            .unwrap();
        assert!(
            matches!(landed[..], [Landing::Landed { .. }]),
            "{old_base} {landed:?}"
        );
        let v2 = EncodedObject::blob(b"v2").id();
        assert_eq!(fixture.at_main("protected/file").unwrap().id, v2);

        // A revision someone else made that deletes the file: eve can't
        // land it, ada can.
        files.retain(|(path, _)| *path != "protected/file");
        repo.revision(4, &[], fixture.tip(&TargetName::MAIN), &files);
        let change = Proposal::Change(ChangeId::from_bytes([4; 16]));
        assert_eq!(
            forbidden(fixture.land(&eve, change.clone())),
            Forbidden::Write(path("protected/file"))
        );
        assert!(fixture.at_main("protected/file").is_some());

        // Nor smuggle the deletion in through another target.
        let side = TargetName::new("side").unwrap();
        repo.queue()
            .create_target(&side, &TargetName::MAIN)
            .unwrap();
        repo.queue()
            .land_change(
                &mut repo.annotations(),
                ChangeId::from_bytes([4; 16]),
                &side,
            )
            .unwrap();
        let from_side = Proposal::Target(side);
        assert_eq!(
            forbidden(fixture.land(&eve, from_side.clone())),
            Forbidden::Write(path("protected/file"))
        );
        let landed = fixture.land(&ada, from_side).unwrap();
        assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");
        assert!(fixture.at_main("protected/file").is_none());
    }

    #[test]
    fn converging_cannot_smuggle_others_changes() {
        let fixture = Fixture::new();
        let eve = caller("user:eve");
        let repo = &fixture.repo;
        let base = fixture.tip(&TargetName::MAIN);
        let files = [
            ("README.md", "hello"),
            ("src/lib.rs", "fn a() {}"),
            ("secrets/key", "hunter2"),
        ];
        let first = repo.revision(
            5,
            &[],
            base,
            &[&files[..], &[("protected/file", "keep")]].concat(),
        );
        // Ada's head edits the protected file, eve's head `src`.
        let ada_head = repo.revision(
            5,
            &[first],
            base,
            &[&files[..], &[("protected/file", "ada")]].concat(),
        );
        let mut eves = files.to_vec();
        eves[1] = ("src/lib.rs", "fn eve() {}");
        eves.push(("protected/file", "keep"));
        let eve_head = repo.revision(5, &[first], base, &eves);

        // Eve converges them, merging ada's edit in.
        let merged = repo
            .store
            .merge(
                Some(repo.store.get(first).unwrap().snapshot),
                repo.store.get(eve_head).unwrap().snapshot,
                repo.store.get(ada_head).unwrap().snapshot,
            )
            .unwrap();
        let (id, records) =
            Fixture::revision(5, "user:eve", base, vec![ada_head, eve_head], merged.tree);
        let mut objects = merged.trees.clone();
        objects.extend(records.clone());
        assert_eq!(
            forbidden(fixture.put(&eve, objects)),
            Forbidden::Write(path("protected/file"))
        );

        // Had someone else written it, eve still couldn't land it.
        repo.store.write_all(&merged.trees).unwrap();
        repo.store.write_all(&records).unwrap();
        assert_eq!(
            repo.annotations()
                .index()
                .heads(ChangeId::from_bytes([5; 16])),
            [id]
        );
        assert_eq!(
            forbidden(fixture.land(&eve, Proposal::Change(ChangeId::from_bytes([5; 16])))),
            Forbidden::Write(path("protected/file"))
        );
    }

    /// `main`'s files, with `edits`.
    fn main_with(edits: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let main = [
            ("README.md", "hello"),
            ("src/lib.rs", "fn a() {}"),
            ("secrets/key", "hunter2"),
            ("protected/file", "keep"),
        ];
        main.iter()
            .map(|&(at, content)| {
                let edit = edits.iter().find(|(edited, _)| *edited == at);
                edit.copied().unwrap_or((at, content))
            })
            .collect()
    }

    #[test]
    fn successors_have_their_predecessors_effect_where_their_writer_cannot_write() {
        let fixture = Fixture::new();
        let (eve, repo) = (caller("user:eve"), &fixture.repo);
        let base = fixture.tip(&TargetName::MAIN);
        let succeed = |predecessors: Vec<RevisionId>, base, files: &[(&str, &str)]| {
            let (tree, mut objects) = Fixture::objects(files);
            let (id, records) = Fixture::revision(30, "user:eve", base, predecessors, tree);
            objects.extend(records);
            fixture.put(&eve, objects).map(|()| id)
        };

        // A successor of ada's change of the README and the library never
        // undoes the README, which eve doesn't write.
        let edits = [("README.md", "ada"), ("src/lib.rs", "fn ada() {}")];
        let adas = repo.revision(30, &[], base, &main_with(&edits));
        let undone = main_with(&[("src/lib.rs", "fn ada() {}")]);
        assert_eq!(
            forbidden(succeed(vec![adas], base, &undone)),
            Forbidden::Write(path("README.md"))
        );

        // Eve takes the change over, keeping the README as ada changed it,
        // and changes the library, which she writes.
        let edits = [("README.md", "ada"), ("src/lib.rs", "fn eve() {}")];
        let taken = succeed(vec![adas], base, &main_with(&edits)).unwrap();

        // Rebased onto a tip where the protected file moved, the successor
        // differs from its predecessor there, as their bases do: that's
        // fine.
        repo.change_on_main(31, &[("protected/file", "newer")]);
        repo.land(31);
        let tip = fixture.tip(&TargetName::MAIN);
        let rebased = [&edits[..], &[("protected/file", "newer")]].concat();
        let rebased = succeed(vec![taken], tip, &main_with(&rebased)).unwrap();

        // Two successors with that effect converge into one.
        let edits = [("README.md", "ada"), ("src/lib.rs", "fn other() {}")];
        let other = succeed(vec![taken], base, &main_with(&edits)).unwrap();
        let edits = [("README.md", "ada"), ("src/lib.rs", "fn both() {}")];
        succeed(vec![rebased, other], base, &main_with(&edits)).unwrap();
        let landed = fixture
            .land(
                &caller("user:ada"),
                Proposal::Change(ChangeId::from_bytes([30; 16])),
            )
            .unwrap();
        assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");
        assert_eq!(
            fixture.at_main("README.md").unwrap().id,
            EncodedObject::blob(b"ada").id()
        );
    }

    /// The probes of the third review: a successor that keeps ada's files,
    /// but on a base that already has her edits, so it changes nothing.
    #[test]
    fn no_base_makes_a_successor_drop_its_predecessors_effect() {
        let fixture = Fixture::new();
        let repo = &fixture.repo;
        let reader = scoped("user:eve", &["read /"], in_a_week());
        let base = fixture.tip(&TargetName::MAIN);
        let state = fixture.state(&TargetName::MAIN);
        let edits = [("README.md", "ada"), ("src/lib.rs", "fn ada() {}")];
        let adas = repo.revision(40, &[], base, &main_with(&edits));
        let snapshot = repo.store.get(adas).unwrap().snapshot;
        let succeed = |base, predecessors| {
            let (id, records) = Fixture::revision(40, "user:eve", base, predecessors, snapshot);
            fixture.put(&reader, records).map(|()| id)
        };

        // On ada's head itself.
        assert_eq!(
            forbidden(succeed(Base::Revision(adas), vec![adas])),
            Forbidden::Below(ChangeId::from_bytes([40; 16]))
        );
        // On a revision of another change, of eve's, on ada's head.
        let (on_adas, records) =
            Fixture::revision(41, "user:eve", Base::Revision(adas), Vec::new(), snapshot);
        fixture.put(&reader, records).unwrap();
        assert_eq!(
            forbidden(succeed(Base::Revision(on_adas), vec![adas])),
            Forbidden::Below(ChangeId::from_bytes([41; 16]))
        );
        // On a tip where another change landed the same files.
        repo.change_on_main(42, &edits);
        repo.land(42);
        let tip = fixture.tip(&TargetName::MAIN);
        assert_eq!(
            forbidden(succeed(tip, vec![adas])),
            Forbidden::Write(path("README.md"))
        );

        // Taking the head over as it is is fine, but chains nothing back:
        // the effect stays ada's.
        let taken = succeed(base, vec![adas]).unwrap();
        let (_, records) = Fixture::revision(40, "user:eve", base, vec![taken], state);
        assert_eq!(
            forbidden(fixture.put(&reader, records)),
            Forbidden::Write(path("README.md"))
        );
    }

    #[test]
    fn changes_keep_their_history() {
        let fixture = Fixture::new();
        let repo = &fixture.repo;
        let reader = scoped("user:eve", &["read /"], in_a_week());
        let base = fixture.tip(&TargetName::MAIN);
        let state = fixture.state(&TargetName::MAIN);
        let change = ChangeId::from_bytes([43; 16]);
        let adas = repo.revision(43, &[], base, &main_with(&[("README.md", "ada")]));
        let snapshot = repo.store.get(adas).unwrap().snapshot;

        // No second root, which would make the change diverge, and eve
        // one of its creators.
        let (_, records) = Fixture::revision(43, "user:eve", base, Vec::new(), state);
        assert_eq!(
            forbidden(fixture.put(&reader, records)),
            Forbidden::History(change)
        );
        // Nor a successor evolving from a root of another change.
        let (eves, records) = Fixture::revision(44, "user:eve", base, Vec::new(), state);
        fixture.put(&reader, records).unwrap();
        let (_, records) = Fixture::revision(43, "user:eve", base, vec![adas, eves], snapshot);
        assert_eq!(
            forbidden(fixture.put(&reader, records)),
            Forbidden::History(change)
        );
        // Nor two roots of a new change in one batch.
        let (_, mut records) = Fixture::revision(45, "user:ada", base, Vec::new(), state);
        records.extend(Fixture::revision(45, "user:ada", base, Vec::new(), snapshot).1);
        assert_eq!(
            forbidden(fixture.put(&caller("user:ada"), records)),
            Forbidden::History(ChangeId::from_bytes([45; 16]))
        );

        // So ada still created hers, and abandons it.
        let abandon = statement(RecordId::Revision(adas), "core.abandon", "user:ada").encode();
        fixture.put(&caller("user:ada"), vec![abandon]).unwrap();
        let landed = fixture.land(&caller("user:ada"), Proposal::Change(change));
        assert!(
            matches!(
                landed,
                Err(ServiceError::Land(crate::queue::LandError::Abandoned(_)))
            ),
            "{landed:?}"
        );
    }

    #[test]
    fn successors_resolve_no_conflict_their_writer_cannot_write() {
        let fixture = Fixture::new();
        let (eve, repo) = (caller("user:eve"), &fixture.repo);
        let base = fixture.tip(&TargetName::MAIN);
        // Ada's revision holds a conflict in the README, and lists it.
        let marked = main_with(&[("README.md", "<<<<<<< ours")]);
        let adas = repo.revision(46, &[], base, &marked);
        let snapshot = repo.store.get(adas).unwrap().snapshot;
        let readme = Snapshots::new(&repo.store)
            .entry_at(snapshot, &path("README.md"))
            .unwrap();
        let conflicts = Conflicts::new(vec![crate::conflict::Conflicted {
            path: path("README.md"),
            written: readme.map(|entry| entry.id),
        }]);
        let listing = |subject, issuer: &str, supersedes| {
            let (payload, statement) = conflicts.statement(
                RecordId::Revision(subject),
                issuer.parse().unwrap(),
                supersedes,
            );
            (payload, statement.encode())
        };
        let (payload, adas_listing) = listing(adas, "user:ada", None);
        repo.store.write_all(&[payload, adas_listing]).unwrap();

        // Eve takes it over as it is: without the conflict, she'd resolve it.
        let (taken, records) = Fixture::revision(46, "user:eve", base, vec![adas], snapshot);
        assert_eq!(
            forbidden(fixture.put(&eve, records.clone())),
            Forbidden::Resolve(path("README.md"))
        );
        let (payload, kept) = listing(taken, "user:eve", None);
        let kept_id = StatementId::from(kept.id());
        fixture
            .put(&eve, [vec![payload, kept], records].concat())
            .unwrap();

        // Nor does she supersede that list with one without it, or with
        // anything else.
        let (payload, empty) = Conflicts::default().statement(
            RecordId::Revision(taken),
            "user:eve".parse().unwrap(),
            Some(kept_id),
        );
        assert_eq!(
            forbidden(fixture.put(&eve, vec![payload, empty.encode()])),
            Forbidden::Resolve(path("README.md"))
        );
        let review = Statement {
            supersedes: Some(kept_id),
            ..statement(RecordId::Revision(taken), "review", "user:eve")
        };
        assert_eq!(
            forbidden(fixture.put(&eve, vec![review.encode()])),
            Forbidden::Resolve(path("README.md"))
        );
        let landed = fixture.land(
            &caller("user:ada"),
            Proposal::Change(ChangeId::from_bytes([46; 16])),
        );
        assert!(
            matches!(
                landed,
                Err(ServiceError::Land(
                    crate::queue::LandError::Unresolved { .. }
                ))
            ),
            "{landed:?}"
        );
    }

    #[test]
    fn revisions_add_no_conflict_their_writer_cannot_write() {
        let fixture = Fixture::new();
        let (eve, base) = (caller("user:eve"), fixture.tip(&TargetName::MAIN));
        let mallory = scoped("user:mallory", &["read /"], in_a_week());
        let change = ChangeId::from_bytes([47; 16]);
        let (tree, mut objects) = Fixture::objects(&main_with(&[("src/lib.rs", "fn eve() {}")]));
        let (eves, records) = Fixture::revision(47, "user:eve", base, Vec::new(), tree);
        objects.extend(records);
        fixture.put(&eve, objects).unwrap();
        let listing = |subject, issuer: &str| {
            let readme = fixture.at_main("README.md").map(|entry| entry.id);
            let conflicts = Conflicts::new(vec![crate::conflict::Conflicted {
                path: path("README.md"),
                written: readme,
            }]);
            let (payload, statement) =
                conflicts.statement(RecordId::Revision(subject), issuer.parse().unwrap(), None);
            vec![payload, statement.encode()]
        };
        // Mallory, who only reads, takes eve's change over as it is, and
        // lists a conflict at the README, which eve doesn't write: then
        // eve could never land it, nor resolve it.
        let (mallorys, mut records) = Fixture::revision(47, "user:mallory", base, vec![eves], tree);
        records.extend(listing(mallorys, "user:mallory"));
        assert_eq!(
            forbidden(fixture.put(&mallory, records)),
            Forbidden::Conflict(path("README.md"))
        );
        // Nor does a root list one its base doesn't have.
        let (root, mut records) = Fixture::revision(48, "user:eve", base, Vec::new(), tree);
        records.extend(listing(root, "user:eve"));
        assert_eq!(
            forbidden(fixture.put(&eve, records)),
            Forbidden::Conflict(path("README.md"))
        );
        let landed = fixture.land(&eve, Proposal::Change(change)).unwrap();
        assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");

        // A root on a revision carries its conflicts.
        let marked = main_with(&[("README.md", "<<<<<<< ours")]);
        let adas = fixture.repo.revision(49, &[], base, &marked);
        let snapshot = fixture.repo.store.get(adas).unwrap().snapshot;
        let readme = Snapshots::new(&fixture.repo.store)
            .entry_at(snapshot, &path("README.md"))
            .unwrap();
        let conflicts = Conflicts::new(vec![crate::conflict::Conflicted {
            path: path("README.md"),
            written: readme.map(|entry| entry.id),
        }]);
        let carry = |subject, issuer: &str| {
            let (payload, statement) =
                conflicts.statement(RecordId::Revision(subject), issuer.parse().unwrap(), None);
            vec![payload, statement.encode()]
        };
        fixture
            .repo
            .store
            .write_all(&carry(adas, "user:ada"))
            .unwrap();
        let on_adas = Base::Revision(adas);
        let (root, records) = Fixture::revision(50, "user:eve", on_adas, Vec::new(), snapshot);
        assert_eq!(
            forbidden(fixture.put(&eve, records.clone())),
            Forbidden::Resolve(path("README.md"))
        );
        fixture
            .put(&eve, [records, carry(root, "user:eve")].concat())
            .unwrap();
    }

    #[test]
    fn only_those_who_see_a_revision_succeed_it() {
        let fixture = Fixture::new();
        let adas = fixture
            .repo
            .change_on_main(30, &[("secrets/key", "rotated")]);
        let base = fixture.tip(&TargetName::MAIN);
        // Eve, who doesn't read `secrets`, makes the head of ada's change
        // her own, changing nothing, to abandon it.
        let state = fixture.state(&TargetName::MAIN);
        let (eves, mut objects) = Fixture::revision(30, "user:eve", base, vec![adas], state);
        objects.push(statement(RecordId::Revision(eves), "core.abandon", "user:eve").encode());
        let blind = scoped("user:eve", &[], in_a_week());
        assert!(fixture.put(&blind, objects.clone()).is_err());
        let put = fixture.put(&caller("user:eve"), objects);
        assert!(
            matches!(put, Err(ServiceError::UnknownRecord(record)) if record == RecordId::Revision(adas)),
            "{put:?}"
        );
        let change = Proposal::Change(ChangeId::from_bytes([30; 16]));
        let landed = fixture.land(&caller("user:ada"), change).unwrap();
        assert!(matches!(landed[..], [Landing::Landed { .. }]), "{landed:?}");
    }

    #[test]
    fn tokens_narrowed_out_of_admin_abandon_nothing() {
        let fixture = Fixture::new();
        let base = fixture.tip(&TargetName::MAIN);
        let state = fixture.state(&TargetName::MAIN);
        let (eves, records) = Fixture::revision(31, "user:eve", base, Vec::new(), state);
        fixture.put(&caller("user:eve"), records).unwrap();
        let abandon = statement(RecordId::Revision(eves), "core.abandon", "user:ada").encode();
        let reader = scoped("user:ada", &["read /"], in_a_week());
        let refused = fixture.put(&reader, vec![abandon.clone()]).unwrap_err();
        assert_eq!(refused.code(), origin_shared::api::ErrorCode::Forbidden);
        let admin = scoped("user:ada", &["admin", "read /"], in_a_week());
        fixture.put(&admin, vec![abandon]).unwrap();
    }

    /// A statement by `issuer` of `kind` about `subject`.
    fn statement(subject: RecordId, kind: &str, issuer: &str) -> Statement {
        Statement {
            subject,
            kind: Kind::new(kind).unwrap(),
            verdict: None,
            issuer: issuer.parse().unwrap(),
            inputs: None,
            payload: None,
            supersedes: None,
            promoted_from: None,
        }
    }

    /// A passing result of `kind` by ada about `subject`, which reads only
    /// the README, so every revision of `main` shares its input digest.
    fn readme_result(subject: RecordId, kind: &str) -> Statement {
        Statement {
            verdict: Some(origin_shared::Verdict::Pass),
            inputs: Some(origin_shared::Inputs {
                paths: vec![path("README.md")],
                definition: origin_shared::Digest::of(b"test"),
                toolchain: origin_shared::Digest::of(b""),
                external: false,
            }),
            ..statement(subject, kind, "user:ada")
        }
    }

    /// `caller`, with a token narrowing them to `scope`, expiring at
    /// `expires`.
    fn scoped(principal: &str, scope: &[&str], expires: u64) -> Caller {
        Caller::Principal {
            principal: principal.parse().unwrap(),
            scope: Some(Scope {
                capabilities: scope.iter().map(|c| c.parse().unwrap()).collect(),
                expires,
            }),
        }
    }

    /// A week from now, in seconds since the Unix epoch.
    fn in_a_week() -> u64 {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        (now.unwrap() + OriginService::MAX_TOKEN_TTL).as_secs()
    }

    #[test]
    fn statements_are_attributed_to_their_issuer() {
        let fixture = Fixture::new();
        let revision = RecordId::Revision(fixture.repo.change_on_main(2, &[]));
        let statement = |issuer: &str, kind: &str| statement(revision, kind, issuer).encode();
        let eve = caller("user:eve");
        assert!(matches!(
            forbidden(fixture.put(&eve, vec![statement("user:ada", "review")])),
            Forbidden::NotYours { .. }
        ));
        fixture
            .put(&eve, vec![statement("user:eve", "review")])
            .unwrap();
        let scoped = scoped("user:eve", &["annotate ci.*"], in_a_week());
        assert_eq!(
            forbidden(fixture.put(&scoped, vec![statement("user:eve", "lint")])),
            Forbidden::Annotate(Kind::new("lint").unwrap())
        );
        assert_eq!(
            forbidden(fixture.put(&Caller::Anonymous, vec![statement("user:eve", "x")])),
            Forbidden::Anonymous
        );
    }

    #[test]
    fn statements_are_about_what_their_issuer_sees_and_annotates() {
        let fixture = Fixture::new();
        let repo = &fixture.repo;
        let hidden = RecordId::Revision(repo.change_on_main(2, &[("secrets/key", "new")]));
        let visible = RecordId::Revision(repo.change_on_main(3, &[("src/lib.rs", "fn b() {}")]));
        let eve = caller("user:eve");
        let unseen = |result: Result<(), ServiceError>| match result {
            Err(ServiceError::UnknownRecord(record)) => record,
            other => panic!("expected an unknown record, got {other:?}"),
        };
        assert_eq!(
            unseen(fixture.put(&eve, vec![statement(hidden, "review", "user:eve").encode()])),
            hidden
        );

        // Ada's results about both.
        let result = |subject| readme_result(subject, "ci.test").encode();
        let (of_hidden, of_visible) = (result(hidden), result(visible));
        repo.store
            .write_all(&[of_hidden.clone(), of_visible.clone()])
            .unwrap();
        let mine = RecordId::Revision(repo.change_on_main(4, &[("src/lib.rs", "fn c() {}")]));
        let promotion = |original: &EncodedObject| {
            Statement {
                subject: mine,
                issuer: "user:eve".parse().unwrap(),
                promoted_from: Some(original.id().into()),
                ..original.decode_as().unwrap()
            }
            .encode()
        };

        // Promoting claims nothing new, but still needs the kind.
        let reviewer = scoped("user:eve", &["annotate review"], in_a_week());
        assert_eq!(
            forbidden(fixture.put(&reviewer, vec![promotion(&of_visible)])),
            Forbidden::Annotate(Kind::new("ci.test").unwrap())
        );
        // And copies no result eve can't read.
        assert_eq!(
            unseen(fixture.put(&eve, vec![promotion(&of_hidden)])),
            RecordId::Statement(of_hidden.id().into())
        );
        fixture.put(&eve, vec![promotion(&of_visible)]).unwrap();
    }

    #[test]
    fn promoting_copies_nothing_the_caller_cannot_see() {
        let fixture = Fixture::new();
        let repo = &fixture.repo;
        let hidden = RecordId::Revision(repo.change_on_main(2, &[("secrets/key", "new")]));
        let visible = RecordId::Revision(repo.change_on_main(3, &[("src/lib.rs", "fn b() {}")]));
        // Ada's review of the hidden change, with a payload eve can't read.
        let payload = EncodedObject::attachment(b"a review of the new secret");
        let review = Statement {
            payload: Some(payload.id().into()),
            ..readme_result(hidden, "review")
        };
        repo.store.write_all(&[payload, review.encode()]).unwrap();
        let promote = |who: &str| fixture.service.promote(&caller(who), visible).unwrap();
        assert_eq!(promote("user:eve").len(), 0);
        let [promoted] = &promote("user:ada")[..] else {
            panic!("ada sees the review")
        };
        assert_eq!(promoted.statement.payload, review.payload);
    }

    #[test]
    fn reserved_kinds_come_from_their_authors_whatever_the_caller_annotates() {
        let fixture = Fixture::new();
        let theirs = RecordId::Revision(fixture.repo.change_on_main(2, &[]));
        let base = fixture.tip(&TargetName::MAIN);
        let (mine, records) = Fixture::revision(
            3,
            "user:eve",
            base,
            Vec::new(),
            fixture.state(&TargetName::MAIN),
        );
        let abandon = |subject| statement(subject, "core.abandon", "user:eve").encode();
        let eve = caller("user:eve");
        let error = fixture.put(&eve, vec![abandon(theirs)]).unwrap_err();
        assert!(
            matches!(&error, ServiceError::Statement { source, .. }
                if matches!(**source, crate::annotations::AnnotateError::Reserved { .. })),
            "{error}"
        );
        assert_eq!(error.code(), origin_shared::api::ErrorCode::Forbidden);
        // Eve abandons her own change, put with it, though she annotates
        // no `core.*`.
        let mut batch = records;
        batch.push(abandon(RecordId::Revision(mine)));
        fixture.put(&eve, batch).unwrap();
    }

    #[test]
    fn batches_hold_stacks_and_statements_about_them() {
        let fixture = Fixture::new();
        let main = fixture.tip(&TargetName::MAIN);
        let files = [
            ("README.md", "hello"),
            ("src/lib.rs", "fn a() {}"),
            ("secrets/key", "hunter2"),
            ("protected/file", "keep"),
        ];
        let (lower_tree, mut objects) =
            Fixture::objects(&[&files[..], &[("src/a.rs", "a")]].concat());
        let (lower, records) = Fixture::revision(2, "user:eve", main, Vec::new(), lower_tree);
        objects.extend(records);
        let (upper_tree, more) =
            Fixture::objects(&[&files[..], &[("src/a.rs", "a"), ("src/b.rs", "b")]].concat());
        objects.extend(more);
        let (upper, records) =
            Fixture::revision(3, "user:eve", Base::Revision(lower), Vec::new(), upper_tree);
        objects.extend(records);
        objects.push(statement(RecordId::Revision(upper), "review", "user:eve").encode());
        fixture.put(&caller("user:eve"), objects).unwrap();
    }

    #[test]
    fn callers_who_read_nothing_see_nothing() {
        let fixture = Fixture::new();
        let revision = fixture.repo.change_on_main(2, &[]);
        let blind = scoped("user:eve", &["annotate review"], in_a_week());
        let service = &fixture.service;
        assert!(!service.sees_repository(&blind).unwrap());
        let records = service.records(&blind, &[revision.into()]).unwrap();
        assert!(
            records.pack.objects.is_empty(),
            "not even a revision changing nothing"
        );
        assert_eq!(service.targets(&blind).unwrap(), []);
        let log = origin_shared::api::LogQuery {
            target: TargetName::MAIN,
            since: 0,
            limit: None,
        };
        assert!(matches!(
            service.log(&blind, &log),
            Err(ServiceError::UnknownTarget(_))
        ));
        assert_eq!(service.log(&caller("user:eve"), &log).unwrap().len(), 2);
    }

    #[test]
    fn tokens_issued_from_a_token_expire_with_it() {
        let fixture = Fixture::new();
        let expires = in_a_week() - 6 * 24 * 3600;
        let parent = scoped("user:eve", &["annotate review", "land *"], expires);
        let request = origin_shared::api::TokenRequest {
            scope: vec!["annotate review".parse().unwrap()],
            ttl: origin_shared::Period(OriginService::MAX_TOKEN_TTL),
        };
        let issued = fixture.service.issue_token(&parent, &request).unwrap();
        assert_eq!(issued.expires, expires);
        let issued = fixture
            .service
            .issue_token(&caller("user:eve"), &request)
            .unwrap();
        assert!(issued.expires > expires, "a static token's last a week");
    }
}
