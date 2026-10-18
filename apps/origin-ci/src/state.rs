//! What `origin-ci` keeps between runs, for each repository it checks, in
//! `$XDG_STATE_HOME/origin-ci/<key>/`, or `~/.local/state/origin-ci/<key>/`,
//! where the key is the start of the hash of the repository's URL or path:
//!
//! ```text
//! worker.lock     held by the one worker of the repository
//! runs/<n>/       a checkout a check runs in, removed after it
//! runs/<n>.lock   held while it runs, so a crashed run's is swept away
//! runs/<n>.held   held by the commands run there while any process of
//!                 theirs lives, even past a crash
//! runs/<n>.group  the process group of the last command started there
//! ```
//!
//! Each command runs in a process group of its own, killed once it ends,
//! with what it left running, or when a crashed run's checkout is swept:
//! so no command of a run outlives it for long, alongside the run that
//! replaces it. Its processes inherit `<n>.held`, open and locked, and a
//! group is only killed while one of its processes still holds it (see
//! [`Processes::group_holds`]): never another group that took its number
//! since, even while a process that left the group (with `setsid`) keeps
//! the lock. Such a process escapes, and so does every leftover where the
//! platform can't tell which files a group holds, as on macOS: leaking a
//! process beats killing someone else's. A command a worker killed with
//! `kill -9` left running still runs until the next sweep: dying with the
//! worker would need code run between `fork` and `exec`, which takes
//! `unsafe`.
//! Being in a group of its own, a command doesn't get the terminal's
//! Ctrl-C either: interrupting `origin ci run` leaves it to the next
//! sweep, or to dying as it writes to the output no one reads anymore.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};

use origin_platform::{LockMode, LockedFile, Native, Processes};
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

/// A snapshot the remote sent: its trees, and the blobs of the files the
/// caller reads, which [`State::checkout`] writes out as many times as
/// checks need.
#[derive(Debug)]
pub struct Snapshot {
    root: TreeId,
    objects: HashMap<ObjectId, EncodedObject>,
}

impl Snapshot {
    /// The snapshot whose root tree is `root`, from `pack`, which holds its
    /// trees and blobs.
    #[must_use]
    pub fn new(root: TreeId, pack: Pack) -> Self {
        let objects = pack
            .objects
            .into_iter()
            .map(|object| (object.id(), object))
            .collect();
        Self { root, objects }
    }

    /// The snapshot's root tree.
    #[must_use]
    pub fn root(&self) -> TreeId {
        self.root
    }

    /// The content of the file `name` at the snapshot's root, unless it has
    /// none, or the remote withholds it.
    ///
    /// # Errors
    ///
    /// Fails if the root tree is missing or malformed.
    pub fn file(&self, name: &str) -> Result<Option<Vec<u8>>, StateError> {
        let file = self
            .tree(self.root.into())?
            .iter()
            .find_map(|(entry, &at)| (entry.as_str() == name).then_some(at)?.file());
        Ok(
            file.and_then(|file| match self.objects.get(&file.id)?.decode() {
                Ok(Object::Blob(content)) => Some(content),
                _ => None,
            }),
        )
    }

    /// Writes the tree `id` into `dir`, which it creates.
    fn write_tree(&self, dir: &Path, id: ObjectId) -> Result<(), StateError> {
        let tree = self.tree(id)?;
        fs::create_dir(dir).map_err(|source| StateError::io(dir, source))?;
        for (name, entry) in &tree {
            let path = dir.join(name.as_str());
            if entry.kind == EntryKind::Tree {
                self.write_tree(&path, entry.id)?;
                continue;
            }
            // Withheld from this caller.
            let Some(Ok(Object::Blob(content))) =
                self.objects.get(&entry.id).map(EncodedObject::decode)
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

    /// The tree `id`.
    fn tree(&self, id: ObjectId) -> Result<Tree, StateError> {
        let object = self.objects.get(&id).ok_or(StateError::MissingTree(id))?;
        object
            .decode_as()
            .map_err(|source| StateError::Decode { id, source })
    }
}

/// A directory holding a snapshot's files, for checks to run in. Removed
/// when dropped.
#[derive(Debug)]
pub struct Checkout {
    dir: PathBuf,
    lock: PathBuf,
    platform: Native,
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

    /// Removes the checkouts no run holds: those of crashed runs, killing
    /// what their commands left running.
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
            if let Some(held) = LockedFile::try_open(&self.platform, &lock, LockMode::Exclusive)? {
                let checkout = Checkout {
                    dir: lock.with_extension(""),
                    lock,
                    platform: self.platform,
                    _held: held,
                };
                if let Some(group) = checkout.group()? {
                    checkout.stop(group)?;
                }
                checkout.remove_all()?;
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

    /// Checks `snapshot` out in a new directory. Files it lacks, which the
    /// remote withholds, are left out.
    ///
    /// # Errors
    ///
    /// Fails if a tree is missing or malformed, or the files cannot be
    /// written.
    pub fn checkout(&self, snapshot: &Snapshot) -> Result<Checkout, StateError> {
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
            platform: self.platform,
            _held: held,
        };
        snapshot.write_tree(&checkout.dir, snapshot.root.into())?;
        Ok(checkout)
    }
}

impl Checkout {
    /// The directory holding the files.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The file locked while a process of the commands run in the
    /// checkout lives.
    fn held(&self) -> PathBuf {
        self.lock.with_extension("held")
    }

    /// The file naming the process group of the last command started.
    fn group_file(&self) -> PathBuf {
        self.lock.with_extension("group")
    }

    /// The process group of the last command started in the checkout, if
    /// it says.
    fn group(&self) -> Result<Option<u32>, StateError> {
        let path = self.group_file();
        match fs::read_to_string(&path) {
            Ok(group) => Ok(group.trim_end().parse().ok()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StateError::io(&path, source)),
        }
    }

    /// Starts `command` in the checkout, in a process group of its own,
    /// which inherits the checkout's `held` file, locked: so its processes
    /// hold the lock while any of them lives (see the module). Commands
    /// must start one at a time, as others would inherit it too.
    ///
    /// # Errors
    ///
    /// Fails if the lock or the group's file cannot be written, or the
    /// command cannot be started.
    pub fn spawn(&self, mut command: Command) -> Result<Child, StateError> {
        let path = self.held();
        let held = LockedFile::open(&self.platform, &path, LockMode::Exclusive)?;
        self.platform
            .inherit(&held)
            .map_err(|source| StateError::io(&path, source))?;
        let group = self.group_file();
        // Never a group that ended, whose number another may take.
        Self::remove(&group)?;
        let child = command
            .current_dir(&self.dir)
            .process_group(0)
            .spawn()
            .map_err(|source| StateError::io(&self.dir, source))?;
        // Only the command's processes hold the lock now.
        drop(held);
        fs::write(&group, format!("{}\n", child.id()))
            .map_err(|source| StateError::io(&group, source))?;
        Ok(child)
    }

    /// Kills what is left of the process group `group` of a command
    /// started in the checkout, if any of it still holds the lock: so
    /// nothing it started outlives it, and no other group is killed (see
    /// the module).
    ///
    /// # Errors
    ///
    /// Fails if the lock cannot be tried, the processes listed, or the
    /// group killed.
    pub fn stop(&self, group: u32) -> Result<(), StateError> {
        let held = self.held();
        // Free: nothing the command started lives, which is the common
        // case, known without listing processes.
        if !held.exists()
            || LockedFile::try_open(&self.platform, &held, LockMode::Exclusive)?.is_some()
        {
            return Ok(());
        }
        let io = |source| StateError::io(&held, source);
        if self.platform.group_holds(group, &held).map_err(io)? {
            self.platform.kill_group(group).map_err(io)?;
        }
        Ok(())
    }

    /// Removes the checkout's directory and files, the lock last.
    fn remove_all(&self) -> Result<(), StateError> {
        for path in [&self.dir, &self.held(), &self.group_file(), &self.lock] {
            Self::remove(path)?;
        }
        Ok(())
    }

    /// Removes a checkout's directory, or one of its files, if it exists.
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
        let _ = self.remove_all();
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

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use origin_shared::Record;

    use super::*;

    /// A command's group ended, but a process it started in a session of
    /// its own still holds the lock; the group's number now names another
    /// group, which stopping the command never kills.
    #[test]
    fn stopping_a_command_never_kills_a_group_that_took_its_number() {
        let base = tempfile::tempdir().unwrap();
        let url = RemoteUrl::parse(base.path().join("repo").to_str().unwrap()).unwrap();
        let state = State::new(base.path(), &url).unwrap();
        let root = Tree::default().encode();
        let snapshot = Snapshot::new(
            root.id().into(),
            Pack {
                objects: vec![root],
            },
        );
        let checkout = state.checkout(&snapshot).unwrap();
        let mut escaped = Command::new("sleep");
        escaped.arg("60");
        let mut escaped = checkout.spawn(escaped).unwrap();
        let mut other = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        checkout.stop(other.id()).unwrap();
        // Time for a kill to land.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            other.try_wait().unwrap().is_none(),
            "another group was killed"
        );
        for child in [&mut escaped, &mut other] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}
