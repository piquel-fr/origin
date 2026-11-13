//! The worker's overhead per check, for a check whose command is `true`,
//! on a server over HTTP on localhost: from a `check.requested` event to
//! the statement issued, which lands the change; the check alone, of a
//! revision no landing waits for, to the statement issued; and a check its
//! result already covers. The command's own runtime is measured alone: the
//! overhead is the difference. Run on a real disk: syncs are much of the
//! cost of a put and a landing.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use divan::Bencher;
use origin_ci::runner::{Outcome, Runner};
use origin_ci::state::State;
use origin_ci::worker::Worker;
use origin_shared::api::{At, LandRequest};
use origin_shared::landing::{Landing, Proposal};
use origin_shared::pack::Pack;
use origin_shared::{
    AttachmentId, Base, ChangeId, EncodedObject, EntryKind, EntryName, Kind, NodeKey, Principal,
    Record, RecordId, Revision, RevisionId, TargetName, Tree, TreeEntry,
};
use origin_src::auth::{Credentials, StaticTokens};
use origin_src::http::Api;
use origin_src::remote::{Remote, RemoteUrl};
use origin_src::server::Repositories;
use origin_src::service::{Caller, Node, OriginService};
use tokio::runtime::Runtime;

fn main() {
    divan::main();
}

/// A server whose `main` requires a passing `ci.test` from `user:ci`, and
/// holds the `checks.toml` defining it, and a worker answering it.
struct Hosted {
    _dir: tempfile::TempDir,
    /// The repository, served in-process, to land changes on.
    service: OriginService,
    _runtime: Runtime,
    worker: Worker,
    /// A runner of its own, for checks no landing waits for.
    runner: Runner,
    /// How many changes were landed.
    changes: u64,
}

impl Hosted {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let node = Node {
            name: "hosted".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let policy = format!(
            "[nodes]\nhosted = \"{}\"\n[targets.policy]\nauthority = \"node:hosted\"\n\
             [targets.main]\nauthority = \"node:hosted\"\n\
             require = [{{ kind = \"ci.test\", verdict = \"pass\", issuer = \"user:ci\" }}]\n",
            node.key.public()
        );
        let root = dir.path().join("demo");
        let service = OriginService::create(root.clone(), node.clone(), &policy).unwrap();

        let runtime = Runtime::new().unwrap();
        let tokens: StaticTokens = toml::from_str("\"user:ci\" = \"ci\"").unwrap();
        let mut repositories = Repositories::new(Credentials::new(node.clone(), Box::new(tokens)));
        repositories.insert(
            "demo".parse().unwrap(),
            OriginService::open(root, node.clone()).unwrap(),
        );
        let url = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, Api::router(repositories)).await });
            format!("http://{address}/demo")
        });
        let url = RemoteUrl::parse(&url).unwrap();
        let state = || State::new(&dir.path().join("state"), &url).unwrap();
        let ci: Principal = "user:ci".parse().unwrap();
        let runner = || {
            let remote = Remote::open(&url, node.clone(), Some("ci".to_owned())).unwrap();
            Runner::new(remote, ci.clone(), state()).unwrap()
        };
        let worker = Worker::new(runner(), &state()).unwrap();
        let runner = runner();
        let mut hosted = Self {
            _dir: dir,
            service,
            _runtime: runtime,
            runner,
            worker,
            changes: 0,
        };
        // `main` takes its `checks.toml` from a first change, checked as
        // a tool would.
        let first = RecordId::Revision(hosted.submit());
        let outcomes = hosted.runner.check(first, &At::Record(first), None);
        assert!(matches!(outcomes.unwrap()[..], [Outcome::Ran { .. }]));
        // Takes the cursor.
        hosted.worker.poll(Duration::ZERO).unwrap();
        hosted
    }

    /// Lands a change of `src/lib.rs` on `main`, which waits for its
    /// `ci.test`, returning its revision.
    fn submit(&mut self) -> RevisionId {
        let (change, revision) = self.put();
        let landed = self
            .service
            .land(
                &Caller::Owner,
                &LandRequest {
                    proposal: Proposal::Change(change),
                    onto: None,
                },
            )
            .unwrap();
        assert!(matches!(landed.landings[..], [Landing::Pending(_)]));
        revision
    }

    /// Puts a revision of a new change of `src/lib.rs`, on `main`.
    fn put(&mut self) -> (ChangeId, RevisionId) {
        self.changes += 1;
        let n = self.changes;
        let store = self.service.store();
        let tip = store.head(&TargetName::MAIN).unwrap().unwrap().acceptance;
        let checks =
            EncodedObject::blob(b"[checks.test]\ncommand = \"true\"\ninputs = [\"src\"]\n");
        let lib = EncodedObject::blob(format!("fn f{n}() {{}}\n").as_bytes());
        let file = |blob: &EncodedObject| TreeEntry {
            kind: EntryKind::File,
            id: blob.id(),
        };
        let src: Tree = [(EntryName::new("lib.rs").unwrap(), file(&lib))]
            .into_iter()
            .collect();
        let src = src.encode();
        let root: Tree = [
            (EntryName::new("checks.toml").unwrap(), file(&checks)),
            (
                EntryName::new("src").unwrap(),
                TreeEntry {
                    kind: EntryKind::Tree,
                    id: src.id(),
                },
            ),
        ]
        .into_iter()
        .collect();
        let root = root.encode();
        let description = EncodedObject::attachment(b"");
        let mut id = [0; 16];
        id[..8].copy_from_slice(&n.to_le_bytes());
        let change = ChangeId::from_bytes(id);
        let revision = Revision {
            change,
            predecessors: Vec::new(),
            base: Base::Acceptance(tip),
            deps: Vec::new(),
            snapshot: root.id().into(),
            description: AttachmentId::from(description.id()),
            author: "user:bench".parse().unwrap(),
        }
        .encode();
        let id = revision.id().into();
        let objects = vec![checks, lib, src, root, description, revision];
        self.service
            .put_records(&Caller::Owner, Pack { objects })
            .unwrap();
        (change, id)
    }
}

/// Answering one `check.requested`: reading the event, checking the
/// revision out, promoting, running `true`, issuing the statement, and the
/// landing it lets continue.
#[divan::bench(sample_size = 1, sample_count = 200)]
fn check_requested_to_statement(bencher: Bencher) {
    let hosted = RefCell::new(Hosted::new());
    bencher
        .with_inputs(|| {
            hosted.borrow_mut().submit();
        })
        .bench_local_values(|()| {
            let answers = hosted.borrow_mut().worker.poll(Duration::ZERO).unwrap();
            assert!(matches!(&answers[..], [answer] if answer.result.is_ok()));
        });
}

/// Checking a revision alone: checking it out, promoting, running `true`
/// and issuing the statement.
#[divan::bench(sample_size = 1, sample_count = 200)]
fn check_to_statement(bencher: Bencher) {
    let hosted = RefCell::new(Hosted::new());
    let test = Kind::new("ci.test").unwrap();
    bencher
        .with_inputs(|| hosted.borrow_mut().put().1)
        .bench_local_values(|revision| {
            let hosted = hosted.borrow();
            let revision = RecordId::Revision(revision);
            let outcomes = hosted
                .runner
                .check(revision, &At::Record(revision), Some(&test));
            assert!(matches!(outcomes.unwrap()[..], [Outcome::Ran { .. }]));
        });
}

/// Checking a revision again, which its result covers: the snapshot is
/// kept from the first check, and no checkout is written.
#[divan::bench(sample_size = 1, sample_count = 200)]
fn covered_check(bencher: Bencher) {
    let hosted = RefCell::new(Hosted::new());
    let test = Kind::new("ci.test").unwrap();
    let check = |revision| {
        let revision = RecordId::Revision(revision);
        let hosted = hosted.borrow();
        hosted
            .runner
            .check(revision, &At::Record(revision), Some(&test))
            .unwrap()
    };
    bencher
        .with_inputs(|| {
            let revision = hosted.borrow_mut().put().1;
            assert!(matches!(check(revision)[..], [Outcome::Ran { .. }]));
            revision
        })
        .bench_local_values(|revision| {
            assert!(matches!(check(revision)[..], [Outcome::Covered { .. }]));
        });
}

/// The command alone: `sh -c true`, in a directory.
#[divan::bench(sample_size = 1, sample_count = 200)]
fn command_alone(bencher: Bencher) {
    let dir: PathBuf = tempfile::tempdir().unwrap().keep();
    bencher.bench(|| {
        let status = Command::new("sh")
            .args(["-c", "true"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());
    });
    let _ = std::fs::remove_dir_all(dir);
}
