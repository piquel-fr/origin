use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::ops::Deref;
use std::path::Path;

use crate::{Error, Op};

/// Advisory locks on whole files.
///
/// A lock belongs to an open file and is released when that file is closed,
/// which the kernel also does when the process dies, so a crash never leaves
/// a stale lock behind. Two opens of the same path conflict even within one
/// process. The default methods use the standard library's locks.
pub trait FileLock {
    /// Blocks until `file` is locked in `mode`.
    ///
    /// # Errors
    ///
    /// Fails if the file system cannot lock the file.
    fn lock(&self, file: &File, mode: LockMode) -> io::Result<()> {
        match mode {
            LockMode::Shared => file.lock_shared(),
            LockMode::Exclusive => file.lock(),
        }
    }

    /// Locks `file` in `mode` unless another holder conflicts, returning
    /// whether it did.
    ///
    /// # Errors
    ///
    /// Fails if the file system cannot lock the file.
    fn try_lock(&self, file: &File, mode: LockMode) -> io::Result<bool> {
        let result = match mode {
            LockMode::Shared => file.try_lock_shared(),
            LockMode::Exclusive => file.try_lock(),
        };
        match result {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

/// How a lock is shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    /// Any number of shared holders, and no exclusive one.
    Shared,
    /// A single holder.
    Exclusive,
}

/// A file opened and locked, until it's dropped.
///
/// Lock a file that is never replaced: renaming another file over the path
/// leaves the lock on the old file, so later openers would not conflict.
#[derive(Debug)]
pub struct LockedFile(File);

impl LockedFile {
    /// Opens `path` for reading and writing, creating it if needed, and
    /// blocks until it's locked in `mode`.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be opened or locked.
    pub fn open(platform: &impl FileLock, path: &Path, mode: LockMode) -> Result<Self, Error> {
        let file = Self::open_file(path)?;
        platform
            .lock(&file, mode)
            .map_err(|source| Error::new(Op::Lock, path, source))?;
        Ok(Self(file))
    }

    /// Opens `path` like [`LockedFile::open`], but returns `None` instead of
    /// waiting when another holder conflicts.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be opened or locked.
    pub fn try_open(
        platform: &impl FileLock,
        path: &Path,
        mode: LockMode,
    ) -> Result<Option<Self>, Error> {
        let file = Self::open_file(path)?;
        let locked = platform
            .try_lock(&file, mode)
            .map_err(|source| Error::new(Op::Lock, path, source))?;
        Ok(locked.then_some(Self(file)))
    }

    fn open_file(path: &Path) -> Result<File, Error> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| Error::new(Op::Open, path, source))
    }
}

impl Deref for LockedFile {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}
