//! What `origin-ci` keeps between runs, for each repository it checks, in
//! `$XDG_STATE_HOME/origin-ci/<key>/`, or `~/.local/state/origin-ci/<key>/`,
//! where the key is the start of the hash of the repository's URL or path:
//!
//! ```text
//! cursor          the event cursor a worker resumes after, durable
//! worker.lock     held by the one worker of the repository
//! runs/<n>/       a checkout a check runs in, removed after it
//! runs/<n>.lock   held while it runs, so a crashed run's is swept away
//! ```

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use origin_platform::{DurableWrite, LockMode, LockedFile, Native};
use origin_shared::object::DecodeError;
use origin_shared::pack::Pack;
use origin_shared::{Digest, EncodedObject, EntryKind, Object, ObjectId, Tree, TreeId};
use origin_src::remote::RemoteUrl;

/// The state kept for one repository.
#[derive(Debug)]
pub struct State {
    dir: PathBuf,
    platform: Native,
}

/// A directory holding a snapshot's files, for checks to run in. Removed
/// when dropped.
#[derive(Debug)]
pub struct Checkout {
    dir: PathBuf,
    lock: PathBuf,
    _held: LockedFile,
}

/// Distinguishes the checkouts of one process.
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

impl State {
    /// How many hex digits of the hash of a repository's URL name its
    /// directory.
    const KEY_LEN: usize = 16;

    /// The state of the repository at `url`, in the user's state
    /// directory, sweeping away the checkouts of crashed runs.
    ///
    /// # Errors
    ///
    /// Fails if there's no state directory, or like [`State::new`].
    pub fn open(url: &RemoteUrl) -> Result<Self, StateError> {
        let base = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))
            .ok_or(StateError::NoDir)?;
        Self::new(&base.join("origin-ci"), url)
    }

    /// The state of the repository at `url`, in `base`, sweeping away the
    /// checkouts of crashed runs.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created or swept.
    pub fn new(base: &Path, url: &RemoteUrl) -> Result<Self, StateError> {
        let mut key = Digest::of(url.to_string().as_bytes()).to_hex();
        key.truncate(Self::KEY_LEN);
        let state = Self {
            dir: base.join(key),
            platform: Native::default(),
        };
        let runs = state.runs();
        fs::create_dir_all(&runs).map_err(|source| StateError::io(&runs, source))?;
        state.sweep()?;
        Ok(state)
    }

    /// The directory of the repository's state.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn runs(&self) -> PathBuf {
        self.dir.join("runs")
    }

    fn cursor_path(&self) -> PathBuf {
        self.dir.join("cursor")
    }

    /// Removes the checkouts no run holds: those of crashed runs.
    fn sweep(&self) -> Result<(), StateError> {
        let runs = self.runs();
        let entries = fs::read_dir(&runs).map_err(|source| StateError::io(&runs, source))?;
        for entry in entries {
            let lock = entry
                .map_err(|source| StateError::io(&runs, source))?
                .path();
            if lock.extension().is_none_or(|extension| extension != "lock") {
                continue;
            }
            if let Some(_held) = LockedFile::try_open(&self.platform, &lock, LockMode::Exclusive)? {
                Checkout::remove(&lock.with_extension(""))?;
                Checkout::remove(&lock)?;
            }
        }
        Ok(())
    }

    /// Takes the repository's worker lock, so a single worker answers its
    /// events, or returns `None` if another worker holds it.
    ///
    /// # Errors
    ///
    /// Fails if the lock file cannot be opened.
    pub fn lock_worker(&self) -> Result<Option<LockedFile>, StateError> {
        let path = self.dir.join("worker.lock");
        Ok(LockedFile::try_open(
            &self.platform,
            &path,
            LockMode::Exclusive,
        )?)
    }

    /// The cursor a worker last saved, if it did.
    ///
    /// # Errors
    ///
    /// Fails if the cursor cannot be read or is malformed.
    pub fn cursor(&self) -> Result<Option<u64>, StateError> {
        let path = self.cursor_path();
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                text.trim_end()
                    .parse()
                    .map_err(|_| StateError::Malformed(path))?,
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StateError::io(&path, source)),
        }
    }

    /// Saves the cursor a worker resumes after, durably.
    ///
    /// # Errors
    ///
    /// Fails if it cannot be written.
    pub fn save_cursor(&self, cursor: u64) -> Result<(), StateError> {
        let write = DurableWrite::new(&self.platform);
        write.stage(self.cursor_path(), format!("{cursor}\n").as_bytes())?;
        Ok(write.commit()?)
    }

    /// Checks out the snapshot `root` from `pack`, which holds its trees and
    /// blobs, in a new directory. Files the pack lacks, which the remote
    /// withholds, are left out.
    ///
    /// # Errors
    ///
    /// Fails if a tree is missing or malformed, or the files cannot be
    /// written.
    pub fn checkout(&self, pack: &Pack, root: TreeId) -> Result<Checkout, StateError> {
        let name = format!(
            "{}-{}",
            process::id(),
            NEXT_RUN.fetch_add(1, Ordering::Relaxed)
        );
        let dir = self.runs().join(&name);
        let lock = self.runs().join(format!("{name}.lock"));
        let held = LockedFile::open(&self.platform, &lock, LockMode::Exclusive)?;
        // A crashed process with the same id may have left it.
        Checkout::remove(&dir)?;
        let checkout = Checkout {
            dir,
            lock,
            _held: held,
        };
        let objects: HashMap<_, _> = pack
            .objects
            .iter()
            .map(|object| (object.id(), object))
            .collect();
        Checkout::write_tree(&objects, &checkout.dir, root.into())?;
        Ok(checkout)
    }
}

impl Checkout {
    /// The directory holding the files.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Writes the tree `id` into `dir`, which it creates, from `objects`.
    fn write_tree(
        objects: &HashMap<ObjectId, &EncodedObject>,
        dir: &Path,
        id: ObjectId,
    ) -> Result<(), StateError> {
        let object = objects.get(&id).ok_or(StateError::MissingTree(id))?;
        let tree: Tree = object
            .decode_as()
            .map_err(|source| StateError::Decode { id, source })?;
        fs::create_dir(dir).map_err(|source| StateError::io(dir, source))?;
        for (name, entry) in &tree {
            let path = dir.join(name.as_str());
            if entry.kind == EntryKind::Tree {
                Self::write_tree(objects, &path, entry.id)?;
                continue;
            }
            // Withheld from this caller.
            let Some(Ok(Object::Blob(content))) = objects.get(&entry.id).map(|blob| blob.decode())
            else {
                continue;
            };
            let written = match entry.kind {
                EntryKind::Symlink => {
                    let target = String::from_utf8_lossy(&content).into_owned();
                    std::os::unix::fs::symlink(target, &path)
                }
                EntryKind::Executable => fs::write(&path, &content)
                    .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(0o755))),
                EntryKind::File | EntryKind::Tree => fs::write(&path, &content),
            };
            written.map_err(|source| StateError::io(&path, source))?;
        }
        Ok(())
    }

    /// Removes a checkout's directory, or its lock file, if it exists.
    fn remove(path: &Path) -> Result<(), StateError> {
        let removed = if path.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        match removed {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(StateError::io(path, error))
            }
            _ => Ok(()),
        }
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        // Best effort: the next run sweeps what's left.
        let _ = Self::remove(&self.dir);
        let _ = Self::remove(&self.lock);
    }
}

/// A failure to keep state.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Neither `$XDG_STATE_HOME` nor `$HOME` is set.
    #[error("no state directory: set $XDG_STATE_HOME or $HOME")]
    NoDir,
    /// A file cannot be read or written.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        source: io::Error,
    },
    /// A file holds something unexpected.
    #[error("{} is malformed", .0.display())]
    Malformed(PathBuf),
    /// A snapshot's tree is missing from what the remote sent.
    #[error("the remote sent no tree {0}")]
    MissingTree(ObjectId),
    /// A tree the remote sent is malformed.
    #[error("tree {id} is malformed")]
    Decode {
        /// The tree.
        id: ObjectId,
        /// What is wrong.
        source: DecodeError,
    },
    /// A lock or durable write failed.
    #[error(transparent)]
    Platform(#[from] origin_platform::Error),
}

impl StateError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}
