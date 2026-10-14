//! Branch storage with atomic, multi-branch transactions.
//!
//! All branches live in one file of `<hex id> <branch>` lines. A transaction
//! takes an exclusive lock by creating `<file>.lock`, checks every expected
//! value, writes the new state into the lock file, and renames it over the
//! original. Readers always see either the old state or the new one, and the
//! new state is on disk before the transaction returns.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use origin_shared::{BranchName, ObjectId, RefUpdate};

/// The branches of a repository, mapped to the commits they point to.
pub type Branches = BTreeMap<BranchName, ObjectId>;

/// A file of branches.
#[derive(Clone, Debug)]
pub struct RefStore {
    path: PathBuf,
}

impl RefStore {
    /// How long a transaction waits for another one to release the lock.
    const LOCK_RETRIES: u32 = 200;
    const LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);

    /// Opens the branches stored in `path`. A missing file has no branches.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Lists every branch.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is malformed.
    pub fn list(&self) -> Result<Branches, RefError> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Branches::new()),
            Err(source) => return Err(RefError::io(&self.path, source)),
        };
        content
            .lines()
            .enumerate()
            .map(|(index, line)| {
                Self::parse_line(line).ok_or_else(|| RefError::Malformed {
                    path: self.path.clone(),
                    line: index + 1,
                })
            })
            .collect()
    }

    fn parse_line(line: &str) -> Option<(BranchName, ObjectId)> {
        let (id, branch) = line.split_once(' ')?;
        Some((BranchName::new(branch).ok()?, id.parse().ok()?))
    }

    /// Looks up one branch.
    ///
    /// # Errors
    ///
    /// Fails like [`RefStore::list`].
    pub fn get(&self, branch: &BranchName) -> Result<Option<ObjectId>, RefError> {
        Ok(self.list()?.get(branch).copied())
    }

    /// Applies every update, or none of them if any expected value does not
    /// match.
    ///
    /// # Errors
    ///
    /// Fails with [`RefError::Conflict`] if a branch is not where an update
    /// expects it, or if the file cannot be locked, read, or written.
    pub fn update(&self, updates: &[RefUpdate]) -> Result<(), RefError> {
        let lock = Lock::acquire(&self.path)?;
        let mut branches = self.list()?;
        for update in updates {
            let actual = branches.get(&update.branch).copied();
            if actual != update.expected {
                return Err(RefError::Conflict {
                    branch: update.branch.clone(),
                    expected: update.expected,
                    actual,
                });
            }
            match update.new {
                Some(id) => branches.insert(update.branch.clone(), id),
                None => branches.remove(&update.branch),
            };
        }
        let mut content = String::new();
        for (branch, id) in &branches {
            writeln!(content, "{id} {branch}").expect("writing to a String cannot fail");
        }
        lock.commit(content.as_bytes())
    }
}

/// An exclusive lock on a ref file, held by creating `<file>.lock`. Dropping
/// it without committing releases it.
struct Lock<'a> {
    target: &'a Path,
    path: PathBuf,
    file: File,
    committed: bool,
}

impl<'a> Lock<'a> {
    fn acquire(target: &'a Path) -> Result<Self, RefError> {
        let path = target.with_extension("lock");
        for _ in 0..RefStore::LOCK_RETRIES {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        target,
                        path,
                        file,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    thread::sleep(RefStore::LOCK_RETRY_DELAY);
                }
                Err(source) => return Err(RefError::io(&path, source)),
            }
        }
        Err(RefError::Locked(path))
    }

    /// Durably replaces the target with `content` and releases the lock.
    fn commit(mut self, content: &[u8]) -> Result<(), RefError> {
        self.file
            .write_all(content)
            .and_then(|()| self.file.sync_all())
            .map_err(|source| RefError::io(&self.path, source))?;
        fs::rename(&self.path, self.target).map_err(|source| RefError::io(self.target, source))?;
        self.committed = true;
        // The rename itself is only durable once the directory is synced.
        let dir = match self.target.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        File::open(dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|source| RefError::io(dir, source))
    }
}

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        // After a commit the lock file became the target, and the lock path
        // may already belong to the next transaction.
        if !self.committed {
            // Nothing to do if removal fails: the error that dropped the lock
            // is already being reported, and `Locked` names the stale file.
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A failure to read or update branches.
#[derive(Debug, thiserror::Error)]
pub enum RefError {
    /// A branch is not where an update expected it.
    #[error(
        "branch `{branch}` moved: expected {}, found {}",
        describe(*expected),
        describe(*actual)
    )]
    Conflict {
        /// The branch.
        branch: BranchName,
        /// Where the update expected it.
        expected: Option<ObjectId>,
        /// Where it actually is.
        actual: Option<ObjectId>,
    },
    /// Another transaction held the lock for too long.
    #[error("branches are locked by another process; remove {} if none is running", .0.display())]
    Locked(PathBuf),
    /// The file has a line that is not `<hex id> <branch>`.
    #[error("malformed branch file {} at line {line}", path.display())]
    Malformed {
        /// The branch file.
        path: PathBuf,
        /// The 1-based line number.
        line: usize,
    },
    /// A filesystem operation failed.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

impl RefError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

fn describe(id: Option<ObjectId>) -> String {
    id.map_or_else(|| "no branch".to_owned(), |id| id.short())
}

#[cfg(test)]
mod tests {
    use origin_shared::EncodedObject;

    use super::*;

    fn branch(name: &str) -> BranchName {
        BranchName::new(name).unwrap()
    }

    fn id(seed: &str) -> ObjectId {
        EncodedObject::blob(seed.as_bytes()).id()
    }

    fn set(name: &str, expected: Option<ObjectId>, new: Option<ObjectId>) -> RefUpdate {
        RefUpdate {
            branch: branch(name),
            expected,
            new,
        }
    }

    #[test]
    fn creates_moves_and_deletes_branches() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::new(dir.path().join("refs"));
        assert!(refs.list().unwrap().is_empty());

        refs.update(&[set("main", None, Some(id("a")))]).unwrap();
        assert_eq!(refs.get(&branch("main")).unwrap(), Some(id("a")));

        refs.update(&[set("main", Some(id("a")), Some(id("b")))])
            .unwrap();
        assert_eq!(refs.get(&branch("main")).unwrap(), Some(id("b")));

        refs.update(&[set("main", Some(id("b")), None)]).unwrap();
        assert!(refs.list().unwrap().is_empty());
    }

    #[test]
    fn transactions_are_all_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::new(dir.path().join("refs"));
        refs.update(&[set("main", None, Some(id("a")))]).unwrap();

        let error = refs
            .update(&[
                set("feature", None, Some(id("f"))),
                set("main", Some(id("stale")), Some(id("b"))),
            ])
            .unwrap_err();
        assert!(matches!(error, RefError::Conflict { .. }), "{error}");
        assert_eq!(
            refs.list().unwrap(),
            Branches::from([(branch("main"), id("a"))]),
            "the valid update was not applied either"
        );
        assert!(
            !dir.path().join("refs.lock").exists(),
            "the lock is released"
        );
    }

    #[test]
    fn relative_ref_files_work() {
        let dir = tempfile::tempdir().unwrap();
        // Safe because nextest runs each test in its own process.
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = RefStore::new("refs").update(&[set("main", None, Some(id("a")))]);
        std::env::set_current_dir(previous).unwrap();
        result.unwrap();
    }

    #[test]
    fn concurrent_transactions_serialize() {
        let dir = tempfile::tempdir().unwrap();
        let refs = RefStore::new(dir.path().join("refs"));
        thread::scope(|scope| {
            for thread in 0..8 {
                let refs = &refs;
                scope.spawn(move || {
                    refs.update(&[set(&format!("t{thread}"), None, Some(id("x")))])
                        .unwrap();
                });
            }
        });
        assert_eq!(refs.list().unwrap().len(), 8);
    }
}
