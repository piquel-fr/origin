//! Object store throughput: compression, filesystem writes, and verified reads.

use divan::counter::{BytesCount, ItemsCount};
use divan::{Bencher, black_box};
use origin_shared::EncodedObject;
use origin_src::store::ObjectStore;

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

/// Writing new objects: the cost of snapshotting changed files.
#[divan::bench(args = [4 * 1024, 1024 * 1024])]
fn write(bencher: Bencher<'_, '_>, len: usize) {
    let dir = tempfile::tempdir().unwrap();
    let store = ObjectStore::new(dir.path());
    let mut seed = 0;
    bencher
        .counter(BytesCount::new(len))
        .with_inputs(|| {
            seed += 1;
            EncodedObject::blob(&content(len, seed))
        })
        .bench_local_refs(|object| store.write(object).unwrap());
}

/// Writing objects the store already has: the common case when snapshotting.
#[divan::bench]
fn write_existing(bencher: Bencher<'_, '_>) {
    let dir = tempfile::tempdir().unwrap();
    let store = ObjectStore::new(dir.path());
    let object = EncodedObject::blob(&content(4 * 1024, 0));
    store.write(&object).unwrap();
    bencher
        .counter(ItemsCount::new(1_usize))
        .bench(|| store.write(black_box(&object)).unwrap());
}

/// Reading, decompressing, and verifying objects: the cost of checkouts.
#[divan::bench(args = [4 * 1024, 1024 * 1024])]
fn read(bencher: Bencher<'_, '_>, len: usize) {
    let dir = tempfile::tempdir().unwrap();
    let store = ObjectStore::new(dir.path());
    let object = EncodedObject::blob(&content(len, 0));
    store.write(&object).unwrap();
    bencher
        .counter(BytesCount::new(len))
        .bench(|| store.read(black_box(object.id())).unwrap());
}
