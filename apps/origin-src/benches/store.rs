//! Store throughput: durable writes with and without batching, verified
//! reads, target appends, the record index, and reading events from the op
//! log. Run on a real disk: syncs cost nothing on a RAM-backed file system.

use std::fs::File;
use std::io;
use std::path::Path;
use std::slice;

use divan::counter::{BytesCount, ItemsCount};
use divan::{Bencher, black_box};
use origin_platform::{
    CloneFile, Durability, Error, FileLock, FileSystem, FsKind, Native, Platform, ReadAt,
};
use origin_shared::{
    AcceptanceBody, Base, ChangeId, EncodedObject, Kind, NodeKey, Record, RecordId, Revision,
    Statement, TargetName, Tree, TreeId, Verdict,
};
use origin_src::events::{Events, Filter};
use origin_src::index::Index;
use origin_src::store::Store;

fn main() {
    divan::main();
}

/// Source-code-like content, which compresses like real files do.
fn content(len: usize, seed: usize) -> Vec<u8> {
    format!("fn item_{seed}() -> usize {{ {seed} }}\n")
        .into_bytes()
        .into_iter()
        .cycle()
        .take(len)
        .collect()
}

/// An initialized store in a new temporary directory.
fn store() -> (tempfile::TempDir, Store) {
    store_on(Native::default())
}

/// An initialized store on `platform` in a new temporary directory.
fn store_on<P: Platform>(platform: P) -> (tempfile::TempDir, Store<P>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), platform);
    store.init().unwrap();
    (dir, store)
}

/// Writing new objects one batch each: a file and a directory sync per
/// object.
#[divan::bench(args = [4 * 1024, 1024 * 1024], sample_count = 20)]
fn write_unbatched(bencher: Bencher<'_, '_>, len: usize) {
    let (_dir, store) = store();
    let mut seed = 0;
    bencher
        .counter(BytesCount::new(len))
        .with_inputs(|| {
            seed += 1;
            EncodedObject::blob(&content(len, seed))
        })
        .bench_local_refs(|object| store.write_all(slice::from_ref(object)).unwrap());
}

/// Writing new 4 KiB objects in one batch, staged from every core: the
/// first snapshot of a tree of that many files.
#[divan::bench(args = [1_000, 20_000], sample_count = 5, sample_size = 1)]
fn write_batch(bencher: Bencher<'_, '_>, count: usize) {
    let objects: Vec<_> = (0..count)
        .map(|seed| EncodedObject::blob(&content(4 * 1024, seed)))
        .collect();
    bencher
        .counter(ItemsCount::new(count))
        .with_inputs(store)
        .bench_local_refs(|(_, store)| store.write_all(&objects).unwrap());
}

/// Walking a store of 20k objects for the temporary files crashed writes
/// left: what garbage collection pays.
#[divan::bench(sample_count = 20)]
fn remove_stale_temps(bencher: Bencher<'_, '_>) {
    let (_dir, store) = store();
    let objects: Vec<_> = (0..20_000)
        .map(|seed| EncodedObject::blob(&content(64, seed)))
        .collect();
    store.write_all(&objects).unwrap();
    bencher.bench(|| store.remove_stale_temps().unwrap());
}

/// Writing objects the store already has: the common case when
/// snapshotting. The batch syncs the directory of each, for the writer that
/// renamed it there may not have yet.
#[divan::bench(args = [1, 1_000, 20_000])]
fn write_existing(bencher: Bencher<'_, '_>, count: usize) {
    let (_dir, store) = store();
    let objects: Vec<_> = (0..count)
        .map(|seed| EncodedObject::blob(&content(4 * 1024, seed)))
        .collect();
    store.write_all(&objects).unwrap();
    bencher
        .counter(ItemsCount::new(count))
        .bench(|| store.write_all(black_box(&objects)).unwrap());
}

/// Reading, decompressing, and verifying objects: the cost of checkouts.
#[divan::bench(args = [4 * 1024, 1024 * 1024])]
fn read(bencher: Bencher<'_, '_>, len: usize) {
    let (_dir, store) = store();
    let object = EncodedObject::blob(&content(len, 0));
    store.write_all(slice::from_ref(&object)).unwrap();
    bencher
        .counter(BytesCount::new(len))
        .bench(|| store.read(black_box(object.id())).unwrap());
}

/// Appending an acceptance to a target: the durable part of every landing.
#[divan::bench(sample_count = 20)]
fn append(bencher: Bencher<'_, '_>) {
    let (_dir, store) = store();
    let key = NodeKey::from_bytes(&[42; 32]);
    let main = TargetName::MAIN;
    let state = TreeId::from(Tree::default().encode().id());
    bencher.bench_local(|| {
        let head = store.head(&main).unwrap();
        let acceptance = AcceptanceBody {
            target: main.id(),
            seq: head.map_or(0, |head| head.seq + 1),
            prev: head.map(|head| head.acceptance),
            state,
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: None,
        }
        .sign(&key);
        store.append(&main, &acceptance).unwrap()
    });
}

/// `count` records, as a busy repository receives them: a revision of one
/// of a hundred changes, evolving from that change's previous one, then nine
/// statements about it.
fn records(count: usize) -> Vec<EncodedObject> {
    let snapshot = TreeId::from(Tree::default().encode().id());
    let mut heads = vec![None; 100];
    let mut revision = None;
    (0..count)
        .map(|i| {
            if i % 10 == 0 {
                let change = i / 10 % heads.len();
                let encoded = Revision {
                    change: ChangeId::from_bytes([u8::try_from(change).unwrap(); 16]),
                    predecessors: heads[change].into_iter().collect(),
                    base: Base::Acceptance(EncodedObject::blob(b"main").id().into()),
                    deps: Vec::new(),
                    snapshot,
                    description: EncodedObject::blob(&content(64, i)).id().into(),
                    author: "user:bench".parse().unwrap(),
                }
                .encode();
                heads[change] = Some(encoded.id().into());
                revision = Some(encoded.id().into());
                encoded
            } else {
                Statement {
                    subject: RecordId::Revision(revision.unwrap()),
                    kind: Kind::new(format!("ci.check-{i}")).unwrap(),
                    verdict: Some(Verdict::Pass),
                    issuer: "node:ci-1".parse().unwrap(),
                    inputs: None,
                    payload: None,
                    supersedes: None,
                    promoted_from: None,
                }
                .encode()
            }
        })
        .collect()
}

/// The native platform without syncs, to measure what syncs hide.
#[derive(Clone, Copy, Debug, Default)]
struct NoSync;

impl Durability for NoSync {
    fn sync_file(&self, _: &File) -> io::Result<()> {
        Ok(())
    }

    fn sync_dir(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }

    fn sync_batch<'f>(&self, _: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
        Ok(())
    }
}

impl FileLock for NoSync {}

impl ReadAt for NoSync {}

impl CloneFile for NoSync {
    fn cannot_clone(&self, error: &io::Error) -> bool {
        Native::default().cannot_clone(error)
    }
}

impl FileSystem for NoSync {
    fn file_system(&self, path: &Path) -> io::Result<FsKind> {
        Native::default().file_system(path)
    }
}

/// Writing new records in one batch, which indexes them, against blobs of
/// the same content, which aren't indexed: the difference is what indexing
/// costs, which [`NoSync`] shows apart from the syncs. One record is what
/// issuing a statement costs.
#[divan::bench(
    types = [Native, NoSync],
    consts = [true, false],
    args = [1, 1_000, 20_000],
    sample_count = 10,
    sample_size = 1,
)]
fn write_records<P: Platform + Default, const INDEXED: bool>(
    bencher: Bencher<'_, '_>,
    count: usize,
) {
    let objects: Vec<_> = records(count)
        .into_iter()
        .map(|record| {
            if INDEXED {
                record
            } else {
                EncodedObject::blob(record.bytes())
            }
        })
        .collect();
    bencher
        .counter(ItemsCount::new(count))
        .with_inputs(|| store_on(P::default()))
        .bench_local_refs(|(_, store)| store.write_all(&objects).unwrap());
}

/// Opening the index of 20k records written in batches of a hundred.
#[divan::bench(sample_count = 20)]
fn open_index(bencher: Bencher<'_, '_>) {
    let (_dir, store) = store();
    for batch in records(20_000).chunks(100) {
        store.write_all(batch).unwrap();
    }
    bencher
        .counter(ItemsCount::new(20_000_usize))
        .bench(|| Index::open(&store).unwrap());
}

/// Rebuilding the index of 20k records from the store.
#[divan::bench(sample_count = 10)]
fn rebuild_index(bencher: Bencher<'_, '_>) {
    let (_dir, store) = store();
    store.write_all(&records(20_000)).unwrap();
    bencher
        .counter(ItemsCount::new(20_000_usize))
        .bench(|| Index::rebuild(&store).unwrap());
}

/// Reading the events of 10k records, each written in its own batch, as
/// issuing statements does: what a subscriber catching up pays.
#[divan::bench(sample_count = 20)]
fn read_events(bencher: Bencher<'_, '_>) {
    let (_dir, store) = store_on(NoSync);
    for record in records(10_000) {
        store.write_all(slice::from_ref(&record)).unwrap();
    }
    let events = Events::new(&store);
    bencher
        .counter(ItemsCount::new(10_000_usize))
        .bench(|| events.after(0, &Filter::default(), usize::MAX).unwrap());
}

/// Polling for events when there are none, as `watch` does every 50 ms.
#[divan::bench]
fn poll_events(bencher: Bencher<'_, '_>) {
    let (_dir, store) = store_on(NoSync);
    for record in records(10_000) {
        store.write_all(slice::from_ref(&record)).unwrap();
    }
    let events = Events::new(&store);
    let end = events.end().unwrap();
    bencher.bench(|| events.after(end, &Filter::default(), usize::MAX).unwrap());
}
