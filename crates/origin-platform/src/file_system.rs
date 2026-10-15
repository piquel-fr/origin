use std::io;
use std::path::{Path, PathBuf};

use crate::{Error, Op};

/// Tells local file systems from network ones, which origin refuses to
/// write to: they lose locks and reorder or drop syncs, so a repository on
/// one can be corrupted by a crash or a concurrent writer.
///
/// Sync services such as Dropbox and iCloud Drive keep their folders on a
/// local file system and replace files behind origin's back. Nothing in
/// `statfs` tells them apart, and origin doesn't guess from paths, so a
/// repository there is accepted.
pub trait FileSystem {
    /// The kind of file system holding `path`, which must exist.
    ///
    /// # Errors
    ///
    /// Fails if `path` cannot be inspected.
    fn file_system(&self, path: &Path) -> io::Result<FsKind>;

    /// Checks that the repository at `path` may be opened for writing:
    /// refuses network file systems, and accepts the ones the platform
    /// cannot identify.
    ///
    /// # Errors
    ///
    /// Fails with [`WritableError::Network`] on a network file system, and
    /// [`WritableError::Io`] if `path` cannot be inspected.
    fn check_writable(&self, path: &Path) -> Result<(), WritableError> {
        let kind = self
            .file_system(path)
            .map_err(|source| Error::new(Op::Inspect, path, source))?;
        match kind {
            FsKind::Network(name) => Err(WritableError::Network {
                path: path.to_owned(),
                name,
            }),
            FsKind::Local | FsKind::Unknown(_) => Ok(()),
        }
    }
}

/// What a file system is, as far as the platform can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    /// A disk attached to this machine.
    Local,
    /// A file system served by another machine, by name, such as `NFS`.
    Network(&'static str),
    /// A file system that could be either, such as FUSE, which serves
    /// sshfs as well as local disks.
    Unknown(&'static str),
}

/// A repository that cannot be opened for writing.
#[derive(Debug, thiserror::Error)]
pub enum WritableError {
    /// The repository is on a network file system.
    #[error(
        "{} is on {name}, a network file system; origin writes only to local \
         disks, so clone the repository to one and use this one as a remote",
        path.display()
    )]
    Network {
        /// The repository.
        path: PathBuf,
        /// The file system's name.
        name: &'static str,
    },
    /// The file system holding the repository cannot be inspected.
    #[error(transparent)]
    Io(#[from] Error),
}
