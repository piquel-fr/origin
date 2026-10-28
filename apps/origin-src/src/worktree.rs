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
//! store. They never overwrite an edit made since the working copy was last
//! snapshotted, and the files they write are durable once they return.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use origin_platform::{CloneError, CloneFile, Platform};
use origin_shared::object::{DecodeError, Reader};
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

/// A file a checkout changes, as it found it, unedited since the snapshot
/// the checkout starts from: its metadata, or `None` where there was no file.
/// A directory where a file goes counts as none: the checkout removes the
/// files it holds first, then the directory, unless it still holds
/// something (see [`Worktree::remove`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seen(Option<Stat>);

impl Seen {
    /// What is at `path`.
    fn at(path: &Path) -> Result<Self, SnapshotError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => Ok(Self(None)),
            Ok(metadata) => Ok(Self(Some(Stat::new(&metadata)))),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(Self(None))
            }
            Err(source) => Err(SnapshotError::Io {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Fails with [`SnapshotError::Edited`] unless the file at `path` is
    /// still as `seen`, if it was seen.
    fn check(seen: Option<Self>, path: &Path) -> Result<(), SnapshotError> {
        match seen {
            Some(seen) if Self::at(path)? != seen => Err(SnapshotError::Edited(path.to_owned())),
            _ => Ok(()),
        }
    }
}

/// A file a checkout writes.
struct Write {
    path: PathBuf,
    entry: TreeEntry,
    /// The file there, as found unedited, if the checkout checks.
    seen: Option<Seen>,
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
            .and_then(|bytes| Self::decode(&bytes).ok())
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

    /// Reads what [`StatCache::encode`] wrote.
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        if reader.take(Self::MAGIC.len())? != Self::MAGIC {
            // Another format, or another version of this one.
            return Err(DecodeError::NonCanonical("stat cache header"));
        }
        let scanned_at = i64::from_le_bytes(reader.array()?);
        let root = reader.option(|reader| reader.array().map(TreeId::from_bytes))?;
        let mut files = HashMap::new();
        while reader.remaining() > 0 {
            let path = reader.prefixed_str()?.to_owned();
            let kind = EntryKind::from_byte(reader.u8()?)?;
            if kind.is_tree() {
                return Err(DecodeError::UnknownTag {
                    field: "file kind",
                    tag: kind as u8,
                });
            }
            let stat = Stat {
                kind,
                size: reader.u64()?,
                mtime: i64::from_le_bytes(reader.array()?),
                ctime: i64::from_le_bytes(reader.array()?),
                inode: reader.u64()?,
            };
            let id = ObjectId::from_bytes(reader.array()?);
            files.insert(path, (stat, id));
        }
        Ok(Self {
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

    /// From this many files on, a checkout syncs the first half of them
    /// while it writes the second.
    const EARLY_SYNC: usize = 1024;

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
    /// modify, in parallel. Directories left empty are removed. Once it
    /// returns, the files and directories it changed are durable, at the
    /// cost of one batch sync.
    ///
    /// With `unchanged`, the stat cache of the snapshot the changes start
    /// from, it first checks that no file it changes was edited since, and
    /// changes nothing if one was: a file created where it writes one, or a
    /// file it removes or replaces whose metadata changed, unless its
    /// content didn't. It checks each file again just before it replaces
    /// it, and stops at one edited in between, or at a directory where it
    /// writes a file that holds what it doesn't track.
    ///
    /// A file one of `sources` has, unmodified, is cloned from it, or copied
    /// where the file system cannot clone. Others are written from the
    /// store.
    ///
    /// # Errors
    ///
    /// Fails with [`SnapshotError::Edited`] if a file changed since the
    /// snapshot, if a file's content cannot be read from the store, or a
    /// file cannot be removed, written or synced.
    pub fn checkout(
        &self,
        changes: &[FileChange],
        sources: &[CloneSource],
        unchanged: Option<&StatCache>,
    ) -> Result<(), SnapshotError> {
        if changes.is_empty() {
            return Ok(());
        }
        let seen = unchanged
            .map(|cache| Self::par_map(changes, |change| self.check_unchanged(change, cache)))
            .transpose()?;
        self.apply(changes, seen.as_deref(), sources)
    }

    /// Makes the files match a snapshot like [`Worktree::checkout`], but
    /// only those still as `changes` start from, by content: it leaves the
    /// others as they are. Files a move cut short left on either side, or
    /// edited since, so end up on one side, and no edit is lost.
    ///
    /// # Errors
    ///
    /// Fails like [`Worktree::checkout`].
    pub fn restore(&self, changes: &[FileChange]) -> Result<(), SnapshotError> {
        let cache = StatCache::default();
        let seen = Self::par_map(changes, |change| {
            match self.check_unchanged(change, &cache) {
                Ok(seen) => Ok(Some(seen)),
                Err(SnapshotError::Edited(_)) => Ok(None),
                Err(error) => Err(error),
            }
        })?;
        let (changes, seen): (Vec<_>, Vec<_>) = changes
            .iter()
            .zip(seen)
            .filter_map(|(change, seen)| Some((change.clone(), seen?)))
            .unzip();
        if changes.is_empty() {
            return Ok(());
        }
        self.apply(&changes, Some(&seen), &[])
    }

    /// Checks `changes` out, as [`Worktree::checkout`] explains. With
    /// `seen`, the files as found unedited, one per change, it stops at a
    /// file edited since.
    fn apply(
        &self,
        changes: &[FileChange],
        seen: Option<&[Seen]>,
        sources: &[CloneSource],
    ) -> Result<(), SnapshotError> {
        let seen = |index: usize| seen.map(|seen| seen[index]);
        // Removing first frees the paths of files that become directories,
        // and of directories that become files.
        for (index, change) in changes.iter().enumerate() {
            if change.new.is_some() {
                continue;
            }
            let path = self.absolute(&change.path);
            Seen::check(seen(index), &path)?;
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
            .enumerate()
            .filter_map(|(index, change)| {
                Some(Write {
                    path: self.absolute(&change.path),
                    entry: change.new?,
                    seen: seen(index),
                })
            })
            .collect();
        // Creating the directories first lets the files be written in any
        // order.
        let mut dirs: Vec<_> = writes
            .iter()
            .filter_map(|write| write.path.parent())
            .collect();
        dirs.sort_unstable();
        dirs.dedup();
        for dir in dirs {
            fs::create_dir_all(dir).map_err(|source| SnapshotError::Io {
                path: dir.to_owned(),
                source,
            })?;
        }
        let originals = Originals::new(sources, writes.iter().map(|write| &write.entry));
        // Many files take long to sync: the first half is written back
        // while the second is written, so the final sync, the barrier, has
        // less left to wait for.
        let half = if writes.len() >= Self::EARLY_SYNC {
            writes.len() / 2
        } else {
            0
        };
        let (first, second) = writes.split_at(half);
        self.write_files(first, &originals)?;
        thread::scope(|scope| {
            let early = (!first.is_empty()).then(|| {
                scope.spawn(|| {
                    let files = first
                        .iter()
                        .filter(|write| write.entry.kind != EntryKind::Symlink);
                    self.store
                        .platform
                        .sync_batch(files.map(|write| write.path.as_path()))
                })
            });
            let written = self.write_files(second, &originals);
            let synced = early.map(|early| {
                early
                    .join()
                    .unwrap_or_else(|panic| panic::resume_unwind(panic))
            });
            written?;
            synced.transpose().map_err(StoreError::from)?;
            Ok::<_, SnapshotError>(())
        })?;
        self.sync(changes, &writes)
    }

    /// Writes `writes`, a contiguous run of files per core, rather than
    /// files handed out one at a time: creating files locks their
    /// directory, and a run mostly keeps to directories of its own, which
    /// is 40% faster.
    fn write_files(
        &self,
        writes: &[Write],
        originals: &Originals<'_>,
    ) -> Result<(), SnapshotError> {
        Store::<P>::on_every_core(writes, |chunk| {
            chunk
                .iter()
                .try_for_each(|write| self.write_file(write, originals))
        })
        .into_iter()
        .collect()
    }

    /// Fails with [`SnapshotError::Edited`] if the file `change` changes was
    /// edited since the snapshot `cache` is of, which `change` starts from.
    /// Returns the file as it found it.
    fn check_unchanged(
        &self,
        change: &FileChange,
        cache: &StatCache,
    ) -> Result<Seen, SnapshotError> {
        let path = self.absolute(&change.path);
        let seen = Seen::at(&path)?;
        let unchanged = match (seen.0, change.old) {
            (None, None) => true,
            (Some(stat), Some(old)) => {
                cache.get(change.path.as_str(), &stat) == Some(old.id)
                    || Self::holds(&change.path, &path, old)?
            }
            _ => false,
        };
        if unchanged {
            Ok(seen)
        } else {
            Err(SnapshotError::Edited(path))
        }
    }

    /// Whether the file at `absolute`, `path` in the working copy, has the
    /// content and kind of `entry`, read and hashed: for files the cache
    /// doesn't vouch for, such as those edited just before the snapshot. A
    /// socket, FIFO or device has neither.
    fn holds(path: &RepoPath, absolute: &Path, entry: TreeEntry) -> Result<bool, SnapshotError> {
        let tracked = fs::symlink_metadata(absolute)
            .is_ok_and(|metadata| metadata.is_file() || metadata.is_symlink());
        if !tracked {
            return Ok(false);
        }
        let file = FoundFile {
            path: path.clone(),
            absolute: absolute.to_owned(),
        };
        let examined = Self::examine(&file, &StatCache::default())?;
        Ok(examined.id == entry.id && examined.stat.kind == entry.kind)
    }

    /// Makes what a checkout of `changes` wrote, `writes`, durable: the
    /// files, but symlinks, whose content their directory holds, and every
    /// directory up to the root whose entries may have changed.
    fn sync(&self, changes: &[FileChange], writes: &[Write]) -> Result<(), SnapshotError> {
        let write = self.store.durable_write();
        for file in writes {
            if file.entry.kind != EntryKind::Symlink {
                write.sync_with(&file.path);
            }
        }
        let mut dirs = BTreeSet::new();
        for change in changes {
            let path = self.absolute(&change.path);
            for dir in path.ancestors().skip(1) {
                if !dirs.insert(dir.to_owned()) || dir == self.root {
                    break;
                }
            }
        }
        // Those a removal emptied are gone, with their entries.
        for dir in dirs.into_iter().filter(|dir| dir.is_dir()) {
            write.sync_with(dir);
        }
        write.commit().map_err(StoreError::from)?;
        Ok(())
    }

    fn absolute(&self, path: &RepoPath) -> PathBuf {
        self.root.join(path.as_str())
    }

    /// Removes a file, if it's there. A directory there, where a checkout
    /// writes a file, goes if it's empty: the checkout removed the files it
    /// held first. One that still holds something holds what no snapshot
    /// tracks, such as a `.git`, a FIFO or a file created since, which
    /// counts as edited.
    fn remove(path: &Path) -> Result<(), SnapshotError> {
        let io_error = |source| SnapshotError::Io {
            path: path.to_owned(),
            source,
        };
        match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) if path.symlink_metadata().is_ok_and(|found| found.is_dir()) => {
                match fs::remove_dir(path) {
                    Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
                        Err(SnapshotError::Edited(path.to_owned()))
                    }
                    removed => removed.map_err(io_error),
                }
            }
            removed => removed.map_err(io_error),
        }
    }

    /// Writes the file `write` describes, whose directory exists, replacing
    /// any file there, unless it was edited since it was seen: a clone of
    /// an original if there is one, else from the store.
    fn write_file(&self, write: &Write, originals: &Originals<'_>) -> Result<(), SnapshotError> {
        let Write { path, entry, seen } = write;
        let (path, entry) = (path.as_path(), *entry);
        let io_error = |source| SnapshotError::Io {
            path: path.to_owned(),
            source,
        };
        Seen::check(*seen, path)?;
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
    /// Its stat cache, read from `cache_path` the first time a checkout
    /// looks for a file in it.
    cache: OnceLock<StatCache>,
    cache_path: PathBuf,
    /// Cleared once its file system refuses a clone, to copy from then on.
    clones: AtomicBool,
}

impl CloneSource {
    /// The working copy at `root`, whose stat cache is `cache`.
    #[must_use]
    pub fn new(root: PathBuf, cache: StatCache) -> Self {
        Self {
            root,
            cache: OnceLock::from(cache),
            cache_path: PathBuf::new(),
            clones: AtomicBool::new(true),
        }
    }

    /// The working copy at `root`, whose stat cache is saved at
    /// `cache_path`, which is only read if a checkout needs it.
    #[must_use]
    pub fn saved(root: PathBuf, cache_path: PathBuf) -> Self {
        Self {
            root,
            cache: OnceLock::new(),
            cache_path,
            clones: AtomicBool::new(true),
        }
    }

    fn cache(&self) -> &StatCache {
        self.cache.get_or_init(|| StatCache::load(&self.cache_path))
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
        (made && self.source.cache().get(self.path, &stat) == Some(self.id)).then_some(stat.kind)
    }
}

/// The settled regular files of some [`CloneSource`]s, by blob.
struct Originals<'s>(HashMap<ObjectId, (&'s CloneSource, &'s str)>);

impl<'s> Originals<'s> {
    /// The originals of the regular files among `entries`, from the first
    /// of `sources` that has each. A source's stat cache is only read while
    /// some file has no original yet.
    fn new<'e>(sources: &'s [CloneSource], entries: impl Iterator<Item = &'e TreeEntry>) -> Self {
        let mut originals = HashMap::new();
        if sources.is_empty() {
            return Self(originals);
        }
        let mut wanted: HashSet<ObjectId> = entries
            .filter(|entry| Self::regular(entry))
            .map(|entry| entry.id)
            .collect();
        for source in sources {
            if wanted.is_empty() {
                break;
            }
            let cache = source.cache();
            for (path, (stat, id)) in &cache.files {
                let regular = stat.kind != EntryKind::Symlink;
                if regular && cache.settled(stat) && wanted.remove(id) {
                    originals.insert(*id, (source, path.as_str()));
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
    /// A checkout found a file edited since the snapshot it starts from,
    /// and changed nothing.
    #[error(
        "{} changed while origin was moving the working copy, which stays where it was: run the command again",
        .0.display()
    )]
    Edited(PathBuf),
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
        assert_eq!(
            StatCache::decode(b"garbage"),
            Err(DecodeError::NonCanonical("stat cache header"))
        );
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
            .checkout(&fixture.store.diff(new, old).unwrap(), &[], None)
            .unwrap();
        assert!(!fixture.dir.path().join("swap/inner").exists());
        let (back, cache) = fixture.snapshot(&cache);
        assert_eq!(back, old);
        worktree
            .checkout(&fixture.store.diff(old, new).unwrap(), &[], None)
            .unwrap();
        assert!(!fixture.dir.path().join("gone").exists(), "emptied dirs go");
        assert_eq!(fixture.snapshot(&cache).0, new);
    }

    #[test]
    fn checkouts_never_overwrite_edits_made_since_the_snapshot() {
        let fixture = Fixture::new();
        fixture.write("kept", "kept");
        fixture.write("changed", "before");
        let (old, _) = fixture.snapshot(&StatCache::default());
        fixture.write("changed", "after");
        fixture.write("added", "added");
        let (new, _) = fixture.snapshot(&StatCache::default());
        let back = fixture.store.diff(new, old).unwrap();
        let worktree = fixture.worktree();

        // Snapshotted, then edited again before the checkout.
        let (_, cache) = fixture.snapshot(&StatCache::default());
        fixture.write("changed", "edited since");
        let error = worktree.checkout(&back, &[], Some(&cache)).unwrap_err();
        assert!(
            matches!(&error, SnapshotError::Edited(path) if path.ends_with("changed")),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(fixture.dir.path().join("added")).unwrap(),
            "added",
            "nothing changed"
        );

        // Edited back, it is as snapshotted: its content tells.
        fixture.write("changed", "after");
        worktree.checkout(&back, &[], Some(&cache)).unwrap();
        assert_eq!(fixture.snapshot(&StatCache::default()).0, old);

        // A file created where the checkout writes one.
        let (_, cache) = fixture.snapshot(&StatCache::default());
        fixture.write("added", "created since");
        let forward = fixture.store.diff(old, new).unwrap();
        assert!(matches!(
            worktree.checkout(&forward, &[], Some(&cache)),
            Err(SnapshotError::Edited(_))
        ));
        assert_eq!(
            fs::read_to_string(fixture.dir.path().join("added")).unwrap(),
            "created since"
        );
    }

    #[test]
    fn checkouts_stop_at_a_file_edited_while_they_run() {
        let fixture = Fixture::new();
        fixture.write("file", "before");
        let (old, _) = fixture.snapshot(&StatCache::default());
        fixture.write("file", "after");
        let (new, cache) = fixture.snapshot(&StatCache::default());
        let back = fixture.store.diff(new, old).unwrap();
        let worktree = fixture.worktree();

        // Found unedited, then edited before the checkout replaces it.
        let seen: Vec<_> = back
            .iter()
            .map(|change| worktree.check_unchanged(change, &cache).unwrap())
            .collect();
        fixture.write("file", "edited meanwhile");
        let error = worktree.apply(&back, Some(&seen), &[]).unwrap_err();
        assert!(matches!(error, SnapshotError::Edited(_)), "{error}");
        assert_eq!(
            fs::read_to_string(fixture.dir.path().join("file")).unwrap(),
            "edited meanwhile"
        );
    }

    #[test]
    fn checkouts_turn_files_into_directories_and_back() {
        let fixture = Fixture::new();
        fixture.write("a", "file");
        let (file, _) = fixture.snapshot(&StatCache::default());
        fs::remove_file(fixture.dir.path().join("a")).unwrap();
        fixture.write("a/b", "in a directory");
        let (dir, cache) = fixture.snapshot(&StatCache::default());
        let worktree = fixture.worktree();

        let back = fixture.store.diff(dir, file).unwrap();
        worktree.checkout(&back, &[], Some(&cache)).unwrap();
        let (found, cache) = fixture.snapshot(&StatCache::default());
        assert_eq!(found, file);
        let forth = fixture.store.diff(file, dir).unwrap();
        worktree.checkout(&forth, &[], Some(&cache)).unwrap();
        assert_eq!(fixture.snapshot(&StatCache::default()).0, dir);
    }

    #[test]
    fn restoring_keeps_files_edited_since() {
        let fixture = Fixture::new();
        for path in ["unmoved", "moved", "edited"] {
            fixture.write(path, "old");
        }
        let (old, _) = fixture.snapshot(&StatCache::default());
        for path in ["unmoved", "moved", "edited", "added"] {
            fixture.write(path, "new");
        }
        let (new, _) = fixture.snapshot(&StatCache::default());
        let changes = fixture.store.diff(old, new).unwrap();

        // A move cut short, then edits.
        fixture.write("unmoved", "old");
        fixture.write("edited", "edited");
        fixture.write("added", "created");
        fixture.worktree().restore(&changes).unwrap();
        let read = |path| fs::read_to_string(fixture.dir.path().join(path)).unwrap();
        assert_eq!(read("unmoved"), "new");
        assert_eq!(read("moved"), "new");
        assert_eq!(read("edited"), "edited");
        assert_eq!(read("added"), "created");
    }

    #[test]
    fn checkouts_read_only_the_stat_caches_they_need() {
        let fixture = Fixture::new();
        fixture.write("file", "content");
        let (root, mut cache) = fixture.snapshot(&StatCache::default());
        cache.scanned_at = i64::MAX;
        let empty = Tree::default().encode();
        fixture.store.write_all(slice::from_ref(&empty)).unwrap();
        let other = tempfile::tempdir().unwrap();
        let sources = [
            CloneSource::new(fixture.dir.path().to_owned(), cache),
            CloneSource::saved(other.path().to_owned(), other.path().join("state")),
        ];
        let worktree = Worktree::new(other.path(), &fixture.store);
        worktree.checkout(&[], &sources, None).unwrap();
        worktree
            .checkout(
                &fixture.store.diff(empty.id().into(), root).unwrap(),
                &sources,
                None,
            )
            .unwrap();
        assert!(
            sources[1].cache.get().is_none(),
            "the first source had every file"
        );
        assert_eq!(
            fs::read_to_string(other.path().join("file")).unwrap(),
            "content"
        );
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
                None,
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
