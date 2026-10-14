//! Content-addressed object storage on disk.
//!
//! Each object lives in its own zstd-compressed file at
//! `<dir>/<first 2 hex digits>/<remaining hex digits>`. Files are written to a
//! temporary name and renamed into place, so readers never see partial
//! objects and concurrent writers of the same object are harmless.
//!
//! Writes are not synced to disk: syncing every loose object would dominate
//! snapshot and push times. Durable batches will come with packed storage;
//! until then a crash can lose recent objects that branches refer to.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use origin_shared::object::{DecodeError, IdMismatch};
use origin_shared::{Commit, EncodedObject, ObjectId, Tree};

/// A directory of objects.
#[derive(Clone, Debug)]
pub struct ObjectStore {
    dir: PathBuf,
}

/// Distinguishes temporary files written concurrently by one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ObjectStore {
    /// Favors speed: higher levels shrink source code little and cost a lot
    /// on every snapshot.
    const COMPRESSION_LEVEL: i32 = 1;

    /// Opens the store in `dir`, which is created on first write.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The shard directory of an object and its file within it.
    fn location(&self, id: ObjectId) -> (PathBuf, PathBuf) {
        let hex = id.to_hex();
        let shard = self.dir.join(&hex[..2]);
        let path = shard.join(&hex[2..]);
        (shard, path)
    }

    fn path(&self, id: ObjectId) -> PathBuf {
        self.location(id).1
    }

    /// Whether the store has the object.
    #[must_use]
    pub fn contains(&self, id: ObjectId) -> bool {
        self.path(id).exists()
    }

    /// Stores an object, returning whether it was new.
    ///
    /// # Errors
    ///
    /// Fails if the object cannot be written.
    pub fn write(&self, object: &EncodedObject) -> Result<bool, StoreError> {
        let (shard, path) = self.location(object.id());
        if path.exists() {
            return Ok(false);
        }
        fs::create_dir_all(&shard).map_err(|source| StoreError::io(&shard, source))?;
        let compressed = zstd::bulk::compress(object.bytes(), Self::COMPRESSION_LEVEL)
            .map_err(|source| StoreError::io(&path, source))?;
        let temp = shard.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&temp, compressed).map_err(|source| StoreError::io(&temp, source))?;
        fs::rename(&temp, &path).map_err(|source| StoreError::io(&path, source))?;
        Ok(true)
    }

    /// Reads an object, or `None` if the store does not have it.
    ///
    /// # Errors
    ///
    /// Fails if the object cannot be read or is corrupt.
    pub fn try_read(&self, id: ObjectId) -> Result<Option<EncodedObject>, StoreError> {
        let path = self.path(id);
        let compressed = match fs::read(&path) {
            Ok(compressed) => compressed,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
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

    /// Reads and decodes a tree.
    ///
    /// # Errors
    ///
    /// Fails like [`ObjectStore::read`], or if the object is not a tree.
    pub fn tree(&self, id: ObjectId) -> Result<Tree, StoreError> {
        self.read(id)?
            .decode_tree()
            .map_err(|source| StoreError::Decode { id, source })
    }

    /// Reads and decodes a commit.
    ///
    /// # Errors
    ///
    /// Fails like [`ObjectStore::read`], or if the object is not a commit.
    pub fn commit(&self, id: ObjectId) -> Result<Commit, StoreError> {
        self.read(id)?
            .decode_commit()
            .map_err(|source| StoreError::Decode { id, source })
    }
}

/// A failure to read or write objects.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The store does not have the object.
    #[error("object {0} not found")]
    NotFound(ObjectId),
    /// A filesystem operation failed.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
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
}

impl StoreError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use origin_shared::Object;

    use super::*;

    #[test]
    fn writes_and_reads_objects() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let blob = EncodedObject::blob(b"hello");

        assert!(!store.contains(blob.id()));
        assert!(store.try_read(blob.id()).unwrap().is_none());
        assert!(store.write(&blob).unwrap());
        assert!(!store.write(&blob).unwrap(), "second write is a no-op");
        assert!(store.contains(blob.id()));
        assert_eq!(store.read(blob.id()).unwrap(), blob);
    }

    #[test]
    fn typed_reads_check_the_kind() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let tree = Object::Tree(Tree::default()).encode();
        store.write(&tree).unwrap();

        assert_eq!(store.tree(tree.id()).unwrap(), Tree::default());
        assert!(matches!(
            store.commit(tree.id()),
            Err(StoreError::Decode { .. })
        ));
    }

    #[test]
    fn detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let blob = EncodedObject::blob(b"hello");
        store.write(&blob).unwrap();
        let other = zstd::bulk::compress(EncodedObject::blob(b"evil").bytes(), 1).unwrap();
        fs::write(store.path(blob.id()), other).unwrap();

        assert!(matches!(store.read(blob.id()), Err(StoreError::Corrupt(_))));
    }
}
