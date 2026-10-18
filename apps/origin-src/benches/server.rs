//! The server: request latency over HTTP on localhost, through the SDK, for
//! a batch of records put and fetched and for an event long-poll woken by a
//! write; and a landing through a service that keeps its index open, against
//! one that reads it per landing. Run on a real disk: syncs are most of the
//! cost of a put.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use divan::Bencher;
use divan::counter::ItemsCount;
use origin_client::Repo;
use origin_shared::api::{EventsQuery, FetchRecords, LandRequest, List};
use origin_shared::landing::{Landing, Proposal};
use origin_shared::pack::Pack;
use origin_shared::{
    AttachmentId, Base, ChangeId, EncodedObject, EntryKind, EntryName, Kind, NodeKey, Period,
    Record, RecordId, Revision, RevisionId, Statement, TargetName, Tree, TreeEntry, Verdict,
};
use origin_src::auth::{Credentials, StaticTokens};
use origin_src::http::Api;
use origin_src::policy::Policy;
use origin_src::server::Repositories;
use origin_src::service::{Caller, Node, OriginService};
use origin_src::store::Store;
use tokio::runtime::Runtime;

fn main() {
    divan::main();
}

/// A repository with one revision on `main`, which statements are about.
struct Hosted {
    dir: tempfile::TempDir,
    node: Node,
    revision: RevisionId,
}

impl Hosted {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let node = Node {
            name: "hosted".parse().unwrap(),
            key: Arc::new(NodeKey::from_bytes(&[7; 32])),
        };
        let policy = Policy::initial(&node.name, &node.key.public());
        let service =
            OriginService::create(dir.path().join("demo"), node.clone(), &policy).unwrap();
        let revision = Self::put_change(&service, 0);
        Self {
            dir,
            node,
            revision,
        }
    }

    fn service(&self) -> OriginService {
        OriginService::open(self.dir.path().join("demo"), self.node.clone()).unwrap()
    }

    /// Puts a change on the tip of `main`, with one file, `file-<n>`.
    fn put_change(service: &OriginService, n: u64) -> RevisionId {
        let store = service.store();
        let tip = store.head(&TargetName::MAIN).unwrap().unwrap().acceptance;
        let mut tree: Tree = store.get(store.get(tip).unwrap().body().state).unwrap();
        let content = EncodedObject::blob(format!("fn f{n}() {{}}\n").as_bytes());
        tree.insert(
            EntryName::new(format!("file-{n}")).unwrap(),
            TreeEntry {
                kind: EntryKind::File,
                id: content.id(),
            },
        );
        let tree = tree.encode();
        let description = EncodedObject::attachment(b"");
        let mut change = [0; 16];
        change[..8].copy_from_slice(&n.to_le_bytes());
        let revision = Revision {
            change: ChangeId::from_bytes(change),
            predecessors: Vec::new(),
            base: Base::Acceptance(tip),
            deps: Vec::new(),
            snapshot: tree.id().into(),
            description: AttachmentId::from(description.id()),
            author: "user:bench".parse().unwrap(),
        }
        .encode();
        let id = revision.id().into();
        let pack = Pack {
            objects: vec![content, tree, description, revision],
        };
        service.put_records(&Caller::Owner, pack).unwrap();
        id
    }

    /// Starts serving the repository, returning the runtime that runs the
    /// server and the SDK's handle on the repository.
    fn serve(&self) -> (Runtime, Repo) {
        let runtime = Runtime::new().unwrap();
        let tokens: StaticTokens = toml::from_str("\"node:ci\" = \"bench\"").unwrap();
        let credentials = Credentials::new(self.node.clone(), Box::new(tokens));
        let mut repositories = Repositories::new(credentials);
        repositories.insert("demo".parse().unwrap(), self.service());
        let url = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, Api::router(repositories)).await });
            format!("http://{address}")
        });
        let repo = origin_client::Client::new(&url)
            .with_token("bench")
            .repo("demo".parse().unwrap());
        (runtime, repo)
    }
}

/// Statements numbered from `start` about `subject`, each of its own kind.
fn statements(subject: RevisionId, start: usize, count: usize) -> Vec<EncodedObject> {
    (start..start + count)
        .map(|n| {
            Statement {
                subject: RecordId::Revision(subject),
                kind: Kind::new(format!("bench.k{n}")).unwrap(),
                verdict: Some(Verdict::Pass),
                issuer: "node:ci".parse().unwrap(),
                inputs: None,
                payload: None,
                supersedes: None,
                promoted_from: None,
            }
            .encode()
        })
        .collect()
}

/// Putting a batch of new statements, durably: one request.
#[divan::bench(args = [1, 100, 1000], sample_count = 30)]
fn put_records(bencher: Bencher<'_, '_>, batch: usize) {
    let hosted = Hosted::new();
    let (runtime, repo) = hosted.serve();
    let next = AtomicUsize::new(0);
    bencher
        .counter(ItemsCount::new(batch))
        .with_inputs(|| Pack {
            objects: statements(
                hosted.revision,
                next.fetch_add(batch, Ordering::Relaxed),
                batch,
            ),
        })
        .bench_local_refs(|pack| runtime.block_on(repo.put_records(pack)).unwrap());
}

/// Fetching a batch of statements by id: one request.
#[divan::bench(args = [1, 100, 1000], sample_count = 30)]
fn fetch_records(bencher: Bencher<'_, '_>, batch: usize) {
    let hosted = Hosted::new();
    let (runtime, repo) = hosted.serve();
    let objects = statements(hosted.revision, 0, batch);
    let request = FetchRecords {
        ids: objects.iter().map(EncodedObject::id).collect(),
    };
    runtime
        .block_on(repo.put_records(&Pack { objects }))
        .unwrap();
    bencher
        .counter(ItemsCount::new(batch))
        .bench(|| runtime.block_on(repo.fetch_records(&request)).unwrap());
}

/// An idle long-poll woken by a statement put: from the put's start to the
/// poll's answer. A put alone, `put_records` with 1, is most of it.
#[divan::bench(sample_count = 30)]
fn long_poll_wakeup(bencher: Bencher<'_, '_>) {
    let hosted = Hosted::new();
    let (runtime, repo) = hosted.serve();
    let next = AtomicUsize::new(0);
    bencher
        .with_inputs(|| {
            let cursor = runtime
                .block_on(repo.events(&EventsQuery::default()))
                .unwrap()
                .next;
            let query = EventsQuery {
                cursor: Some(cursor),
                wait: Some(Period(Duration::from_secs(30))),
                kind: List(vec!["statement.issued".parse().unwrap()]),
                ..EventsQuery::default()
            };
            let poller = repo.clone();
            let poll = runtime.spawn(async move { poller.events(&query).await.unwrap() });
            // Long enough for the poll to be waiting.
            std::thread::sleep(Duration::from_millis(20));
            let pack = Pack {
                objects: statements(hosted.revision, next.fetch_add(1, Ordering::Relaxed), 1),
            };
            (poll, pack)
        })
        .bench_local_values(|(poll, pack)| {
            runtime.block_on(async {
                repo.put_records(&pack).await.unwrap();
                let page = poll.await.unwrap();
                assert_eq!(page.events.len(), 1);
            });
        });
}

/// An idle long-poll woken by a statement another process wrote: the
/// server only sees it at its next read of the op log, every 50 ms.
#[divan::bench(sample_count = 10)]
fn long_poll_wakeup_from_another_process(bencher: Bencher<'_, '_>) {
    let hosted = Hosted::new();
    let (runtime, repo) = hosted.serve();
    let other = Store::new(
        hosted.dir.path().join("demo"),
        origin_platform::Native::default(),
    );
    let next = AtomicUsize::new(0);
    bencher
        .with_inputs(|| {
            let cursor = runtime
                .block_on(repo.events(&EventsQuery::default()))
                .unwrap()
                .next;
            let query = EventsQuery {
                cursor: Some(cursor),
                wait: Some(Period(Duration::from_secs(30))),
                kind: List(vec!["statement.issued".parse().unwrap()]),
                ..EventsQuery::default()
            };
            let poller = repo.clone();
            let poll = runtime.spawn(async move { poller.events(&query).await.unwrap() });
            std::thread::sleep(Duration::from_millis(20));
            (
                poll,
                statements(hosted.revision, next.fetch_add(1, Ordering::Relaxed), 1),
            )
        })
        .bench_local_values(|(poll, objects)| {
            other.write_all(&objects).unwrap();
            let page = runtime.block_on(poll).unwrap();
            assert_eq!(page.events.len(), 1);
        });
}

/// How many records the repository holds for `land_with_large_index`.
const RECORDS: usize = 200_000;

/// Landing a change made on the tip in a repository of 200k statements,
/// through one service that keeps its index open (`true`), or a new one
/// each time, which reads the index whole (`false`), as a command does.
#[divan::bench(args = [true, false], sample_count = 10)]
fn land_with_large_index(bencher: Bencher<'_, '_>, kept: bool) {
    let hosted = Hosted::new();
    let service = hosted.service();
    for batch in statements(hosted.revision, 0, RECORDS).chunks(1000) {
        service.store().write_all(batch).unwrap();
    }
    let land = |service: &OriginService, change: ChangeId| {
        let request = LandRequest {
            proposal: Proposal::Change(change),
            onto: None,
        };
        let landed = service.land(&Caller::Owner, &request).unwrap();
        assert!(matches!(landed.landings[..], [Landing::Landed { .. }]));
    };
    // Reads the index for the kept service.
    service
        .statements(&Caller::Owner, RecordId::Revision(hosted.revision))
        .unwrap();
    let mut n = 0;
    bencher
        .with_inputs(|| {
            n += 1;
            let revision = Hosted::put_change(&service, n);
            service.store().get(revision).unwrap().change
        })
        .bench_local_values(|change| {
            if kept {
                land(&service, change);
            } else {
                land(&hosted.service(), change);
            }
        });
}
