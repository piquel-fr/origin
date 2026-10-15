//! The files of a working copy: snapshotting them into trees, and checking
//! trees out into them.
//!
//! There is no staging area: a snapshot records every file in the working
//! copy. To keep that fast, a stat cache remembers each file's metadata and
//! blob id, so only files whose metadata changed are read and hashed, and
//! every phase runs in parallel.
//!
//! Checkouts clone files from the repository's other working copies, where
//! their stat caches prove the files unmodified, and write the rest from the
//! store.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use origin_platform::{CloneError, CloneFile, Platform};
use origin_shared::{
    EncodedObject, EntryKind, EntryName, ObjectId, Record, RepoPath, Tree, TreeEntry, TreeId,
};

use crate::store::{Store, StoreError};
use crate::tree::FileChange;

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
    /// The metadata of a file, from `lstat`.
    fn new(metadata: &fs::Metadata) -> Self {
        let kind = if metadata.is_symlink() {
            EntryKind::Symlink
        } else if metadata.mode() & 0o111 != 0 {
            EntryKind::Executable
        } else {
            EntryKind::File
        };
        Self {
            kind,
            size: metadata.size(),
            mtime: metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
            ctime: metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
            inode: metadata.ino(),
        }
    }
}

/// The stat cache: what each file looked like when it was last hashed, and
/// the snapshot those files made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatCache {
    /// When the scan that produced the cache started, in nanoseconds since
    /// the Unix epoch.
    scanned_at: i64,
    /// The root tree of that scan. A scan that finds the same files, all
    /// vouched for by the cache, finds this tree without building it.
    root: Option<TreeId>,
    /// Keyed by path. Paths are only looked up, never turned into trees, so
    /// they are not validated when loaded.
    files: HashMap<String, (Stat, ObjectId)>,
}

impl StatCache {
    /// Identifies the binary format.
    const MAGIC: &[u8; 4] = b"OST3";

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
    fn get(&self, path: &str, stat: &Stat) -> Option<ObjectId> {
        let (cached, id) = self.files.get(path)?;
        (cached == stat && self.settled(stat)).then_some(*id)
    }

    /// Whether a file with this metadata had last changed long enough
    /// before the scan for its entry to be trusted.
    fn settled(&self, stat: &Stat) -> bool {
        stat.mtime.max(stat.ctime) < self.scanned_at.saturating_sub(Self::RACY_MARGIN)
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

    /// Saves the cache atomically, but not durably: losing it only costs
    /// rehashing. Every object it names is already durable.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be written.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let temp = path.with_extension("tmp");
        fs::write(&temp, self.encode())?;
        fs::rename(temp, path)
    }

    /// `magic scanned_at:i64 has_root:u8 [root:[u8; 32]]` then, per file,
    /// `path_len:u32 path kind:u8 size:u64 mtime:i64 ctime:i64 inode:u64
    /// id:[u8; 32]`.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(48 + self.files.len() * 96);
        out.extend_from_slice(Self::MAGIC);
        out.extend_from_slice(&self.scanned_at.to_le_bytes());
        match self.root {
            Some(root) => {
                out.push(1);
                out.extend_from_slice(root.as_bytes());
            }
            None => out.push(0),
        }
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
        let root = match array::<1>(&mut bytes)?[0] {
            0 => None,
            1 => Some(TreeId::from_bytes(array(&mut bytes)?)),
            _ => return None,
        };
        let mut files = HashMap::new();
        while !bytes.is_empty() {
            let path_len = u32::from_le_bytes(array(&mut bytes)?) as usize;
            let path = str::from_utf8(take(&mut bytes, path_len)?).ok()?.to_owned();
            let kind = EntryKind::from_byte(array::<1>(&mut bytes)?[0])
                .ok()
                .filter(|kind| !kind.is_tree())?;
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
        Some(Self {
            scanned_at,
            root,
            files,
        })
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
    File,
}

/// A file found by the walk, not yet examined.
struct FoundFile {
    path: RepoPath,
    absolute: PathBuf,
}

/// What examining a file found.
struct Examined {
    stat: Stat,
    id: ObjectId,
    /// The file's blob, if it had to be read because the cache could not
    /// vouch for it.
    blob: Option<EncodedObject>,
}

/// The files of the working copy rooted at `root`, whose snapshots go to
/// `store`.
pub struct Worktree<'a, P> {
    root: &'a Path,
    store: &'a Store<P>,
}

impl<'a, P: Platform> Worktree<'a, P> {
    /// Below this many items, threads cost more than they save.
    const PARALLEL_THRESHOLD: usize = 64;

    /// The working copy rooted at `root`.
    #[must_use]
    pub fn new(root: &'a Path, store: &'a Store<P>) -> Self {
        Self { root, store }
    }

    /// Records every file into the store and returns the root tree, with the
    /// updated stat cache if it changed.
    ///
    /// Three phases, each parallel: list directories one depth at a time,
    /// examine every file (one `lstat`, plus reading and hashing the files
    /// the cache cannot vouch for), and build trees one depth at a time from
    /// the deepest. Then the new blobs and trees are written in one batch.
    /// A tree may be published before a blob it names, if a crash cuts the
    /// batch short, but nothing refers to the tree until the batch commits,
    /// and the next snapshot rewrites the blob: the stat cache that would
    /// vouch for it is only saved after the commit.
    ///
    /// # Errors
    ///
    /// Fails if the working copy cannot be read, contains a file name that
    /// cannot be tracked, or objects cannot be written.
    pub fn snapshot(
        &self,
        cache: &StatCache,
    ) -> Result<(TreeId, Option<StatCache>), SnapshotError> {
        let scanned_at = Self::now_nanos();
        let Walk {
            dirs,
            depths,
            files,
        } = self.walk()?;
        let mut examined = Self::par_map(&files, |file| Self::examine(file, cache))?;
        let unchanged =
            files.len() == cache.files.len() && examined.iter().all(|file| file.blob.is_none());
        if let Some(root) = cache.root.filter(|_| unchanged) {
            return Ok((root, None));
        }

        let mut objects: Vec<EncodedObject> = examined
            .iter_mut()
            .filter_map(|file| file.blob.take())
            .collect();
        let mut trees = vec![None; dirs.len()];
        for depth in depths.into_iter().rev() {
            let built = Self::par_map(&dirs[depth.clone()], |dir| {
                Ok(Self::build_tree(dir, &trees, &examined))
            })?;
            for (slot, tree) in trees[depth].iter_mut().zip(built) {
                *slot = tree.map(|tree| {
                    let id = tree.id().into();
                    objects.push(tree);
                    id
                });
            }
        }
        let root = trees[0].unwrap_or_else(|| {
            let empty = Tree::default().encode();
            let id = empty.id().into();
            objects.push(empty);
            id
        });
        self.store.write_all(&objects)?;

        let next = StatCache {
            scanned_at,
            root: Some(root),
            files: files
                .into_iter()
                .zip(examined)
                .map(|(file, Examined { stat, id, .. })| {
                    (file.path.as_str().to_owned(), (stat, id))
                })
                .collect(),
        };
        Ok((root, Some(next)))
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
                        Listed::File => {
                            files.push(FoundFile { path, absolute });
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
                Listed::File
            } else {
                // Sockets, FIFOs and devices have no content to track.
                continue;
            };
            listing.push((name, listed));
        }
        Ok(listing)
    }

    /// Stats a file, and reads and hashes it unless the cache vouches for it.
    fn examine(file: &FoundFile, cache: &StatCache) -> Result<Examined, SnapshotError> {
        let io_error = |source| SnapshotError::Io {
            path: file.absolute.clone(),
            source,
        };
        let stat = Stat::new(&fs::symlink_metadata(&file.absolute).map_err(io_error)?);
        if let Some(id) = cache.get(file.path.as_str(), &stat) {
            return Ok(Examined {
                stat,
                id,
                blob: None,
            });
        }
        let blob = if stat.kind == EntryKind::Symlink {
            let target = fs::read_link(&file.absolute).map_err(io_error)?;
            EncodedObject::blob(target.as_os_str().as_bytes())
        } else {
            let size = usize::try_from(stat.size).unwrap_or(0);
            EncodedObject::read_blob(File::open(&file.absolute).map_err(io_error)?, size)
                .map_err(io_error)?
        };
        Ok(Examined {
            stat,
            id: blob.id(),
            blob: Some(blob),
        })
    }

    /// Builds a directory's tree, given the trees of every deeper directory.
    /// Returns `None` for a directory without files, which is not tracked.
    fn build_tree(
        dir: &FoundDir,
        trees: &[Option<TreeId>],
        examined: &[Examined],
    ) -> Option<EncodedObject> {
        let tree: Tree = dir
            .entries
            .iter()
            .filter_map(|(name, child)| {
                let entry = match *child {
                    Child::Dir(index) => TreeEntry {
                        kind: EntryKind::Tree,
                        id: trees[index]?.into(),
                    },
                    Child::File(index) => TreeEntry {
                        kind: examined[index].stat.kind,
                        id: examined[index].id,
                    },
                };
                Some((name.clone(), entry))
            })
            .collect();
        (!tree.is_empty()).then(|| tree.encode())
    }

    /// Makes the files match a snapshot, given how they differ from it:
    /// removes the files `changes` remove, then writes the ones they add or
    /// modify, in parallel. Directories left empty are removed.
    ///
    /// A file one of `sources` has, unmodified, is cloned from it, or copied
    /// where the file system cannot clone. Others are written from the
    /// store.
    ///
    /// # Errors
    ///
    /// Fails if a file's content cannot be read from the store, or a file
    /// cannot be removed or written.
    pub fn checkout(
        &self,
        changes: &[FileChange],
        sources: &[CloneSource],
    ) -> Result<(), SnapshotError> {
        // Removing first frees the paths of files that become directories,
        // and of directories that become files.
        for change in changes.iter().filter(|change| change.new.is_none()) {
            let path = self.absolute(&change.path);
            Self::remove(&path)?;
            for dir in path.ancestors().skip(1) {
                // Stops at the first directory that isn't empty, or the root.
                if dir == self.root || fs::remove_dir(dir).is_err() {
                    break;
                }
            }
        }
        let writes: Vec<_> = changes
            .iter()
            .filter_map(|change| Some((self.absolute(&change.path), change.new?)))
            .collect();
        // Creating the directories first lets the files be written in any
        // order.
        let mut dirs: Vec<_> = writes
            .iter()
            .filter_map(|(path, _)| path.parent())
            .collect();
        dirs.sort_unstable();
        dirs.dedup();
        for dir in dirs {
            fs::create_dir_all(dir).map_err(|source| SnapshotError::Io {
                path: dir.to_owned(),
                source,
            })?;
        }
        let originals = Originals::new(sources, writes.iter().map(|(_, entry)| entry));
        // A contiguous run of files per core, rather than files handed out
        // one at a time: creating files locks their directory, and a run
        // mostly keeps to directories of its own, which is 40% faster.
        Store::<P>::on_every_core(&writes, |chunk| {
            chunk
                .iter()
                .try_for_each(|(path, entry)| self.write_file(path, *entry, &originals))
        })
        .into_iter()
        .collect::<Result<(), _>>()?;
        Ok(())
    }

    fn absolute(&self, path: &RepoPath) -> PathBuf {
        self.root.join(path.as_str())
    }

    /// Removes a file, if it's there.
    fn remove(path: &Path) -> Result<(), SnapshotError> {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(SnapshotError::Io {
                path: path.to_owned(),
                source: error,
            }),
            _ => Ok(()),
        }
    }

    /// Writes the file `entry` describes at `path`, whose directory exists,
    /// replacing any file there: a clone of an original if there is one,
    /// else from the store.
    fn write_file(
        &self,
        path: &Path,
        entry: TreeEntry,
        originals: &Originals<'_>,
    ) -> Result<(), SnapshotError> {
        let io_error = |source| SnapshotError::Io {
            path: path.to_owned(),
            source,
        };
        // Never write through a symlink, or keep the old file's mode.
        Self::remove(path)?;
        if let Some(original) = originals.get(entry) {
            match original.clone_to(&self.store.platform, path) {
                Some(kind) if kind == entry.kind => return Ok(()),
                Some(_) => return Self::set_kind(path, entry.kind).map_err(io_error),
                None => Self::remove(path)?,
            }
        }
        let blob = self.store.read(entry.id)?;
        let content = blob.blob_content().map_err(|source| StoreError::Decode {
            id: entry.id,
            source,
        })?;
        match entry.kind {
            EntryKind::Symlink => symlink(OsStr::from_bytes(content), path).map_err(io_error),
            EntryKind::Executable => fs::write(path, content)
                .and_then(|()| Self::set_kind(path, entry.kind))
                .map_err(io_error),
            EntryKind::File | EntryKind::Tree => fs::write(path, content).map_err(io_error),
        }
    }

    /// Makes a regular file executable, or not.
    fn set_kind(path: &Path, kind: EntryKind) -> io::Result<()> {
        let mode = if kind == EntryKind::Executable {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    /// Maps `items` in order, on as many threads as there are cores when
    /// there are enough items. Threads take the next item as they finish
    /// one, since some files take much longer than others.
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

    fn now_nanos() -> i64 {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        i64::try_from(since_epoch.as_nanos()).unwrap_or(i64::MAX)
    }
}

/// Another working copy of the repository, whose files a checkout clones
/// rather than writing them from the store, when its stat cache proves them
/// unmodified.
#[derive(Debug)]
pub struct CloneSource {
    root: PathBuf,
    cache: StatCache,
    /// Cleared once its file system refuses a clone, to copy from then on.
    clones: AtomicBool,
}

impl CloneSource {
    /// The working copy at `root`, whose stat cache is `cache`.
    #[must_use]
    pub fn new(root: PathBuf, cache: StatCache) -> Self {
        Self {
            root,
            cache,
            clones: AtomicBool::new(true),
        }
    }
}

/// A file of a [`CloneSource`] that its cache vouches for.
struct Original<'s> {
    source: &'s CloneSource,
    path: &'s str,
    id: ObjectId,
}

impl Original<'_> {
    /// Clones the file to `to`, which must not exist, or copies it where the
    /// file system cannot clone. Returns the kind of file it made, or `None`
    /// if the original changed since its cache was saved or cannot be read,
    /// leaving whatever it made at `to`.
    fn clone_to(&self, platform: &impl CloneFile, to: &Path) -> Option<EntryKind> {
        let from = self.source.root.join(self.path);
        let made = if self.source.clones.load(Ordering::Relaxed) {
            match platform.clone_file(&from, to) {
                Ok(()) => true,
                Err(CloneError::Unsupported { .. }) => {
                    self.source.clones.store(false, Ordering::Relaxed);
                    fs::copy(&from, to).is_ok()
                }
                Err(CloneError::Io { .. }) => false,
            }
        } else {
            fs::copy(&from, to).is_ok()
        };
        // Every write moves a file's ctime forward, past the time its entry
        // settled at, so an original still matching its entry now was
        // unmodified during the clone.
        let stat = Stat::new(&fs::symlink_metadata(&from).ok()?);
        (made && self.source.cache.get(self.path, &stat) == Some(self.id)).then_some(stat.kind)
    }
}

/// The settled regular files of some [`CloneSource`]s, by blob.
struct Originals<'s>(HashMap<ObjectId, (&'s CloneSource, &'s str)>);

impl<'s> Originals<'s> {
    /// The originals of the regular files among `entries`.
    fn new<'e>(sources: &'s [CloneSource], entries: impl Iterator<Item = &'e TreeEntry>) -> Self {
        let mut originals = HashMap::new();
        if sources.is_empty() {
            return Self(originals);
        }
        let wanted: HashSet<ObjectId> = entries
            .filter(|entry| Self::regular(entry))
            .map(|entry| entry.id)
            .collect();
        for source in sources {
            for (path, (stat, id)) in &source.cache.files {
                let regular = stat.kind != EntryKind::Symlink;
                if regular && wanted.contains(id) && source.cache.settled(stat) {
                    originals.entry(*id).or_insert((source, path.as_str()));
                }
            }
        }
        Self(originals)
    }

    /// The original of a regular file. Symlinks are never cloned, though
    /// their blob may be a file's content.
    fn get(&self, entry: TreeEntry) -> Option<Original<'s>> {
        if !Self::regular(&entry) {
            return None;
        }
        let &(source, path) = self.0.get(&entry.id)?;
        Some(Original {
            source,
            path,
            id: entry.id,
        })
    }

    fn regular(entry: &TreeEntry) -> bool {
        matches!(entry.kind, EntryKind::File | EntryKind::Executable)
    }
}

/// A failure to snapshot or check out the working copy.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// A file or directory cannot be read or written.
    #[error("cannot access {}", path.display())]
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
    /// Objects cannot be read or stored.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use origin_platform::{Durability, Error, FileLock, FileSystem, FsKind, Native, ReadAt};

    use std::slice;

    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path().join(".origin"), Native::default());
            store.init().unwrap();
            Self { dir, store }
        }

        fn worktree(&self) -> Worktree<'_, Native> {
            Worktree::new(self.dir.path(), &self.store)
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        /// Takes a snapshot, returning the cache to use next.
        fn snapshot(&self, cache: &StatCache) -> (TreeId, StatCache) {
            let (root, next) = self.worktree().snapshot(cache).unwrap();
            (root, next.unwrap_or_else(|| cache.clone()))
        }

        fn entry(&self, root: TreeId, path: &str) -> TreeEntry {
            let mut entry = TreeEntry {
                kind: EntryKind::Tree,
                id: root.into(),
            };
            for component in path.split('/') {
                let tree = self.store.get(TreeId::from(entry.id)).unwrap();
                entry = *tree.get(component).unwrap();
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
        let tree = fixture.store.get(root).unwrap();
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
    fn empty_working_copies_are_empty_trees() {
        let fixture = Fixture::new();
        let (root, _) = fixture.snapshot(&StatCache::default());
        assert_eq!(fixture.store.get(root).unwrap(), Tree::default());
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
        fixture.write("other", "content");
        let (root, mut cache) = fixture.snapshot(&StatCache::default());
        // Pretend the scan happened long after the write, so the entries are
        // trusted, and point one at a different blob: a hit returns the
        // cached id without reading the file.
        cache.scanned_at = i64::MAX;
        cache.root = None;
        let fake = EncodedObject::blob(b"cached");
        fixture
            .store
            .write_all(std::slice::from_ref(&fake))
            .unwrap();
        cache.files.get_mut("file").unwrap().1 = fake.id();

        let (cached_root, _) = fixture.snapshot(&cache);
        assert_ne!(cached_root, root);
        assert_eq!(fixture.entry(cached_root, "file").id, fake.id());
    }

    #[test]
    fn unchanged_files_reuse_the_cached_root() {
        let fixture = Fixture::new();
        fixture.write("file", "content");
        let (root, mut cache) = fixture.snapshot(&StatCache::default());
        cache.scanned_at = i64::MAX;
        let worktree = fixture.worktree();
        assert_eq!(worktree.snapshot(&cache).unwrap(), (root, None));

        fixture.write("other", "new file");
        let (changed, next) = worktree.snapshot(&cache).unwrap();
        assert_ne!(changed, root);
        assert_eq!(next.unwrap().root, Some(changed));
    }

    #[test]
    fn hashes_many_files_in_parallel() {
        let fixture = Fixture::new();
        let count = Worktree::<Native>::PARALLEL_THRESHOLD * 2;
        for i in 0..count {
            fixture.write(&format!("dir{}/file{i}", i % 7), &i.to_string());
        }
        let (root, cache) = fixture.snapshot(&StatCache::default());
        assert_eq!(cache.files.len(), count);
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

    #[test]
    fn checkouts_reproduce_snapshots() {
        let fixture = Fixture::new();
        fixture.write("keep", "same");
        fixture.write("edit", "old");
        fixture.write("gone/deep/file", "removed");
        fixture.write("swap", "a file that becomes a directory");
        fixture.write("dir/inner", "a directory that becomes a file");
        let (old, cache) = fixture.snapshot(&StatCache::default());

        for path in ["edit", "swap", "gone/deep/file", "dir/inner"] {
            fs::remove_file(fixture.dir.path().join(path)).unwrap();
        }
        fs::remove_dir_all(fixture.dir.path().join("gone")).unwrap();
        fs::remove_dir(fixture.dir.path().join("dir")).unwrap();
        fixture.write("edit", "new");
        fixture.write("swap/inner", "now inside");
        fixture.write("dir", "now a file");
        fixture.write("run.sh", "#!/bin/sh");
        let run = fixture.dir.path().join("run.sh");
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("keep", fixture.dir.path().join("link")).unwrap();
        let (new, cache) = fixture.snapshot(&cache);

        // Back to the old snapshot, then forward to the new one.
        let worktree = fixture.worktree();
        worktree
            .checkout(&fixture.store.diff(new, old).unwrap(), &[])
            .unwrap();
        assert!(!fixture.dir.path().join("swap/inner").exists());
        let (back, cache) = fixture.snapshot(&cache);
        assert_eq!(back, old);
        worktree
            .checkout(&fixture.store.diff(old, new).unwrap(), &[])
            .unwrap();
        assert!(!fixture.dir.path().join("gone").exists(), "emptied dirs go");
        assert_eq!(fixture.snapshot(&cache).0, new);
    }

    /// The native platform, except that it counts clones and refuses them,
    /// as ext4 does.
    #[derive(Debug, Default)]
    struct NoClones {
        attempts: AtomicUsize,
    }

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
            self.attempts.fetch_add(1, Ordering::Relaxed);
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

    #[test]
    fn checkouts_clone_only_unmodified_files() {
        let fixture = Fixture::new();
        fixture.write("same", "same");
        fixture.write("edited", "before");
        fixture.write("run.sh", "#!/bin/sh");
        let run = fixture.dir.path().join("run.sh");
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("same", fixture.dir.path().join("link")).unwrap();
        // Enough files that every writer thread has several.
        let files = 64;
        for i in 0..files {
            fixture.write(&format!("copies/{i}"), &i.to_string());
        }
        let (root, mut cache) = fixture.snapshot(&StatCache::default());
        cache.scanned_at = i64::MAX;
        fixture.write("edited", "after!");
        let empty = Tree::default().encode();
        fixture.store.write_all(slice::from_ref(&empty)).unwrap();

        let other = tempfile::tempdir().unwrap();
        let store = Store::new(fixture.dir.path().join(".origin"), NoClones::default());
        let source = CloneSource::new(fixture.dir.path().to_owned(), cache);
        Worktree::new(other.path(), &store)
            .checkout(
                &store.diff(empty.id().into(), root).unwrap(),
                slice::from_ref(&source),
            )
            .unwrap();
        // Each writer thread tries once at most: the first refusal it sees
        // makes it, and every thread after, copy.
        let threads = thread::available_parallelism().map_or(1, usize::from);
        let attempts = store.platform.attempts.load(Ordering::Relaxed);
        assert!(
            (1..=threads.min(files)).contains(&attempts),
            "a refused clone falls back to copies for the rest: {attempts} attempts"
        );
        let read = |path| fs::read_to_string(other.path().join(path)).unwrap();
        assert_eq!(read("edited"), "before", "a modified original isn't copied");
        assert_eq!(read("same"), "same");
        let (copied, _) = Worktree::new(other.path(), &fixture.store)
            .snapshot(&StatCache::default())
            .unwrap();
        assert_eq!(copied, root, "kinds and links are reproduced");
    }
}
