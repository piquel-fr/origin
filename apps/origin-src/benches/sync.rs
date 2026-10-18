//! Sync over HTTP on localhost, through the SDK, on a 20k-file tree: the
//! snapshot a clone fetches, fetching a snapshot with 100 files changed
//! since one the client has, and pushing a revision with 100 files edited.
//! And a fetch that brings nothing, from a repository with a long history,
//! with the remote's event cursor as of the last fetch and without, as
//! every fetch was before cursors. Each includes the server's work and the
//! client's durable writes, so run on a real disk.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use divan::Bencher;
use origin_platform::Native;
use origin_shared::api::LandRequest;
use origin_shared::landing::{Landing, Proposal};
use origin_shared::pack::Pack;
use origin_shared::{
    Base, ChangeId, EncodedObject, Kind, NodeKey, Principal, Record, RecordId, Revision, Statement,
    TargetName, TreeId, Verdict,
};
use origin_src::auth::{Credentials, StaticTokens};
use origin_src::fixture::FixtureTree;
use origin_src::http::Api;
use origin_src::policy::Policy;
use origin_src::remote::{Remote, RemoteUrl};
use origin_src::server::Repositories;
use origin_src::service::{Caller, Node, OriginService};
use origin_src::store::Store;
use origin_src::sync::{Cursor, Fetched, Sync};
use origin_src::worktree::{StatCache, Worktree};
use tokio::runtime::Runtime;

fn main() {
    divan::main();
}

const TREE: FixtureTree = FixtureTree::LARGE;

/// The files each edit changes.
const EDITS: usize = 100;

/// The runs of the benchmarks that need a snapshot of their own each.
const SAMPLES: u32 = 10;

/// A server hosting the fixture as `main`'s tip, and snapshots of it with
/// 100 files edited; and a client holding the tip, with snapshots of its
/// own edits.
struct Setup {
    _dirs: [tempfile::TempDir; 3],
    _server: Runtime,
    url: RemoteUrl,
    node: Node,
    /// The tip of `main`, the whole fixture, and its snapshot.
    tip: Base,
    snapshot: TreeId,
    /// Revisions of the server's, each with its own edits.
    theirs: Vec<RecordId>,
    client: Store,
    /// Edits only the client has.
    ours: Vec<TreeId>,
}

static SETUP: LazyLock<Setup> = LazyLock::new(|| {
    let [files, server, client] = [(); 3].map(|()| tempfile::tempdir().unwrap());
    TREE.write(files.path()).unwrap();
    let node = Node {
        name: "hosted".parse().unwrap(),
        key: Arc::new(NodeKey::from_bytes(&[7; 32])),
    };
    let policy = Policy::initial(&node.name, &node.key.public(), None);
    let service = OriginService::create(server.path().join("demo"), node.clone(), &policy).unwrap();
    let mut cache = StatCache::default();
    let mut snapshot_into = |store: &Store, version: Option<u64>| {
        if let Some(version) = version {
            TREE.edit(files.path(), EDITS, version).unwrap();
        }
        let (tree, new) = Worktree::new(files.path(), store).snapshot(&cache).unwrap();
        cache = new.unwrap_or_default();
        tree
    };
    let snapshot = snapshot_into(service.store(), None);
    let tip = land(&service, snapshot);
    let mut records = Vec::new();
    let theirs = (1..=u64::from(SAMPLES))
        .map(|version| {
            let snapshot = snapshot_into(service.store(), Some(version));
            let (description, revision) = revision(tip, snapshot);
            let id = RecordId::Revision(revision.id().into());
            records.extend([description.clone(), revision.clone()]);
            let pack = Pack {
                objects: vec![description, revision],
            };
            service.put_records(&Caller::Owner, pack).unwrap();
            id
        })
        .collect();

    let (runtime, url) = serve(service, &node);

    let store = Store::new(client.path().join(".origin"), Native::default());
    store.init().unwrap();
    let remote = Remote::open(&url, node.clone(), Some("bench".to_owned())).unwrap();
    let sync = Sync::new(&store, &remote, "laptop".parse().unwrap(), bench_user());
    sync.fetch_chains(&[TargetName::MAIN]).unwrap();
    sync.need(tip.into(), &[]).unwrap();
    // The client knows the records, as a fetch would have brought them.
    store.write_all(&records).unwrap();
    // Versions the server's edits never reach.
    let ours = (1..=u64::from(SAMPLES))
        .map(|version| snapshot_into(&store, Some(1_000 + version)))
        .collect();
    Setup {
        _dirs: [files, server, client],
        _server: runtime,
        url,
        node,
        tip,
        snapshot,
        theirs,
        client: store,
        ours,
    }
});

/// Serves `service` over HTTP on localhost as `demo`, where `user:bench`'s
/// token is `bench`, on a runtime of its own.
fn serve(service: OriginService, node: &Node) -> (Runtime, RemoteUrl) {
    let runtime = Runtime::new().unwrap();
    let tokens: StaticTokens = toml::from_str("\"user:bench\" = \"bench\"").unwrap();
    let mut repositories = Repositories::new(Credentials::new(node.clone(), Box::new(tokens)));
    repositories.insert("demo".parse().unwrap(), service);
    let base = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, Api::router(repositories)).await });
        format!("http://{address}")
    });
    let url = RemoteUrl::Http {
        base,
        repo: "demo".parse().unwrap(),
    };
    (runtime, url)
}

/// Lands a change with `snapshot` on `main`, returning the acceptance.
fn land(service: &OriginService, snapshot: TreeId) -> Base {
    let store = service.store();
    let tip = store.head(&TargetName::MAIN).unwrap().unwrap().acceptance;
    let (description, revision) = revision(Base::Acceptance(tip), snapshot);
    let change = revision.decode_as::<Revision>().unwrap().change;
    let pack = Pack {
        objects: vec![description, revision],
    };
    service.put_records(&Caller::Owner, pack).unwrap();
    let request = LandRequest {
        proposal: Proposal::Change(change),
        onto: None,
    };
    let landed = service.land(&Caller::Owner, &request).unwrap();
    let [Landing::Landed { acceptance, .. }] = landed.landings[..] else {
        panic!("{landed:?}")
    };
    Base::Acceptance(acceptance.acceptance)
}

/// A revision of a new change with `snapshot`, on `base`, and its
/// description.
fn revision(base: Base, snapshot: TreeId) -> (EncodedObject, EncodedObject) {
    let description = EncodedObject::attachment(b"bench");
    let revision = Revision {
        change: ChangeId::random().unwrap(),
        predecessors: Vec::new(),
        base,
        deps: Vec::new(),
        snapshot,
        description: description.id().into(),
        author: "user:bench".parse().unwrap(),
    }
    .encode();
    (description, revision)
}

impl Setup {
    fn remote(&self) -> Remote {
        Remote::open(&self.url, self.node.clone(), Some("bench".to_owned())).unwrap()
    }
}

/// The user the benchmarks push as.
fn bench_user() -> Principal {
    "user:bench".parse().unwrap()
}

/// The snapshot a clone fetches: every file, into an empty repository.
#[divan::bench(sample_count = 10)]
fn clone_snapshot(bencher: Bencher<'_, '_>) {
    let setup = &*SETUP;
    let remote = setup.remote();
    bencher
        .with_inputs(|| {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path().join(".origin"), Native::default());
            store.init().unwrap();
            // A clone fetches the chain first.
            let Base::Acceptance(tip) = setup.tip else {
                unreachable!("the tip is an acceptance")
            };
            store
                .write_all(&[setup.client.read(tip.into()).unwrap()])
                .unwrap();
            (dir, store)
        })
        .bench_local_refs(|(_, store)| {
            let sync = Sync::new(store, &remote, "laptop".parse().unwrap(), bench_user());
            assert!(sync.need(setup.tip.into(), &[]).unwrap());
        });
}

/// Fetching a snapshot with 100 files changed since the one the client
/// has.
#[divan::bench(sample_count = SAMPLES)]
fn fetch_100_edited(bencher: Bencher<'_, '_>) {
    let setup = &*SETUP;
    let remote = setup.remote();
    let sync = Sync::new(
        &setup.client,
        &remote,
        "laptop".parse().unwrap(),
        bench_user(),
    );
    let next = AtomicUsize::new(0);
    bencher
        .with_inputs(|| setup.theirs[next.fetch_add(1, Ordering::Relaxed) % SAMPLES as usize])
        .bench_local_values(|revision| {
            sync.need(revision, &[setup.snapshot]).unwrap();
        });
}

/// Pushing a new change with 100 files edited on `main`'s tip: its
/// revision, and the trees and blobs it changed.
#[divan::bench(sample_count = SAMPLES)]
fn push_100_edited(bencher: Bencher<'_, '_>) {
    let setup = &*SETUP;
    let remote = setup.remote();
    let sync = Sync::new(
        &setup.client,
        &remote,
        "laptop".parse().unwrap(),
        bench_user(),
    );
    let next = AtomicUsize::new(0);
    bencher
        .with_inputs(|| {
            let snapshot = setup.ours[next.fetch_add(1, Ordering::Relaxed) % SAMPLES as usize];
            let (description, revision) = revision(setup.tip, snapshot);
            let change = revision.decode_as::<Revision>().unwrap().change;
            setup.client.write_all(&[description, revision]).unwrap();
            change
        })
        .bench_local_values(|change| {
            let pushed = sync.push(&[change]).unwrap();
            assert_eq!(pushed.revisions, 1);
        });
}

/// The changes of the long history a fetch that brings nothing runs on.
const HISTORY: u32 = 10_000;

/// A server whose repository has [`HISTORY`] changes, each with a revision
/// and a statement, and a client that fetched them all, with the remote's
/// event cursor as of then.
struct History {
    _dirs: [tempfile::TempDir; 2],
    _server: Runtime,
    client: Store,
    remote: Remote,
    cursor: Cursor,
}

static HISTORY_SETUP: LazyLock<History> = LazyLock::new(|| {
    let [server, client] = [(); 2].map(|()| tempfile::tempdir().unwrap());
    let node = Node {
        name: "hosted".parse().unwrap(),
        key: Arc::new(NodeKey::from_bytes(&[7; 32])),
    };
    let policy = Policy::initial(&node.name, &node.key.public(), None);
    let service = OriginService::create(server.path().join("demo"), node.clone(), &policy).unwrap();
    let tip = service.store().head(&TargetName::MAIN).unwrap().unwrap();
    let state = service.store().get(tip.acceptance).unwrap().body().state;
    let description = EncodedObject::attachment(b"bench");
    let mut objects = vec![description.clone()];
    let mut changes = Vec::new();
    for change in 0..HISTORY {
        let mut bytes = [0; 16];
        bytes[..4].copy_from_slice(&change.to_le_bytes());
        let change = ChangeId::from_bytes(bytes);
        let revision = Revision {
            change,
            predecessors: Vec::new(),
            base: Base::Acceptance(tip.acceptance),
            deps: Vec::new(),
            snapshot: state,
            description: description.id().into(),
            author: bench_user(),
        }
        .encode();
        let statement = Statement {
            subject: RecordId::Revision(revision.id().into()),
            kind: Kind::new("ci.test").unwrap(),
            verdict: Some(Verdict::Pass),
            issuer: bench_user(),
            inputs: None,
            payload: None,
            supersedes: None,
            promoted_from: None,
        }
        .encode();
        objects.extend([revision, statement]);
        changes.push(change);
    }
    service
        .put_records(&Caller::Owner, Pack { objects })
        .unwrap();
    let (runtime, url) = serve(service, &node);

    let store = Store::new(client.path().join(".origin"), Native::default());
    store.init().unwrap();
    let remote = Remote::open(&url, node, Some("bench".to_owned())).unwrap();
    let sync = Sync::new(&store, &remote, "laptop".parse().unwrap(), bench_user());
    let fetch = sync.fetch(&[TargetName::MAIN], &changes, None).unwrap();
    assert_eq!(fetch.fetched.revisions, HISTORY as usize);
    let cursor = fetch.cursor;
    History {
        _dirs: [server, client],
        _server: runtime,
        client: store,
        remote,
        cursor,
    }
});

/// A fetch that brings nothing, from a remote with a long history, with
/// the remote's event cursor as of the last fetch: it costs the same
/// however long the history.
#[divan::bench(sample_count = 20)]
fn fetch_unchanged(bencher: Bencher<'_, '_>) {
    let history = &*HISTORY_SETUP;
    let sync = Sync::new(
        &history.client,
        &history.remote,
        "laptop".parse().unwrap(),
        bench_user(),
    );
    bencher.bench_local(|| {
        let fetch = sync.fetch(&[], &[], Some(&history.cursor)).unwrap();
        assert_eq!(fetch.fetched, Fetched::default());
    });
}

/// The same fetch without a cursor, as when it expired, and as every
/// fetch was before cursors: it compares every change.
#[divan::bench(sample_count = 10)]
fn fetch_unchanged_without_cursor(bencher: Bencher<'_, '_>) {
    let history = &*HISTORY_SETUP;
    let sync = Sync::new(
        &history.client,
        &history.remote,
        "laptop".parse().unwrap(),
        bench_user(),
    );
    bencher.bench_local(|| {
        let fetch = sync.fetch(&[], &[], None).unwrap();
        assert_eq!(fetch.fetched, Fetched::default());
    });
}
