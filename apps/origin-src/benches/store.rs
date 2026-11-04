//! Store throughput: durable writes with and without batching, verified
//! reads, and target appends. Run on a real disk: syncs cost nothing on a
//! RAM-backed file system.

use std::slice;

use divan::counter::{BytesCount, ItemsCount};
use divan::{Bencher, black_box};
use origin_platform::Native;
use origin_shared::{AcceptanceBody, EncodedObject, NodeKey, Record, TargetName, Tree, TreeId};
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
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
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
