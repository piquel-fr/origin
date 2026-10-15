//! Creating a workspace: checking a 20k-file tree out into an empty
//! directory, durably, by writing every file from the store, by copying
//! them from another working copy, or by cloning them where the file system
//! can.
//!
//! Workspaces are created in the temporary directory, so set `TMPDIR` to a
//! directory on Btrfs, XFS or APFS to measure real clones. Elsewhere,
//! `cloned` falls back to copying after its first clone.

use std::io;
use std::path::Path;
use std::sync::LazyLock;
use std::thread;
use std::time::Duration;

use divan::Bencher;
use divan::counter::ItemsCount;
use origin_platform::{
    CloneError, CloneFile, Durability, Error, FileLock, FileSystem, FsKind, Native, Platform,
    ReadAt,
};
use origin_shared::{Record, Tree, TreeId};
use origin_src::fixture::FixtureTree;
use origin_src::store::Store;
use origin_src::tree::FileChange;
use origin_src::worktree::{CloneSource, StatCache, Worktree};

fn main() {
    divan::main();
}

const TREE: FixtureTree = FixtureTree::LARGE;

/// The main working copy: the fixture, snapshotted once its files settled,
/// so its stat cache vouches for every file.
struct Main {
    dir: tempfile::TempDir,
    cache: StatCache,
    /// Every file of the snapshot, as checking it out adds them.
    changes: Vec<FileChange>,
}

static MAIN: LazyLock<Main> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    TREE.write(dir.path()).unwrap();
    let store = Store::new(dir.path().join(".origin"), Native::default());
    store.init().unwrap();
    let empty = Tree::default().encode();
    store.write_all(std::slice::from_ref(&empty)).unwrap();
    // The stat cache only vouches for files that changed well before the
    // scan.
    thread::sleep(Duration::from_millis(2_100));
    let (root, cache) = Worktree::new(dir.path(), &store)
        .snapshot(&StatCache::default())
        .unwrap();
    let changes = store.diff(TreeId::from(empty.id()), root).unwrap();
    Main {
        dir,
        cache: cache.unwrap(),
        changes,
    }
});

impl Main {
    /// Checks the tree out into new directories, with `platform`, from the
    /// main working copy if `clone_from_main`.
    fn checkout<P: Platform>(&self, bencher: Bencher<'_, '_>, platform: P, clone_from_main: bool) {
        let store = Store::new(self.dir.path().join(".origin"), platform);
        bencher
            .counter(ItemsCount::new(self.changes.len()))
            .with_inputs(|| {
                let sources = if clone_from_main {
                    vec![CloneSource::new(
                        self.dir.path().to_owned(),
                        self.cache.clone(),
                    )]
                } else {
                    Vec::new()
                };
                (tempfile::tempdir().unwrap(), sources)
            })
            .bench_local_refs(|(workspace, sources)| {
                Worktree::new(workspace.path(), &store)
                    .checkout(&self.changes, sources, None)
                    .unwrap();
            });
    }
}

/// Every file read from the store, decompressed, verified and written.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn from_store(bencher: Bencher<'_, '_>) {
    MAIN.checkout(bencher, Native::default(), false);
}

/// Every file copied from the main working copy.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn copied(bencher: Bencher<'_, '_>) {
    MAIN.checkout(bencher, NoClones, true);
}

/// Every file cloned from the main working copy, where the file system
/// can.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn cloned(bencher: Bencher<'_, '_>) {
    MAIN.checkout(bencher, Native::default(), true);
}

/// The native platform, on a file system that cannot clone.
#[derive(Clone, Copy, Debug, Default)]
struct NoClones;

impl Durability for NoClones {
    fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
        Native::default().sync_batch(files)
    }
}

impl FileLock for NoClones {}

impl ReadAt for NoClones {}

impl CloneFile for NoClones {
    fn cannot_clone(&self, _: &io::Error) -> bool {
        true
    }

    fn clone_file(&self, from: &Path, to: &Path) -> Result<(), CloneError> {
        Err(CloneError::Unsupported {
            from: from.to_owned(),
            to: to.to_owned(),
            source: io::ErrorKind::Unsupported.into(),
        })
    }
}

impl FileSystem for NoClones {
    fn file_system(&self, path: &Path) -> io::Result<FsKind> {
        Native::default().file_system(path)
    }
}
