//! Snapshotting a working copy: the cost behind `status`, `diff`, and
//! `commit`.

use std::sync::atomic::{AtomicU64, Ordering};

use divan::Bencher;
use divan::counter::ItemsCount;
use origin_src::fixture::FixtureTree;
use origin_src::store::ObjectStore;
use origin_src::worktree::{Snapshot, StatCache};

fn main() {
    divan::main();
}

const TREE: FixtureTree = FixtureTree {
    files: 5_000,
    ..FixtureTree::LARGE
};

/// A fixture working copy with an object store inside it.
fn working_copy() -> (tempfile::TempDir, ObjectStore) {
    let dir = tempfile::tempdir().unwrap();
    TREE.write(dir.path()).unwrap();
    let store = ObjectStore::new(dir.path().join(".origin/objects"));
    (dir, store)
}

/// The first snapshot: every file is read, hashed, compressed, and written.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn cold(bencher: Bencher<'_, '_>) {
    let (dir, _) = working_copy();
    bencher
        .counter(ItemsCount::new(TREE.files))
        .with_inputs(|| {
            let store = tempfile::tempdir().unwrap();
            let objects = ObjectStore::new(store.path());
            (store, objects)
        })
        .bench_local_refs(|(_, objects)| {
            Snapshot::new(dir.path(), objects)
                .take(&StatCache::default())
                .unwrap()
        });
}

/// Nothing changed: every file is answered by the stat cache.
#[divan::bench(sample_count = 20, sample_size = 1)]
fn unchanged(bencher: Bencher<'_, '_>) {
    let (dir, store) = working_copy();
    let snapshot = Snapshot::new(dir.path(), &store);
    let (_, cache) = snapshot.take(&StatCache::default()).unwrap();
    let cache = cache.unwrap();
    bencher
        .counter(ItemsCount::new(TREE.files))
        .bench_local(|| snapshot.take(&cache).unwrap());
}

/// A typical edit: 100 files changed since the last snapshot.
#[divan::bench(sample_count = 20, sample_size = 1)]
fn edited_100(bencher: Bencher<'_, '_>) {
    let (dir, store) = working_copy();
    let snapshot = Snapshot::new(dir.path(), &store);
    let (_, cache) = snapshot.take(&StatCache::default()).unwrap();
    let cache = cache.unwrap();
    let version = AtomicU64::new(1);
    bencher
        .counter(ItemsCount::new(TREE.files))
        .with_inputs(|| {
            TREE.edit(dir.path(), 100, version.fetch_add(1, Ordering::Relaxed))
                .unwrap();
        })
        .bench_local_values(|()| snapshot.take(&cache).unwrap());
}
