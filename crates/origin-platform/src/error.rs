use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// A file operation that failed, and the path it failed on.
#[derive(Debug, thiserror::Error)]
#[error("cannot {op} {}", path.display())]
pub struct Error {
    /// What was being done.
    pub op: Op,
    /// The file or directory involved.
    pub path: PathBuf,
    /// The underlying error.
    pub source: io::Error,
}

impl Error {
    pub(crate) fn new(op: Op, path: &Path, source: io::Error) -> Self {
        Self {
            op,
            path: path.to_owned(),
            source,
        }
    }
}

/// The operations an [`Error`] can come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Creating a directory.
    CreateDir,
    /// Opening a file.
    Open,
    /// Writing a temporary file.
    Write,
    /// Syncing a file or directory to stable storage.
    Sync,
    /// Renaming a file into place.
    Publish,
    /// Locking a file.
    Lock,
    /// Finding out which file system holds a path.
    Inspect,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CreateDir => "create directory",
            Self::Open => "open",
            Self::Write => "write",
            Self::Sync => "sync",
            Self::Publish => "publish",
            Self::Lock => "lock",
            Self::Inspect => "inspect the file system of",
        })
    }
}
