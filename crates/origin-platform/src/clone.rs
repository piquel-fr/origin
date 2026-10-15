use std::io;
use std::path::{Path, PathBuf};

/// Copy-on-write clones: a new file that shares the original's storage until
/// either is modified, made in constant time whatever its size.
///
/// Only some file systems can clone, so callers fall back to copying on
/// [`CloneError::Unsupported`].
pub trait CloneFile {
    /// Whether `error`, from a failed clone, means the file system cannot
    /// clone these files, rather than that the files are unusable. Each
    /// platform reports this differently.
    fn cannot_clone(&self, error: &io::Error) -> bool;

    /// Clones `from` to `to`, which must not exist.
    ///
    /// # Errors
    ///
    /// Fails with [`CloneError::Unsupported`] if the file system cannot
    /// clone these files, and [`CloneError::Io`] if `from` cannot be read or
    /// `to` cannot be created.
    fn clone_file(&self, from: &Path, to: &Path) -> Result<(), CloneError> {
        reflink_copy::reflink(from, to).map_err(|source| {
            // reflink-copy reports a source that isn't a regular file as
            // `InvalidInput`, which can also mean the file system can't clone.
            let unsupported = self.cannot_clone(&source) && from.is_file();
            let (from, to) = (from.to_owned(), to.to_owned());
            if unsupported {
                CloneError::Unsupported { from, to, source }
            } else {
                CloneError::Io { from, to, source }
            }
        })
    }
}

/// A clone that failed.
#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    /// The file system cannot clone these files. Copying them works instead.
    #[error("the file system cannot clone {} to {}", from.display(), to.display())]
    Unsupported {
        /// The file to clone.
        from: PathBuf,
        /// The clone's path.
        to: PathBuf,
        /// The error the platform reported.
        source: io::Error,
    },
    /// The source cannot be read or the clone cannot be created.
    #[error("cannot clone {} to {}", from.display(), to.display())]
    Io {
        /// The file to clone.
        from: PathBuf,
        /// The clone's path.
        to: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}
