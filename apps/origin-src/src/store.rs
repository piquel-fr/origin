//! A repository's storage on disk, under its `.origin` directory:
//! content-addressed objects here, and target chains in
//! [`targets`](crate::targets).
//!
//! Each object lives in its own zstd-compressed file at
//! `objects/loose/<first 2 hex digits>/<remaining hex digits>`. Objects are
//! written in batches through a [`DurableWrite`]: every file of a batch is
//! staged beside its destination, then the batch is synced at once, renamed
//! into place and its directories synced, so a batch costs one batch sync
//! and one sync per directory. Once [`Batch::commit`] returns, the objects
//! survive a crash, so anything that refers to them can be published.
//! Readers never see a partial object, and concurrent writers of the same
//! object are harmless. A batch that finds an object already there syncs
//! its directory too, since the writer that put it there may not have yet.
//! Writers that crash leave temporary files behind, which
//! [`Store::remove_stale_temps`] removes. Garbage collection
//! ([`Store::gc`]) calls it, so opening a store never pays for a walk over
//! every object.
//!
//! [`Store::compact`] moves small loose objects into [`packs`], and reads
//! look in the loose objects first, then in the packs.
//!
//! Committing a batch also indexes its new records, see
//! [`index`](crate::index).

use std::fs;
use std::io;
use std::num::NonZero;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use origin_platform::{DurableWrite, Native, Platform, WritableError};
use origin_shared::object::{DecodeError, IdMismatch};
use origin_shared::{EncodedObject, ObjectId, TargetName, TypedId};

use crate::index::Entry;
use crate::targets::Head;

pub use packs::Compaction;
use packs::Packs;

pub mod packs;

/// The storage of one repository.
#[derive(Clone, Debug)]
pub struct Store<P = Native> {
    root: PathBuf,
    pub(crate) platform: P,
    packs: Arc<Packs>,
}

impl<P: Platform> Store<P> {
    /// Favors speed: higher levels shrink source code little and cost a lot
    /// on every snapshot.
    const COMPRESSION_LEVEL: i32 = 1;

    /// How old a temporary file must be for [`Store::remove_stale_temps`]
    /// to remove it. A write publishes or removes its own files long before:
    /// even a million-object batch stages in under a minute.
    const STALE_TEMP: Duration = Duration::from_hours(1);

    /// Opens the storage in `root`, the `.origin` directory, on `platform`,
    /// for reading.
    pub fn new(root: impl Into<PathBuf>, platform: P) -> Self {
        Self {
            root: root.into(),
            platform,
            packs: Arc::default(),
        }
    }

    /// Opens the storage in `root` for writing, refusing a network file
    /// system.
    ///
    /// # Errors
    ///
    /// Fails if the store is on a network file system or its file system
    /// cannot be inspected.
    pub fn open_writable(root: impl Into<PathBuf>, platform: P) -> Result<Self, StoreError> {
        let store = Self::new(root, platform);
        store.platform.check_writable(&store.root)?;
        Ok(store)
    }

    /// Collects garbage: compacts the store, unless another compaction is
    /// running, then removes the temporary files crashed writes left behind.
    /// Returns what the compaction did.
    ///
    /// # Errors
    ///
    /// Fails like [`Store::compact`] and [`Store::remove_stale_temps`].
    pub fn gc(&self) -> Result<Option<Compaction>, StoreError> {
        let compaction = self.compact()?;
        self.remove_stale_temps()?;
        Ok(compaction)
    }

    /// Removes the temporary files that crashed writes left behind: those
    /// older than [`Store::STALE_TEMP`]. It walks every loose object, so
    /// only garbage collection calls it. It leaves the packs directory to
    /// the compactor, which removes what crashed compactions left there
    /// under its lock, so a long compaction never loses its pack.
    ///
    /// # Errors
    ///
    /// Fails if the store's files cannot be listed or a stale one removed.
    pub fn remove_stale_temps(&self) -> Result<(), StoreError> {
        self.remove_stale_temps_in(&self.root)
    }

    fn remove_stale_temps_in(&self, dir: &Path) -> Result<(), StoreError> {
        let io_error = |source| StoreError::io(dir, source);
        for entry in fs::read_dir(dir).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            if DurableWrite::<P>::is_temporary(&entry.file_name()) {
                let path = entry.path();
                match Self::remove_if_stale(&path) {
                    // Its writer published or removed it in the meantime.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    result => result.map_err(|source| StoreError::io(&path, source))?,
                }
            } else if entry.file_type().map_err(io_error)?.is_dir()
                && entry.path() != self.packs_dir()
            {
                self.remove_stale_temps_in(&entry.path())?;
            }
        }
        Ok(())
    }

    fn remove_if_stale(path: &Path) -> io::Result<()> {
        let modified = fs::symlink_metadata(path)?.modified()?;
        if modified.elapsed().is_ok_and(|age| age > Self::STALE_TEMP) {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Creates the directories of an empty store and its empty index,
    /// durably, refusing a network file system. Creating every object shard
    /// up front means batches only ever sync the shards they write to, never
    /// a directory they had to create.
    ///
    /// # Errors
    ///
    /// Fails if the store would be on a network file system, or if a
    /// directory cannot be created or synced.
    pub fn init(&self) -> Result<(), StoreError> {
        fs::create_dir_all(&self.root).map_err(|source| StoreError::io(&self.root, source))?;
        self.platform.check_writable(&self.root)?;
        let loose = self.loose();
        for shard in 0..=u8::MAX {
            let shard = loose.join(format!("{shard:02x}"));
            fs::create_dir_all(&shard).map_err(|source| StoreError::io(&shard, source))?;
        }
        for dir in [self.packs_dir(), self.targets_dir()] {
            fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
        }
        let objects = self.root.join("objects");
        let parent = self.root.parent().unwrap_or(Path::new("."));
        for dir in [&loose, &objects, &self.root, parent] {
            self.sync_dir(dir)?;
        }
        self.write_index(&[])
    }

    pub(crate) fn sync_dir(&self, dir: &Path) -> Result<(), StoreError> {
        self.platform
            .sync_dir(dir)
            .map_err(|source| StoreError::io(dir, source))
    }

    fn loose(&self) -> PathBuf {
        self.root.join("objects/loose")
    }

    pub(crate) fn targets_dir(&self) -> PathBuf {
        self.root.join("targets")
    }

    pub(crate) fn index_dir(&self) -> PathBuf {
        self.root.join("index")
    }

    pub(crate) fn path(&self, id: ObjectId) -> PathBuf {
        let hex = id.to_hex();
        let mut path = self.loose();
        path.push(&hex[..2]);
        path.push(&hex[2..]);
        path
    }

    /// Starts a batch of durable writes.
    pub fn batch(&self) -> Batch<'_, P> {
        Batch {
            store: self,
            write: self.durable_write(),
            entries: Mutex::default(),
        }
    }

    /// Starts a durable write of any files in the store.
    pub(crate) fn durable_write(&self) -> DurableWrite<'_, P> {
        DurableWrite::new(&self.platform)
    }

    /// Writes `objects` in one batch, staged from every core, returning how
    /// many were new. Staging compresses and writes without syncing, so it
    /// scales with the cores.
    ///
    /// # Errors
    ///
    /// Fails if an object cannot be written. Objects written before the
    /// failure may not be durable.
    pub fn write_all(&self, objects: &[EncodedObject]) -> Result<usize, StoreError> {
        let batch = self.batch();
        let written = Self::on_every_core(objects, |chunk| batch.write_all(chunk))
            .into_iter()
            .sum::<Result<usize, _>>()?;
        batch.commit()?;
        Ok(written)
    }

    /// Splits `items` into a chunk per core, and maps each on its own
    /// thread, returning each chunk's result in order.
    pub(crate) fn on_every_core<T: Sync, R: Send>(
        items: &[T],
        map: impl Fn(&[T]) -> R + Sync,
    ) -> Vec<R> {
        // Asking for the cores reads the cgroup's quota, which costs more than
        // handling a lone item, such as writing one object.
        if items.len() <= 1 {
            return vec![map(items)];
        }
        let threads = thread::available_parallelism()
            .map_or(1, NonZero::get)
            .min(items.len());
        thread::scope(|scope| {
            items
                .chunks(items.len().div_ceil(threads))
                .map(|chunk| scope.spawn(|| map(chunk)))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| {
                    thread
                        .join()
                        .unwrap_or_else(|panic| panic::resume_unwind(panic))
                })
                .collect()
        })
    }

    /// The id of every object in the store, loose or packed, in no
    /// particular order.
    ///
    /// # Errors
    ///
    /// Fails if the store's directories cannot be listed or its packs
    /// loaded.
    pub fn ids(&self) -> Result<Vec<ObjectId>, StoreError> {
        let loose = self.loose();
        let list = |dir: &Path| {
            fs::read_dir(dir)
                .and_then(Iterator::collect::<io::Result<Vec<_>>>)
                .map_err(|source| StoreError::io(dir, source))
        };
        let mut ids = Vec::new();
        for shard in list(&loose)? {
            let prefix = shard.file_name();
            for object in list(&shard.path())? {
                // Anything else, such as temporary files, isn't an object.
                let mut hex = prefix.clone();
                hex.push(object.file_name());
                ids.extend(hex.to_str().and_then(|hex| hex.parse::<ObjectId>().ok()));
            }
        }
        // Only now: a compaction publishes its pack before it removes the
        // loose files, so one that removed some since they were listed
        // published them in the manifest read here.
        ids.extend(self.packed_ids()?);
        // A compaction in progress, or a writer unaware of a pack, leaves
        // objects both loose and packed.
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    /// Whether the store has the object.
    ///
    /// # Errors
    ///
    /// Fails if the packs cannot be loaded.
    pub fn contains(&self, id: ObjectId) -> Result<bool, StoreError> {
        Ok(self.path(id).exists() || self.find_packed(id)?.is_some())
    }

    /// Reads an object, or `None` if the store does not have it.
    ///
    /// # Errors
    ///
    /// Fails if the object cannot be read or is corrupt.
    pub fn try_read(&self, id: ObjectId) -> Result<Option<EncodedObject>, StoreError> {
        let path = self.path(id);
        let (compressed, path) = match fs::read(&path) {
            Ok(compressed) => (compressed, path),
            // Compaction may have just moved it into a pack.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some((pack, location)) = self.find_packed(id)? else {
                    return Ok(None);
                };
                (pack.read(&self.platform, location)?, pack.path().to_owned())
            }
            Err(source) => return Err(StoreError::io(&path, source)),
        };
        let bytes =
            Self::decompress(&compressed).map_err(|source| StoreError::io(&path, source))?;
        Ok(Some(EncodedObject::verify(id, bytes)?))
    }

    /// Decompresses in one shot using the size recorded in the frame, which
    /// is much faster for small objects than a streaming decoder.
    fn decompress(compressed: &[u8]) -> io::Result<Vec<u8>> {
        let corrupt = || io::Error::new(io::ErrorKind::InvalidData, "missing zstd frame size");
        let size = zstd::zstd_safe::get_frame_content_size(compressed)
            .ok()
            .flatten()
            .ok_or_else(corrupt)?;
        zstd::bulk::decompress(compressed, usize::try_from(size).map_err(|_| corrupt())?)
    }

    /// Reads an object the store must have.
    ///
    /// # Errors
    ///
    /// Fails if the object is missing, cannot be read, or is corrupt.
    pub fn read(&self, id: ObjectId) -> Result<EncodedObject, StoreError> {
        self.try_read(id)?.ok_or(StoreError::NotFound(id))
    }

    /// Reads and decodes the record `id` names.
    ///
    /// # Errors
    ///
    /// Fails like [`Store::read`], or if the object is not a record of the
    /// id's type.
    pub fn get<I: TypedId>(&self, id: I) -> Result<I::Record, StoreError> {
        let id = id.into();
        self.read(id)?
            .decode_as()
            .map_err(|source| StoreError::Decode { id, source })
    }
}

/// Objects written durably as a group. None is durable until
/// [`Batch::commit`] returns, so nothing may refer to them before. Dropping
/// the batch abandons them.
///
/// Objects can be written from several threads at once, and should be: each
/// write compresses its object (see [`Store::write_all`]).
#[derive(Debug)]
pub struct Batch<'a, P> {
    store: &'a Store<P>,
    write: DurableWrite<'a, P>,
    /// What the index keeps of each new record staged.
    entries: Mutex<Vec<Entry>>,
}

impl<P: Platform> Batch<'_, P> {
    /// Stages an object, unless the store already has it, returning whether
    /// it was new. Either way, it is durable once the batch commits. Packs
    /// are only checked as last loaded: an object packed since is staged
    /// again, and the next compaction removes the copy.
    ///
    /// # Errors
    ///
    /// Fails if the object is a record that doesn't decode, or if it cannot
    /// be compressed or written.
    pub fn write(&self, object: &EncodedObject) -> Result<bool, StoreError> {
        let id = object.id();
        let path = self.store.path(id);
        if path.exists() {
            // Its writer may not have synced its directory yet, and the
            // batch's objects may refer to it.
            let mut shard = path;
            shard.pop();
            self.write.sync_dir_with(shard);
            return Ok(false);
        }
        if self.store.packs.find(id).is_some() {
            // The manifest naming its pack may not be durable yet.
            self.write.sync_dir_with(self.store.packs_dir());
            return Ok(false);
        }
        let entry = Entry::of(object).map_err(|source| StoreError::Decode { id, source })?;
        let compressed = zstd::bulk::compress(object.bytes(), Store::<P>::COMPRESSION_LEVEL)
            .map_err(|source| StoreError::io(&path, source))?;
        self.write.stage(path, &compressed)?;
        if let Some(entry) = entry {
            self.entries
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(entry);
        }
        Ok(true)
    }

    /// Stages each of `objects` in turn, returning how many were new.
    fn write_all(&self, objects: &[EncodedObject]) -> Result<usize, StoreError> {
        objects.iter().try_fold(0, |written, object| {
            Ok(written + usize::from(self.write(object)?))
        })
    }

    /// Publishes every object staged, durably, and adds the new records to
    /// the [`Index`](crate::index::Index).
    ///
    /// # Errors
    ///
    /// Fails if an object cannot be renamed into place, a directory cannot
    /// be synced, or the index cannot be appended to.
    pub fn commit(self) -> Result<(), StoreError> {
        let entries = self
            .entries
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        if entries.is_empty() {
            return Ok(self.write.commit()?);
        }
        self.store.commit_indexed(self.write, &entries)
    }
}

/// A failure to read or write the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The store does not have the object.
    #[error("object {0} not found")]
    NotFound(ObjectId),
    /// A file operation failed.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A durable write failed.
    #[error(transparent)]
    Write(#[from] origin_platform::Error),
    /// The store is somewhere origin refuses to write.
    #[error(transparent)]
    Unwritable(#[from] WritableError),
    /// An object's content does not match its id.
    #[error(transparent)]
    Corrupt(#[from] IdMismatch),
    /// An object is not a valid encoding of the expected kind.
    #[error("cannot decode object {id}")]
    Decode {
        /// The object.
        id: ObjectId,
        /// Why decoding failed.
        source: DecodeError,
    },
    /// An acceptance doesn't follow the target's latest one: another landing
    /// got there first.
    #[error(
        "target `{target}` moved: expected acceptance {expected} to follow {}",
        Head::describe(*actual)
    )]
    Conflict {
        /// The target.
        target: TargetName,
        /// The sequence number of the acceptance appended.
        expected: u64,
        /// The target's latest acceptance.
        actual: Option<Head>,
    },
    /// An acceptance was appended to a target it doesn't belong to.
    #[error("the acceptance does not belong to target `{0}`")]
    WrongTarget(TargetName),
    /// A target file isn't in the format the store writes.
    #[error("malformed target file {}", .0.display())]
    Malformed(PathBuf),
}

impl StoreError {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs::File;
    use std::slice;
    use std::sync::Mutex;
    use std::time::SystemTime;

    use origin_platform::{CloneFile, Durability, Error, FileLock, FileSystem, FsKind, ReadAt};

    use origin_shared::{Record, Tree, TreeId};

    use super::*;

    pub(super) fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        (dir, store)
    }

    /// Records syncs, in order, and performs them.
    #[derive(Default)]
    pub(crate) struct Recorder(pub(crate) Mutex<Vec<Synced>>);

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Synced {
        File,
        Batch(usize),
        Dir(PathBuf),
    }

    impl Durability for Recorder {
        fn sync_file(&self, file: &File) -> io::Result<()> {
            self.0.lock().unwrap().push(Synced::File);
            Native::default().sync_file(file)
        }

        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.0.lock().unwrap().push(Synced::Dir(dir.to_owned()));
            Native::default().sync_dir(dir)
        }

        fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error> {
            let files: Vec<_> = files.into_iter().collect();
            self.0.lock().unwrap().push(Synced::Batch(files.len()));
            Native::default().sync_batch(files)
        }
    }

    impl FileSystem for Recorder {
        fn file_system(&self, path: &Path) -> io::Result<FsKind> {
            Native::default().file_system(path)
        }
    }

    impl FileLock for Recorder {}

    impl ReadAt for Recorder {}

    impl CloneFile for Recorder {
        fn cannot_clone(&self, error: &io::Error) -> bool {
            Native::default().cannot_clone(error)
        }
    }

    #[test]
    fn rewriting_an_object_syncs_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Recorder::default());
        store.init().unwrap();
        let blob = EncodedObject::blob(b"hello");
        store.write_all(slice::from_ref(&blob)).unwrap();
        store.platform.0.lock().unwrap().clear();

        // Its writer may not have synced it yet, if another one wrote it.
        assert_eq!(store.write_all(slice::from_ref(&blob)).unwrap(), 0);
        let shard = store.path(blob.id()).parent().unwrap().to_owned();
        assert_eq!(
            store.platform.0.into_inner().unwrap(),
            [Synced::Batch(0), Synced::Dir(shard)]
        );
    }

    #[test]
    fn writes_and_reads_objects() {
        let (_dir, store) = store();
        let blob = EncodedObject::blob(b"hello");

        assert!(!store.contains(blob.id()).unwrap());
        assert!(store.try_read(blob.id()).unwrap().is_none());
        assert_eq!(store.write_all(slice::from_ref(&blob)).unwrap(), 1);
        assert_eq!(
            store.write_all(slice::from_ref(&blob)).unwrap(),
            0,
            "already there"
        );
        assert!(store.contains(blob.id()).unwrap());
        assert_eq!(store.read(blob.id()).unwrap(), blob);
    }

    #[test]
    fn batches_publish_nothing_before_commit() {
        let (_dir, store) = store();
        let objects: Vec<_> = (0..100_u32)
            .map(|i| EncodedObject::blob(&i.to_le_bytes()))
            .collect();

        let batch = store.batch();
        for object in &objects {
            assert!(batch.write(object).unwrap());
        }
        assert!(!store.contains(objects[0].id()).unwrap());
        drop(batch);
        assert!(
            !store.contains(objects[0].id()).unwrap(),
            "dropped batches vanish"
        );

        assert_eq!(store.write_all(&objects).unwrap(), objects.len());
        assert!(
            objects
                .iter()
                .all(|object| store.contains(object.id()).unwrap())
        );
    }

    #[test]
    fn typed_reads_check_the_kind() {
        let (_dir, store) = store();
        let tree = Tree::default().encode();
        let blob = EncodedObject::blob(b"not a tree");
        store.write_all(&[tree.clone(), blob.clone()]).unwrap();

        assert_eq!(store.get(TreeId::from(tree.id())).unwrap(), Tree::default());
        assert!(matches!(
            store.get(TreeId::from(blob.id())),
            Err(StoreError::Decode { .. })
        ));
    }

    #[test]
    fn removes_stale_temporary_files() {
        let (_dir, store) = store();
        let shard = store.loose().join("ab");
        let stale = shard.join(".tmp-1-0");
        let fresh = shard.join(".tmp-1-1");
        let packing = store.packs_dir().join(".tmp-1-2");
        for temp in [&stale, &fresh, &packing] {
            fs::write(temp, b"partial").unwrap();
        }
        let old = SystemTime::now() - Store::<Native>::STALE_TEMP * 2;
        for temp in [&stale, &packing] {
            File::options()
                .write(true)
                .open(temp)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }

        store.remove_stale_temps().unwrap();
        assert!(!stale.exists(), "a crashed write's file is removed");
        assert!(fresh.exists(), "a write in progress keeps its file");
        assert!(packing.exists(), "the compactor's are left to it");
        store.gc().unwrap();
        assert!(!packing.exists());
    }

    #[test]
    fn detects_corruption() {
        let (_dir, store) = store();
        let blob = EncodedObject::blob(b"hello");
        store.write_all(slice::from_ref(&blob)).unwrap();
        let other = zstd::bulk::compress(EncodedObject::blob(b"evil").bytes(), 1).unwrap();
        fs::write(store.path(blob.id()), other).unwrap();

        assert!(matches!(store.read(blob.id()), Err(StoreError::Corrupt(_))));
    }
}
