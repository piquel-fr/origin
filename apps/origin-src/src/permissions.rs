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
//! A statement's kind must be one they annotate, unless it promotes
//! another, which claims nothing new. A revision may only change paths
//! they write, compared with its base, and may only hold content they
//! supply or that a snapshot it refers to holds at the same path (see
//! [`Store::unsupplied`]): so no one reads a file by placing its id where
//! they may read. Landings check what they change on the target, in the
//! [queue](crate::queue).

use std::collections::{HashMap, HashSet};

use origin_platform::Platform;
use origin_shared::access::{Capability, Rights};
use origin_shared::event::EventKind;
use origin_shared::{
    Kind, ObjectId, Principal, RecordId, RepoPath, Revision, RevisionId, Statement, TargetName,
};

use crate::store::{Store, StoreError};

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
        scope: Option<Vec<Capability>>,
    },
}

impl Caller {
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
    rights: &'a Rights,
    seen: HashMap<RecordId, bool>,
}

impl<'a, P: Platform> Reader<'a, P> {
    /// What a caller with `rights` sees of `store`.
    pub fn new(store: &'a Store<P>, rights: &'a Rights) -> Self {
        Self {
            store,
            rights,
            seen: HashMap::new(),
        }
    }

    /// Whether the caller sees the repository: they read some path.
    #[must_use]
    pub fn sees_repository(&self) -> bool {
        self.rights.paths.reads_under(&RepoPath::root())
    }

    /// Whether the caller sees every record.
    fn sees_all(&self) -> bool {
        self.rights.paths.reads_all()
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
        if let Some(&seen) = self.seen.get(&record) {
            return Ok(seen);
        }
        let seen = match record {
            RecordId::Revision(id) => self.sees_revision(id),
            RecordId::Statement(id) => match self.store.get(id) {
                Ok(statement) => self.sees(statement.subject),
                Err(error) => Err(error),
            },
            RecordId::Acceptance(_) | RecordId::Snapshot(_) => Ok(self.sees_repository()),
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
        Ok(changes.is_empty()
            || changes
                .iter()
                .any(|change| self.rights.paths.reads(&change.path)))
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

/// Checks the records an authenticated caller puts, as the module explains.
pub struct Writer<'a, P> {
    store: &'a Store<P>,
    principal: &'a Principal,
    rights: &'a Rights,
    /// The objects the caller supplied with the records.
    supplied: &'a HashSet<ObjectId>,
}

impl<'a, P: Platform> Writer<'a, P> {
    /// Checks for `principal`, with `rights`, who supplied `supplied`.
    pub fn new(
        store: &'a Store<P>,
        principal: &'a Principal,
        rights: &'a Rights,
        supplied: &'a HashSet<ObjectId>,
    ) -> Self {
        Self {
            store,
            principal,
            rights,
            supplied,
        }
    }

    /// Checks a statement put.
    ///
    /// # Errors
    ///
    /// Fails if the caller isn't its issuer, or may not issue its kind.
    pub fn statement(&self, statement: &Statement) -> Result<(), Forbidden> {
        if statement.issuer != *self.principal {
            return Err(Forbidden::NotYours {
                record: "statement",
                by: statement.issuer.clone(),
                caller: self.principal.clone(),
            });
        }
        if statement.promoted_from.is_none() && !self.rights.may_annotate(&statement.kind) {
            return Err(Forbidden::Annotate(statement.kind.clone()));
        }
        Ok(())
    }

    /// Checks a revision put, whose objects are written.
    ///
    /// # Errors
    ///
    /// Fails if the caller isn't its author, it changes a path they may
    /// not write, or holds content they neither supplied nor share with a
    /// snapshot it refers to; or if a tree cannot be read.
    pub fn revision(&self, id: RevisionId, revision: &Revision) -> Result<(), PutError> {
        if revision.author != *self.principal {
            return Err(Forbidden::NotYours {
                record: "revision",
                by: revision.author.clone(),
                caller: self.principal.clone(),
            }
            .into());
        }
        let base = self.store.base_snapshot(revision.base)?;
        if !self.rights.paths.writes_all() {
            for change in self.store.diff(base, revision.snapshot)? {
                if !self.rights.paths.writes(&change.path) {
                    return Err(Forbidden::Write(change.path).into());
                }
            }
        }
        if !self.rights.paths.reads_all() {
            let mut references = vec![base];
            for &predecessor in &revision.predecessors {
                match self.store.get(predecessor) {
                    Ok(predecessor) => references.push(predecessor.snapshot),
                    Err(StoreError::NotFound(_)) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let unsupplied =
                self.store
                    .unsupplied(revision.snapshot, &references, self.supplied)?;
            if let Some(path) = unsupplied {
                return Err(Forbidden::Unsupplied { revision: id, path }.into());
            }
        }
        Ok(())
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
        AttachmentId, Base, ChangeId, EncodedObject, EntryKind, EntryName, NodeKey, Record,
        Revision, Statement, Tree, TreeEntry, TreeId,
    };

    use super::*;
    use crate::queue::tests::Repo;
    use crate::service::{Node, OriginService, ServiceError};
    use crate::tree::Snapshots;
    use crate::tree::tests::build;

    /// Ada writes everything and changes the rules. Eve reads all but
    /// `secrets`, and writes only `src`. Both land on `main`.
    const RULES: &str = r#"
[targets.main]
authority = "node:queue"

[grants]
"user:ada" = ["admin", "land *"]
"user:eve" = ["land *", "annotate review"]

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
            matches!(error, ServiceError::NotAnAttachment { .. }),
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
            matches!(error, ServiceError::NotAnAttachment { .. }),
            "{error}"
        );

        // And had one got into the store, fetching it never brings the file.
        fixture
            .repo
            .store
            .write_all(std::slice::from_ref(&crafted))
            .unwrap();
        let fetched = fixture.service.records(&eve, &[crafted.id()]).unwrap();
        assert_eq!(fetched.objects, [crafted]);
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

    #[test]
    fn statements_are_attributed_to_their_issuer() {
        let fixture = Fixture::new();
        let revision = fixture.repo.change_on_main(2, &[]);
        let statement = |issuer: &str, kind: &str| {
            Statement {
                subject: RecordId::Revision(revision),
                kind: Kind::new(kind).unwrap(),
                verdict: None,
                issuer: issuer.parse().unwrap(),
                inputs: None,
                payload: None,
                supersedes: None,
                promoted_from: None,
            }
            .encode()
        };
        let eve = caller("user:eve");
        assert!(matches!(
            forbidden(fixture.put(&eve, vec![statement("user:ada", "review")])),
            Forbidden::NotYours { .. }
        ));
        fixture
            .put(&eve, vec![statement("user:eve", "review")])
            .unwrap();
        let scoped = Caller::Principal {
            principal: "user:eve".parse().unwrap(),
            scope: Some(vec!["annotate ci.*".parse().unwrap()]),
        };
        assert_eq!(
            forbidden(fixture.put(&scoped, vec![statement("user:eve", "lint")])),
            Forbidden::Annotate(Kind::new("lint").unwrap())
        );
        assert_eq!(
            forbidden(fixture.put(&Caller::Anonymous, vec![statement("user:eve", "x")])),
            Forbidden::Anonymous
        );
    }
}
