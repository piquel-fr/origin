//! Snapshotting the working copy into trees.
//!
//! There is no staging area: a snapshot records every file in the working
//! copy. To keep that fast, a stat cache remembers each file's metadata and
//! blob id, so only files whose metadata changed are read and hashed, and
//! every phase runs in parallel.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use origin_shared::{
    EncodedObject, EntryKind, EntryName, Object, ObjectId, RepoPath, Tree, TreeEntry,
};

use crate::store::{ObjectStore, StoreError};

/// File metadata that changes whenever the content might have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    kind: EntryKind,
    size: u64,
    /// Modification time in nanoseconds since the Unix epoch. Tools can set
    /// it back, so it does not prove a file is unchanged on its own.
    mtime: i64,
    /// Status change time in nanoseconds since the Unix epoch. Every write
    /// moves it forward, and nothing can set it back.
    ctime: i64,
    inode: u64,
}

impl Stat {
    fn new(kind: EntryKind, metadata: &fs::Metadata) -> Self {
        Self {
            kind,
            size: metadata.size(),
            mtime: metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
            ctime: metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
            inode: metadata.ino(),
        }
    }
}

/// The stat cache: what each file looked like when it was last hashed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatCache {
    /// When the scan that produced the cache started, in nanoseconds since
    /// the Unix epoch.
    scanned_at: i64,
    /// Keyed by path. Paths are only looked up, never turned into trees, so
    /// they are not validated when loaded.
    files: HashMap<String, (Stat, ObjectId)>,
}

impl StatCache {
    /// Identifies the binary format.
    const MAGIC: &[u8; 4] = b"OST2";

    /// How long before a scan a file must have been modified for its cache
    /// entry to be trusted, in nanoseconds. Filesystems record modification
    /// times in ticks of up to 2 seconds (FAT), and a file edited again
    /// within the tick of its last modification keeps the same metadata.
    const RACY_MARGIN: i64 = 2_000_000_000;

    /// Looks up a file whose metadata has not changed.
    ///
    /// Files modified shortly before the previous scan started ("racy"
    /// files) are never trusted, since they may have changed again without
    /// their metadata changing. They are rehashed until they settle.
    fn get(&self, path: &RepoPath, stat: &Stat) -> Option<ObjectId> {
        let (cached, id) = self.files.get(path.as_str())?;
        let settled =
            stat.mtime.max(stat.ctime) < self.scanned_at.saturating_sub(Self::RACY_MARGIN);
        (cached == stat && settled).then_some(*id)
    }

    /// Loads the cache, treating a missing or unreadable one as empty: the
    /// cache only saves work, so the next scan just rehashes everything.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| Self::decode(&bytes))
            .unwrap_or_default()
    }

    /// Saves the cache atomically.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be written.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let temp = path.with_extension("tmp");
        fs::write(&temp, self.encode())?;
        fs::rename(temp, path)
    }

    /// `magic scanned_at:i64` then, per file,
    /// `path_len:u32 path kind:u8 size:u64 mtime:i64 ctime:i64 inode:u64
    /// id:[u8; 32]`.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.files.len() * 96);
        out.extend_from_slice(Self::MAGIC);
        out.extend_from_slice(&self.scanned_at.to_le_bytes());
        for (path, (stat, id)) in &self.files {
            let path_len = u32::try_from(path.len()).expect("paths are shorter than 4 GiB");
            out.extend_from_slice(&path_len.to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            out.push(stat.kind as u8);
            out.extend_from_slice(&stat.size.to_le_bytes());
            out.extend_from_slice(&stat.mtime.to_le_bytes());
            out.extend_from_slice(&stat.ctime.to_le_bytes());
            out.extend_from_slice(&stat.inode.to_le_bytes());
            out.extend_from_slice(id.as_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
            let (head, rest) = bytes.split_at_checked(len)?;
            *bytes = rest;
            Some(head)
        }
        fn array<const N: usize>(bytes: &mut &[u8]) -> Option<[u8; N]> {
            take(bytes, N)?.try_into().ok()
        }

        let mut bytes = bytes.strip_prefix(Self::MAGIC)?;
        let scanned_at = i64::from_le_bytes(array(&mut bytes)?);
        let mut files = HashMap::new();
        while !bytes.is_empty() {
            let path_len = u32::from_le_bytes(array(&mut bytes)?) as usize;
            let path = std::str::from_utf8(take(&mut bytes, path_len)?)
                .ok()?
                .to_owned();
            let kind = match array::<1>(&mut bytes)?[0] {
                1 => EntryKind::File,
                2 => EntryKind::Executable,
                3 => EntryKind::Symlink,
                _ => return None,
            };
            let stat = Stat {
                kind,
                size: u64::from_le_bytes(array(&mut bytes)?),
                mtime: i64::from_le_bytes(array(&mut bytes)?),
                ctime: i64::from_le_bytes(array(&mut bytes)?),
                inode: u64::from_le_bytes(array(&mut bytes)?),
            };
            let id = ObjectId::from_bytes(array(&mut bytes)?);
            files.insert(path, (stat, id));
        }
        Some(Self { scanned_at, files })
    }
}

/// Everything the walk found.
struct Walk {
    /// Every directory, root first, each depth after the previous one.
    dirs: Vec<FoundDir>,
    /// The range of `dirs` at each depth.
    depths: Vec<Range<usize>>,
    files: Vec<FoundFile>,
}

/// A directory found by the walk.
struct FoundDir {
    path: RepoPath,
    absolute: PathBuf,
    entries: Vec<(EntryName, Child)>,
}

/// An entry of a [`FoundDir`], indexing the walk's directories or files.
enum Child {
    Dir(usize),
    File(usize),
}

/// What listing a directory says an entry is.
enum Listed {
    Dir,
    File { symlink: bool },
}

/// A file found by the walk, not yet examined.
struct FoundFile {
    path: RepoPath,
    absolute: PathBuf,
    symlink: bool,
}

/// What examining a file found.
struct Examined {
    stat: Stat,
    id: ObjectId,
    /// Whether the file had to be read, because the cache could not vouch
    /// for it.
    hashed: bool,
}

/// Snapshots the working copy rooted at `root` into `store`.
pub struct Snapshot<'a> {
    root: &'a Path,
    store: &'a ObjectStore,
}

impl<'a> Snapshot<'a> {
    /// Below this many items, threads cost more than they save.
    const PARALLEL_THRESHOLD: usize = 64;

    /// Prepares a snapshot of `root`.
    #[must_use]
    pub fn new(root: &'a Path, store: &'a ObjectStore) -> Self {
        Self { root, store }
    }

    /// Records every file into the store and returns the root tree, with the
    /// updated stat cache if it changed.
    ///
    /// Three phases, each parallel: list directories one depth at a time,
    /// examine every file (one `lstat`, plus reading and hashing the files
    /// the cache cannot vouch for), and write trees one depth at a time from
    /// the deepest.
    ///
    /// # Errors
    ///
    /// Fails if the working copy cannot be read, contains a file name that
    /// cannot be tracked, or objects cannot be written.
    pub fn take(&self, cache: &StatCache) -> Result<(ObjectId, Option<StatCache>), SnapshotError> {
        let scanned_at = now_nanos();
        let Walk {
            dirs,
            depths,
            files,
        } = self.walk()?;
        let examined = Self::par_map(&files, |file| self.examine(file, cache))?;

        let mut trees = vec![None; dirs.len()];
        for depth in depths.into_iter().rev() {
            let written = Self::par_map(&dirs[depth.clone()], |dir| {
                self.write_tree(dir, &trees, &examined)
            })?;
            trees[depth].copy_from_slice(&written);
        }
        let root = match trees[0] {
            Some(root) => root,
            None => self.write(&Tree::default())?,
        };

        let changed = files.len() != cache.files.len() || examined.iter().any(|file| file.hashed);
        let next = changed.then(|| StatCache {
            scanned_at,
            files: files
                .into_iter()
                .zip(examined)
                .map(|(file, Examined { stat, id, .. })| {
                    (file.path.as_str().to_owned(), (stat, id))
                })
                .collect(),
        });
        Ok((root, next))
    }

    /// Lists the working copy one depth at a time, listing each depth's
    /// directories in parallel.
    fn walk(&self) -> Result<Walk, SnapshotError> {
        let mut dirs = vec![FoundDir {
            path: RepoPath::root(),
            absolute: self.root.to_owned(),
            entries: Vec::new(),
        }];
        let (mut depths, mut files) = (Vec::new(), Vec::new());
        let mut depth = 0..1;
        while !depth.is_empty() {
            let listings = Self::par_map(&dirs[depth.clone()], |dir| Self::list(&dir.absolute))?;
            let next = dirs.len();
            for (parent, listing) in depth.clone().zip(listings) {
                for (name, listed) in listing {
                    let path = dirs[parent].path.join(&name);
                    let absolute = dirs[parent].absolute.join(name.as_str());
                    let child = match listed {
                        Listed::Dir => {
                            dirs.push(FoundDir {
                                path,
                                absolute,
                                entries: Vec::new(),
                            });
                            Child::Dir(dirs.len() - 1)
                        }
                        Listed::File { symlink } => {
                            files.push(FoundFile {
                                path,
                                absolute,
                                symlink,
                            });
                            Child::File(files.len() - 1)
                        }
                    };
                    dirs[parent].entries.push((name, child));
                }
            }
            depths.push(depth);
            depth = next..dirs.len();
        }
        Ok(Walk {
            dirs,
            depths,
            files,
        })
    }

    /// Lists one directory, skipping repository metadata.
    fn list(dir: &Path) -> Result<Vec<(EntryName, Listed)>, SnapshotError> {
        let io_error = |source| SnapshotError::Io {
            path: dir.to_owned(),
            source,
        };
        let mut listing = Vec::new();
        for entry in fs::read_dir(dir).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                return Err(SnapshotError::NonUtf8Name(entry.path()));
            };
            if EntryName::is_reserved(&name) {
                continue;
            }
            let name =
                EntryName::new(name).map_err(|_| SnapshotError::UntrackableName(entry.path()))?;
            // On most filesystems the type comes with the listing, without a
            // `stat` call.
            let file_type = entry.file_type().map_err(io_error)?;
            let listed = if file_type.is_dir() {
                Listed::Dir
            } else if file_type.is_file() || file_type.is_symlink() {
                Listed::File {
                    symlink: file_type.is_symlink(),
                }
            } else {
                // Sockets, FIFOs and devices have no content to track.
                continue;
            };
            listing.push((name, listed));
        }
        Ok(listing)
    }

    /// Stats a file, and reads and stores it unless the cache vouches for it.
    fn examine(&self, file: &FoundFile, cache: &StatCache) -> Result<Examined, SnapshotError> {
        let io_error = |source| SnapshotError::Io {
            path: file.absolute.clone(),
            source,
        };
        let metadata = fs::symlink_metadata(&file.absolute).map_err(io_error)?;
        let kind = if file.symlink {
            EntryKind::Symlink
        } else if metadata.mode() & 0o111 != 0 {
            EntryKind::Executable
        } else {
            EntryKind::File
        };
        let stat = Stat::new(kind, &metadata);
        if let Some(id) = cache.get(&file.path, &stat) {
            return Ok(Examined {
                stat,
                id,
                hashed: false,
            });
        }
        let blob = if file.symlink {
            let target = fs::read_link(&file.absolute).map_err(io_error)?;
            EncodedObject::blob(target.as_os_str().as_bytes())
        } else {
            let size = usize::try_from(stat.size).unwrap_or(0);
            EncodedObject::read_blob(File::open(&file.absolute).map_err(io_error)?, size)
                .map_err(io_error)?
        };
        self.store.write(&blob)?;
        Ok(Examined {
            stat,
            id: blob.id(),
            hashed: true,
        })
    }

    /// Writes a directory's tree, given the trees of every deeper directory.
    /// Returns `None` for a directory without files, which is not tracked.
    fn write_tree(
        &self,
        dir: &FoundDir,
        trees: &[Option<ObjectId>],
        examined: &[Examined],
    ) -> Result<Option<ObjectId>, SnapshotError> {
        let tree: Tree = dir
            .entries
            .iter()
            .filter_map(|(name, child)| {
                let entry = match *child {
                    Child::Dir(index) => TreeEntry {
                        kind: EntryKind::Tree,
                        id: trees[index]?,
                    },
                    Child::File(index) => TreeEntry {
                        kind: examined[index].stat.kind,
                        id: examined[index].id,
                    },
                };
                Some((name.clone(), entry))
            })
            .collect();
        if tree.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.write(&tree)?))
    }

    fn write(&self, tree: &Tree) -> Result<ObjectId, StoreError> {
        let encoded = Object::Tree(tree.clone()).encode();
        self.store.write(&encoded)?;
        Ok(encoded.id())
    }

    /// Maps `items` in order, on as many threads as there are cores when
    /// there are enough items.
    fn par_map<T: Sync, R: Send>(
        items: &[T],
        map: impl Fn(&T) -> Result<R, SnapshotError> + Sync,
    ) -> Result<Vec<R>, SnapshotError> {
        if items.len() < Self::PARALLEL_THRESHOLD {
            return items.iter().map(map).collect();
        }
        let workers = thread::available_parallelism().map_or(4, usize::from);
        let next = AtomicUsize::new(0);
        let results = Mutex::new(Vec::with_capacity(items.len()));
        thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| -> Result<(), SnapshotError> {
                        let mut local = Vec::new();
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(item) = items.get(index) else {
                                break;
                            };
                            local.push((index, map(item)?));
                        }
                        results
                            .lock()
                            .expect("no worker panics holding the lock")
                            .extend(local);
                        Ok(())
                    })
                })
                .collect();
            handles
                .into_iter()
                .try_for_each(|handle| handle.join().expect("snapshot threads do not panic"))
        })?;
        let mut results = results
            .into_inner()
            .expect("no worker panics holding the lock");
        results.sort_unstable_by_key(|(index, _)| *index);
        Ok(results.into_iter().map(|(_, result)| result).collect())
    }
}

fn now_nanos() -> i64 {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_nanos()).unwrap_or(i64::MAX)
}

/// A failure to snapshot the working copy.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// A file or directory cannot be read.
    #[error("cannot read {}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A file name is not UTF-8.
    #[error("cannot track {}: its name is not UTF-8", .0.display())]
    NonUtf8Name(PathBuf),
    /// A file name cannot appear in a tree.
    #[error("cannot track {}: invalid file name", .0.display())]
    UntrackableName(PathBuf),
    /// Objects cannot be stored.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        store: ObjectStore,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = ObjectStore::new(dir.path().join(".origin/objects"));
            Self { dir, store }
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        /// Takes a snapshot, returning the cache to use next.
        fn snapshot(&self, cache: &StatCache) -> (ObjectId, StatCache) {
            let (root, next) = Snapshot::new(self.dir.path(), &self.store)
                .take(cache)
                .unwrap();
            (root, next.unwrap_or_else(|| cache.clone()))
        }

        fn entry(&self, root: ObjectId, path: &str) -> TreeEntry {
            let mut entry = TreeEntry {
                kind: EntryKind::Tree,
                id: root,
            };
            for component in path.split('/') {
                entry = *self.store.tree(entry.id).unwrap().get(component).unwrap();
            }
            entry
        }
    }

    #[test]
    fn records_files_kinds_and_skips_metadata() {
        let fixture = Fixture::new();
        fixture.write("src/lib.rs", "lib");
        fixture.write("run.sh", "#!/bin/sh");
        fixture.write(".git/HEAD", "ignored");
        let run = fixture.dir.path().join("run.sh");
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("src/lib.rs", fixture.dir.path().join("link")).unwrap();

        let (root, cache) = fixture.snapshot(&StatCache::default());
        let tree = fixture.store.tree(root).unwrap();
        let names: Vec<_> = tree.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["link", "run.sh", "src"]);
        assert_eq!(fixture.entry(root, "run.sh").kind, EntryKind::Executable);
        let link = fixture.entry(root, "link");
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(
            fixture.store.read(link.id).unwrap().blob_content().unwrap(),
            b"src/lib.rs"
        );
        assert_eq!(
            fixture.entry(root, "src/lib.rs").id,
            EncodedObject::blob(b"lib").id()
        );
        assert_eq!(cache.files.len(), 3);
    }

    #[test]
    fn same_size_rewrites_are_detected() {
        let fixture = Fixture::new();
        fixture.write("file", "aaaa");
        let (first, cache) = fixture.snapshot(&StatCache::default());
        // Rewritten right away, possibly within the same mtime tick.
        fixture.write("file", "bbbb");
        let (second, _) = fixture.snapshot(&cache);
        assert_ne!(first, second);
    }

    #[test]
    fn restored_modification_times_do_not_hide_changes() {
        let fixture = Fixture::new();
        fixture.write("file", "aaaa");
        let path = fixture.dir.path().join("file");
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let (first, mut cache) = fixture.snapshot(&StatCache::default());
        // Trust every entry, as if the file had settled long ago.
        cache.scanned_at = i64::MAX;
        fixture.write("file", "bbbb");
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let (second, _) = fixture.snapshot(&cache);
        assert_ne!(first, second);
    }

    #[test]
    fn cache_hits_skip_reading() {
        let fixture = Fixture::new();
        fixture.write("file", "content");
        let (root, mut cache) = fixture.snapshot(&StatCache::default());
        // Pretend the scan happened long after the write, so the entry is
        // trusted, and point it at a different blob: a hit returns the
        // cached id without reading the file.
        cache.scanned_at = i64::MAX;
        let fake = EncodedObject::blob(b"cached");
        fixture.store.write(&fake).unwrap();
        cache.files.get_mut("file").unwrap().1 = fake.id();

        let (cached_root, _) = fixture.snapshot(&cache);
        assert_ne!(cached_root, root);
        assert_eq!(fixture.entry(cached_root, "file").id, fake.id());
    }

    #[test]
    fn unchanged_caches_are_not_replaced() {
        let fixture = Fixture::new();
        fixture.write("file", "content");
        let (_, mut cache) = fixture.snapshot(&StatCache::default());
        cache.scanned_at = i64::MAX;
        let snapshot = Snapshot::new(fixture.dir.path(), &fixture.store);
        assert!(snapshot.take(&cache).unwrap().1.is_none());

        fixture.write("other", "new file");
        assert!(snapshot.take(&cache).unwrap().1.is_some());
    }

    #[test]
    fn hashes_many_files_in_parallel() {
        let fixture = Fixture::new();
        for i in 0..Snapshot::PARALLEL_THRESHOLD * 2 {
            fixture.write(&format!("dir{}/file{i}", i % 7), &i.to_string());
        }
        let (root, cache) = fixture.snapshot(&StatCache::default());
        assert_eq!(cache.files.len(), Snapshot::PARALLEL_THRESHOLD * 2);
        assert_eq!(
            fixture.entry(root, "dir3/file10").id,
            EncodedObject::blob(b"10").id()
        );
    }

    #[test]
    fn cache_round_trips() {
        let fixture = Fixture::new();
        fixture.write("a", "1");
        fixture.write("b/c", "2");
        let (_, cache) = fixture.snapshot(&StatCache::default());
        let path = fixture.dir.path().join(".origin/state");
        cache.save(&path).unwrap();
        assert_eq!(StatCache::load(&path), cache);
        assert_eq!(StatCache::decode(b"garbage"), None);
    }
}
