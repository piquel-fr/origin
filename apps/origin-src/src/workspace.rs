//! A working copy and its local repository in `.origin/`.
//!
//! ```text
//! .origin/
//!   config.toml   the checked-out branch
//!   branches      local branches (see `RefStore`)
//!   objects/      local objects (see `ObjectStore`)
//!   state         the stat cache of the last snapshot
//! ```

use std::env;
use std::fs;
use std::io;
use std::path::{self, Path, PathBuf};

use origin_shared::{BranchName, Commit, Object, ObjectId, RefUpdate, Signature};
use serde::{Deserialize, Serialize};

use crate::output::FileDiff;
use crate::refs::{RefError, RefStore};
use crate::store::{ObjectStore, StoreError};
use crate::tree::Change;
use crate::worktree::{Snapshot, SnapshotError, StatCache};

/// A working copy and its local repository.
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    meta: PathBuf,
    objects: ObjectStore,
    branches: RefStore,
    config: Config,
}

/// `.origin/config.toml`.
#[derive(Debug, Serialize, Deserialize)]
struct Config {
    /// The checked-out branch. It may not exist yet in a new repository.
    branch: BranchName,
}

impl Workspace {
    /// The metadata directory at the root of every working copy.
    pub const META_DIR: &str = ".origin";

    /// `origin init`: creates a repository in `root` with an empty `main`
    /// branch.
    ///
    /// # Errors
    ///
    /// Fails if `root` already has a repository or cannot be written.
    pub fn init(root: &Path) -> Result<(), WorkspaceError> {
        let root = path::absolute(root).map_err(|source| WorkspaceError::io(root, source))?;
        let meta = root.join(Self::META_DIR);
        if meta.exists() {
            return Err(WorkspaceError::AlreadyInitialized(root));
        }
        fs::create_dir_all(&meta).map_err(|source| WorkspaceError::io(&meta, source))?;
        let config = Config {
            branch: BranchName::default(),
        };
        Self::with_config(root.clone(), config).save_config()?;
        println!("Initialized an empty repository in {}", root.display());
        Ok(())
    }

    /// Opens the working copy containing `start`.
    ///
    /// # Errors
    ///
    /// Fails if neither `start` nor any parent has a repository, or its
    /// configuration cannot be read.
    pub fn discover(start: &Path) -> Result<Self, WorkspaceError> {
        let root = start
            .ancestors()
            .find(|dir| dir.join(Self::META_DIR).is_dir())
            .ok_or_else(|| WorkspaceError::NotFound(start.to_owned()))?;
        let path = root.join(Self::META_DIR).join("config.toml");
        let content =
            fs::read_to_string(&path).map_err(|source| WorkspaceError::io(&path, source))?;
        let config =
            toml::from_str(&content).map_err(|source| WorkspaceError::Config { path, source })?;
        Ok(Self::with_config(root.to_owned(), config))
    }

    fn with_config(root: PathBuf, config: Config) -> Self {
        let meta = root.join(Self::META_DIR);
        Self {
            objects: ObjectStore::new(meta.join("objects")),
            branches: RefStore::new(meta.join("branches")),
            root,
            meta,
            config,
        }
    }

    fn save_config(&self) -> Result<(), WorkspaceError> {
        let path = self.meta.join("config.toml");
        let content = toml::to_string(&self.config).expect("the config serializes to TOML");
        fs::write(&path, content).map_err(|source| WorkspaceError::io(&path, source))
    }

    /// The root of the working copy.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The local objects.
    #[must_use]
    pub fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    /// The checked-out branch.
    #[must_use]
    pub fn branch(&self) -> &BranchName {
        &self.config.branch
    }

    /// The commit the checked-out branch points to, if it has any.
    ///
    /// # Errors
    ///
    /// Fails if the branches cannot be read.
    pub fn head(&self) -> Result<Option<ObjectId>, WorkspaceError> {
        Ok(self.branches.get(&self.config.branch)?)
    }

    /// Returns the root tree of `head`
    fn head_tree(&self, head: Option<ObjectId>) -> Result<Option<ObjectId>, WorkspaceError> {
        Ok(head
            .map(|id| self.objects.commit(id))
            .transpose()?
            .map(|commit| commit.tree))
    }

    /// Records the working copy into the store, returning its root tree.
    ///
    /// # Errors
    ///
    /// Fails if the working copy cannot be read or stored.
    pub fn snapshot(&self) -> Result<ObjectId, WorkspaceError> {
        let path = self.meta.join("state");
        let (tree, cache) =
            Snapshot::new(&self.root, &self.objects).take(&StatCache::load(&path))?;
        if let Some(cache) = cache {
            cache
                .save(&path)
                .map_err(|source| WorkspaceError::io(&path, source))?;
        }
        Ok(tree)
    }

    /// Snapshots the working copy and lists how it differs from the head.
    ///
    /// # Errors
    ///
    /// Fails like [`Workspace::snapshot`], or if the head cannot be read.
    pub fn changes(&self) -> Result<Vec<Change>, WorkspaceError> {
        let tree = self.snapshot()?;
        let head_tree = self.head_tree(self.head()?)?;
        Ok(self.objects.diff(head_tree, Some(tree))?)
    }

    /// `origin status`: prints the branch and the changed paths.
    ///
    /// # Errors
    ///
    /// Fails like [`Workspace::changes`].
    pub fn status(&self) -> Result<(), WorkspaceError> {
        println!("On branch {}", self.branch());
        let changes = self.changes()?;
        if changes.is_empty() {
            println!("No changes.");
        }
        for change in changes {
            let marker = match (change.old, change.new) {
                (None, _) => 'A',
                (_, None) => 'D',
                _ => 'M',
            };
            println!("{marker} {}", change.path);
        }
        Ok(())
    }

    /// `origin diff`: prints the changes line by line.
    ///
    /// # Errors
    ///
    /// Fails like [`Workspace::changes`], or if a changed file's content
    /// cannot be read.
    pub fn diff(&self) -> Result<(), WorkspaceError> {
        for change in self.changes()? {
            print!("{}", FileDiff::new(&self.objects, &change)?);
        }
        Ok(())
    }

    /// `origin commit`: commits the working copy onto the checked-out
    /// branch.
    ///
    /// # Errors
    ///
    /// Fails with [`WorkspaceError::NothingToCommit`] if the working copy
    /// matches the head, if the user has no identity, or like
    /// [`Workspace::snapshot`].
    pub fn commit(&self, message: String) -> Result<(), WorkspaceError> {
        let author = Identity::load()?.sign();
        let tree = self.snapshot()?;
        let head = self.head()?;
        if self
            .objects
            .diff(self.head_tree(head)?, Some(tree))?
            .is_empty()
        {
            return Err(WorkspaceError::NothingToCommit);
        }
        let summary = message.lines().next().unwrap_or_default().to_owned();
        let commit = Object::Commit(Commit {
            tree,
            parents: head.into_iter().collect(),
            author,
            message,
        })
        .encode();
        self.objects.write(&commit)?;
        self.branches.update(&[RefUpdate {
            branch: self.config.branch.clone(),
            expected: head,
            new: Some(commit.id()),
        }])?;
        println!("[{} {}] {summary}", self.branch(), commit.id().short());
        Ok(())
    }

    /// `origin log`: prints the history of the checked-out branch, newest
    /// first, following first parents.
    ///
    /// # Errors
    ///
    /// Fails if the head or a commit cannot be read.
    pub fn log(&self, limit: Option<usize>) -> Result<(), WorkspaceError> {
        let mut next = self.head()?;
        for _ in 0..limit.unwrap_or(usize::MAX) {
            let Some(id) = next else { break };
            let commit = self.objects.commit(id)?;
            let date = commit.author.time().strftime("%Y-%m-%d %H:%M %z");
            println!("{}  {date}  {}", id.short(), commit.author.name);
            for line in commit.message.lines() {
                println!("    {line}");
            }
            next = commit.parents.first().copied();
        }
        Ok(())
    }
}

/// Who is making changes, from `ORIGIN_USER_NAME` and `ORIGIN_USER_EMAIL`,
/// or the `[user]` table of `~/.config/origin/config.toml`.
#[derive(Debug, Deserialize)]
pub struct Identity {
    name: String,
    email: String,
}

impl Identity {
    /// Finds the user's identity.
    ///
    /// # Errors
    ///
    /// Fails if it is not configured.
    pub fn load() -> Result<Self, WorkspaceError> {
        if let (Ok(name), Ok(email)) = (env::var("ORIGIN_USER_NAME"), env::var("ORIGIN_USER_EMAIL"))
        {
            return Ok(Self { name, email });
        }
        let path = Self::config_path().ok_or(WorkspaceError::NoIdentity)?;
        let Ok(content) = fs::read_to_string(&path) else {
            return Err(WorkspaceError::NoIdentity);
        };
        let config: UserConfig =
            toml::from_str(&content).map_err(|source| WorkspaceError::Config { path, source })?;
        config.user.ok_or(WorkspaceError::NoIdentity)
    }

    fn config_path() -> Option<PathBuf> {
        let base = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| Path::new(&home).join(".config")))?;
        Some(base.join("origin/config.toml"))
    }

    /// Signs as this identity, now.
    #[must_use]
    pub fn sign(self) -> Signature {
        Signature::now(self.name, self.email)
    }
}

/// `~/.config/origin/config.toml`.
#[derive(Deserialize)]
struct UserConfig {
    user: Option<Identity>,
}

/// A failure to operate on a working copy.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// No repository was found.
    #[error("not in an origin repository: {} or its parents have no `.origin` directory", .0.display())]
    NotFound(PathBuf),
    /// `init` found an existing repository.
    #[error("{} already has a repository", .0.display())]
    AlreadyInitialized(PathBuf),
    /// The working copy matches the head.
    #[error("nothing to commit: the working copy has no changes")]
    NothingToCommit,
    /// The user's identity is not configured.
    #[error(
        "who are you? Set ORIGIN_USER_NAME and ORIGIN_USER_EMAIL, or add \
         `[user]` with `name` and `email` to ~/.config/origin/config.toml"
    )]
    NoIdentity,
    /// A configuration file is invalid.
    #[error("invalid configuration in {}", path.display())]
    Config {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        source: toml::de::Error,
    },
    /// A metadata file cannot be accessed.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// Objects cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Branches cannot be read or updated.
    #[error(transparent)]
    Refs(#[from] RefError),
    /// The working copy cannot be snapshotted.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

impl WorkspaceError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}
