//! Snapshotting a working copy: the cost behind every command that reads it.
//! Objects are written durably, as the commands write them.

use std::sync::atomic::{AtomicU64, Ordering};

use divan::Bencher;
use divan::counter::ItemsCount;
use origin_platform::Native;
use origin_src::fixture::FixtureTree;
use origin_src::store::Store;
use origin_src::worktree::{StatCache, Worktree};

fn main() {
    divan::main();
}

const TREE: FixtureTree = FixtureTree {
    files: 5_000,
    ..FixtureTree::LARGE
};

/// An empty store in `dir`.
fn store(dir: &tempfile::TempDir) -> Store {
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    store
}

/// A fixture working copy with a store inside it.
fn working_copy() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    TREE.write(dir.path()).unwrap();
    let store = store(&dir);
    (dir, store)
}

/// The first snapshot: every file is read, hashed, compressed, and written.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn cold(bencher: Bencher<'_, '_>) {
    let (dir, _) = working_copy();
    bencher
        .counter(ItemsCount::new(TREE.files))
        .with_inputs(|| {
            let objects = tempfile::tempdir().unwrap();
            let store = store(&objects);
            (objects, store)
        })
        .bench_local_refs(|(_, store)| {
            Worktree::new(dir.path(), store)
                .snapshot(&StatCache::default())
                .unwrap()
        });
}

/// Nothing changed: every file is answered by the stat cache.
#[divan::bench(sample_count = 20, sample_size = 1)]
fn unchanged(bencher: Bencher<'_, '_>) {
    let (dir, store) = working_copy();
    let worktree = Worktree::new(dir.path(), &store);
    let (_, cache) = worktree.snapshot(&StatCache::default()).unwrap();
    let cache = cache.unwrap();
    bencher
        .counter(ItemsCount::new(TREE.files))
        .bench_local(|| worktree.snapshot(&cache).unwrap());
}

/// A typical edit: 100 files changed since the last snapshot.
#[divan::bench(sample_count = 20, sample_size = 1)]
fn edited_100(bencher: Bencher<'_, '_>) {
    let (dir, store) = working_copy();
    let worktree = Worktree::new(dir.path(), &store);
    let (_, cache) = worktree.snapshot(&StatCache::default()).unwrap();
    let cache = cache.unwrap();
    let version = AtomicU64::new(1);
    bencher
        .counter(ItemsCount::new(TREE.files))
        .with_inputs(|| {
            TREE.edit(dir.path(), 100, version.fetch_add(1, Ordering::Relaxed))
                .unwrap();
        })
        .bench_local_values(|()| worktree.snapshot(&cache).unwrap());
}
