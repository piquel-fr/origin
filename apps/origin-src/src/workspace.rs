//! A working copy and its local repository in `.origin/`.
//!
//! ```text
//! .origin/
//!   config.toml       the checked-out branch and the remote
//!   branches          local branches (see `RefStore`)
//!   remote-branches   where the remote's branches were when last synced
//!   objects/          local objects (see `ObjectStore`): only what was
//!                     fetched or created here, not the whole repository
//!   state             the stat cache of the last snapshot
//!   withheld.json     files whose content the remote withheld
//!   access.json       what the remote lets the user do
//! ```
//!
//! Withheld files are missing from the working copy, but not from
//! snapshots: they are carried over unchanged, so commits keep them.

use std::fs;
use std::io;
use std::path::{self, Path, PathBuf};

use std::collections::HashSet;

use origin_shared::access::{Access, Permissions, Principal};
use origin_shared::{
    BranchName, Commit, EntryKind, Object, ObjectId, RefUpdate, RepoPath, TreeEntry,
};
use serde::{Deserialize, Serialize};

use crate::output::FileDiff;
use crate::refs::{RefError, RefStore};
use crate::remote::{AccessInfo, Remote};
use crate::store::{ObjectStore, StoreError};
use crate::sync::{Pulled, Pushed, Sync, SyncError};
use crate::tree::{Change, TreeSource};
use crate::user::{UserConfig, UserConfigError};
use crate::worktree::{Checkout, Snapshot, SnapshotError, StatCache};

/// A working copy and its local repository.
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    meta: PathBuf,
    objects: ObjectStore,
    branches: RefStore,
    remote_branches: RefStore,
    config: Config,
}

/// One entry of `.origin/withheld.json`.
#[derive(Serialize, Deserialize)]
struct WithheldFile {
    path: RepoPath,
    kind: EntryKind,
    id: ObjectId,
}

/// `.origin/config.toml`.
#[derive(Debug, Serialize, Deserialize)]
struct Config {
    /// The checked-out branch. It may not exist yet in a new repository.
    branch: BranchName,
    /// Where to push and pull: a URL, or the path of a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
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
        Self::create(&root, BranchName::default(), None)?;
        println!("Initialized an empty repository in {}", root.display());
        Ok(())
    }

    /// Creates a repository in `root`, on a `branch` without commits.
    ///
    /// # Errors
    ///
    /// Fails if `root` already has a repository or cannot be written.
    pub fn create(
        root: &Path,
        branch: BranchName,
        remote: Option<String>,
    ) -> Result<Self, WorkspaceError> {
        let meta = root.join(Self::META_DIR);
        if meta.exists() {
            return Err(WorkspaceError::AlreadyInitialized(root.to_owned()));
        }
        fs::create_dir_all(&meta).map_err(|source| WorkspaceError::io(&meta, source))?;
        let config = Config { branch, remote };
        let workspace = Self::with_config(root.to_owned(), config);
        workspace.save_config()?;
        Ok(workspace)
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
            remote_branches: RefStore::new(meta.join("remote-branches")),
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

    /// Moves the checked-out branch, if it is still at `expected`.
    ///
    /// # Errors
    ///
    /// Fails if the branch moved or the branches cannot be written.
    pub fn move_head(
        &self,
        expected: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<(), WorkspaceError> {
        Ok(self.branches.update(&[RefUpdate {
            branch: self.config.branch.clone(),
            expected,
            new: Some(new),
        }])?)
    }

    /// The remote to push to and pull from.
    #[must_use]
    pub fn remote(&self) -> Option<&str> {
        self.config.remote.as_deref()
    }

    /// `origin remote`: prints the remote.
    pub fn show_remote(&self) {
        println!("{}", self.remote().unwrap_or("No remote."));
    }

    /// `origin remote <url>`: sets the remote. Pushing and pulling start over
    /// from what the new remote has.
    ///
    /// # Errors
    ///
    /// Fails if `url` is not a valid remote or the configuration cannot be
    /// written.
    pub fn set_remote(&mut self, url: &str) -> Result<(), SyncError> {
        let url = Self::absolute_url(url)?;
        Remote::open(&url)?;
        self.config.remote = Some(url);
        self.save_config()?;
        for file in ["remote-branches", "access.json"] {
            let path = self.meta.join(file);
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(WorkspaceError::io(&path, error).into());
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// What the remote lets the user do, as of the last sync. `None` means
    /// everything: no remote, or one the user owns.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is invalid.
    pub fn access(&self) -> Result<Option<AccessInfo>, WorkspaceError> {
        let path = self.meta.join("access.json");
        let content = match fs::read(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(WorkspaceError::io(&path, source)),
        };
        serde_json::from_slice(&content)
            .map(Some)
            .map_err(|source| WorkspaceError::Corrupt { path, source })
    }

    /// Records what the remote lets the user do.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be written.
    ///
    /// # Panics
    ///
    /// Panics if the access cannot be represented as JSON, which its fields
    /// rule out.
    pub fn set_access(&self, access: Option<&AccessInfo>) -> Result<(), WorkspaceError> {
        let path = self.meta.join("access.json");
        let result = match access {
            Some(access) => fs::write(
                &path,
                serde_json::to_vec(access).expect("access serializes to JSON"),
            ),
            None => fs::remove_file(&path).or_else(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(error)
                }
            }),
        };
        result.map_err(|source| WorkspaceError::io(&path, source))
    }

    /// The files whose content the remote withheld, with their entries.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is invalid.
    pub fn withheld(&self) -> Result<Vec<(RepoPath, TreeEntry)>, WorkspaceError> {
        let path = self.meta.join("withheld.json");
        let content = match fs::read(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(WorkspaceError::io(&path, source)),
        };
        let files: Vec<WithheldFile> = serde_json::from_slice(&content)
            .map_err(|source| WorkspaceError::Corrupt { path, source })?;
        Ok(files
            .into_iter()
            .map(|file| {
                (
                    file.path,
                    TreeEntry {
                        kind: file.kind,
                        id: file.id,
                    },
                )
            })
            .collect())
    }

    /// Records the files whose content the remote withheld.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be written.
    ///
    /// # Panics
    ///
    /// Panics if the files cannot be represented as JSON, which their fields
    /// rule out.
    pub fn set_withheld(&self, withheld: &[(RepoPath, TreeEntry)]) -> Result<(), WorkspaceError> {
        let path = self.meta.join("withheld.json");
        let files: Vec<WithheldFile> = withheld
            .iter()
            .map(|(path, entry)| WithheldFile {
                path: path.clone(),
                kind: entry.kind,
                id: entry.id,
            })
            .collect();
        let content = serde_json::to_vec(&files).expect("withheld files serialize to JSON");
        fs::write(&path, content).map_err(|source| WorkspaceError::io(&path, source))
    }

    /// Where the checked-out branch was on the remote when last synced.
    ///
    /// # Errors
    ///
    /// Fails if the remote branches cannot be read.
    pub fn tracking(&self) -> Result<Option<ObjectId>, WorkspaceError> {
        Ok(self.remote_branches.get(&self.config.branch)?)
    }

    /// Records that the checked-out branch moved on the remote.
    ///
    /// # Errors
    ///
    /// Fails if the remote branches cannot be written.
    pub fn set_tracking(
        &self,
        expected: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<(), WorkspaceError> {
        Ok(self.remote_branches.update(&[RefUpdate {
            branch: self.config.branch.clone(),
            expected,
            new: Some(new),
        }])?)
    }

    /// The root tree of a commit, or `None` for no commit.
    ///
    /// # Errors
    ///
    /// Fails if the commit cannot be read.
    pub fn head_tree(&self, head: Option<ObjectId>) -> Result<Option<ObjectId>, WorkspaceError> {
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
        let withheld = self.withheld()?;
        if withheld.is_empty() {
            return Ok(tree);
        }
        Ok(self.objects.overlay(tree, &withheld)?)
    }

    /// Moves a working copy matching the tree `old` to the tree `new`,
    /// leaving out the `withheld` files of `new`, and writes the files in
    /// `restore`, which were withheld until now, where nothing is in their
    /// way.
    ///
    /// # Errors
    ///
    /// Fails if an object is missing or a file cannot be written.
    pub fn checkout(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        withheld: &[(RepoPath, TreeEntry)],
        restore: &[(RepoPath, TreeEntry)],
    ) -> Result<(), WorkspaceError> {
        let path = self.meta.join("state");
        let mut cache = StatCache::load(&path);
        let mut changes = self.objects.diff(old, Some(new))?;
        // Withheld files are removed rather than written: their old content
        // is stale, and snapshots carry them over.
        let withheld: HashSet<&RepoPath> = withheld.iter().map(|(path, _)| path).collect();
        for change in &mut changes {
            if withheld.contains(&change.path) {
                change.new = None;
            }
        }
        changes.retain(|change| change.old.is_some() || change.new.is_some());
        changes.extend(
            restore
                .iter()
                .filter(|(path, _)| fs::symlink_metadata(self.root.join(path.as_str())).is_err())
                .map(|(path, entry)| Change {
                    path: path.clone(),
                    old: None,
                    new: Some(*entry),
                }),
        );
        let staging = self.meta.join("staging");
        Checkout::new(&self.root, &self.objects, &staging).apply(&changes, &mut cache)?;
        cache
            .save(&path)
            .map_err(|source| WorkspaceError::io(&path, source))
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
        let author = UserConfig::load()?.author()?.sign();
        let tree = self.snapshot()?;
        let head = self.head()?;
        let changes = self.objects.diff(self.head_tree(head)?, Some(tree))?;
        if changes.is_empty() {
            return Err(WorkspaceError::NothingToCommit);
        }
        if let Some(access) = self.access()? {
            for change in changes {
                if access
                    .permissions
                    .access(access.identity.as_ref(), &change.path)
                    < Access::Write
                {
                    return Err(WorkspaceError::ReadOnly(change.path));
                }
            }
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
    /// first, following first parents. Commits missing locally are fetched.
    ///
    /// # Errors
    ///
    /// Fails if the head or a commit cannot be read or fetched.
    pub async fn log(&self, limit: Option<usize>) -> Result<(), SyncError> {
        let sync = Sync::new(self)?;
        let mut next = self.head()?;
        for _ in 0..limit.unwrap_or(usize::MAX) {
            let Some(id) = next else { break };
            let commit = sync.commit(id).await?;
            let date = commit.author.time().strftime("%Y-%m-%d %H:%M %z");
            println!("{}  {date}  {}", id.short(), commit.author.name);
            for line in commit.message.lines() {
                println!("    {line}");
            }
            next = commit.parents.first().copied();
        }
        Ok(())
    }

    /// `origin clone`: checks out `branch` of the repository at `url` into
    /// `path`, or a directory named after the repository.
    ///
    /// # Errors
    ///
    /// Fails like [`Sync::clone`], or if `path` is missing and the
    /// repository's name cannot be told from `url`.
    pub async fn clone(
        url: &str,
        path: Option<&Path>,
        branch: Option<BranchName>,
    ) -> Result<(), SyncError> {
        let url = Self::absolute_url(url)?;
        let name = Path::new(url.trim_end_matches('/')).file_name();
        let root = path
            .or(name.map(Path::new))
            .ok_or_else(|| SyncError::Unnamed(url.clone()))?;
        let root = path::absolute(root).map_err(|source| WorkspaceError::io(root, source))?;
        let workspace = Sync::clone(&url, &root, branch).await?;
        match workspace.head()? {
            Some(head) => println!(
                "Cloned {} at {} into {}",
                workspace.branch(),
                head.short(),
                root.display()
            ),
            None => println!("Cloned an empty repository into {}", root.display()),
        }
        Ok(())
    }

    /// `origin whoami`: prints who the remote thinks the user is.
    ///
    /// # Errors
    ///
    /// Fails if there is no remote or it cannot be reached.
    pub async fn whoami(&self) -> Result<(), SyncError> {
        match Sync::new(self)?.remote()?.access().await? {
            None => println!("The owner: the remote is a local repository."),
            Some(AccessInfo { identity: None, .. }) => println!("Anonymous."),
            Some(AccessInfo {
                identity: Some(identity),
                ..
            }) if identity.groups.is_empty() => println!("{}", identity.user),
            Some(AccessInfo {
                identity: Some(identity),
                ..
            }) => println!("{} (groups: {})", identity.user, identity.groups.join(", ")),
        }
        Ok(())
    }

    /// `origin permissions`: prints who may read and write what on the
    /// remote.
    ///
    /// # Errors
    ///
    /// Fails if there is no remote or it cannot be reached.
    pub async fn permissions(&self) -> Result<(), SyncError> {
        Self::print_permissions(&Sync::new(self)?.remote()?.permissions().await?);
        Ok(())
    }

    /// `origin permissions grant`: sets `principal`'s access to `path` and
    /// everything under it, then prints the permissions.
    ///
    /// # Errors
    ///
    /// Fails if there is no remote, it cannot be reached, or it refuses.
    pub async fn grant(
        &self,
        path: RepoPath,
        principal: Principal,
        access: Access,
    ) -> Result<(), SyncError> {
        let sync = Sync::new(self)?;
        let remote = sync.remote()?;
        let mut permissions = remote.permissions().await?;
        permissions.rule(path).set(principal, access);
        remote.set_permissions(&permissions).await?;
        Self::print_permissions(&permissions);
        Ok(())
    }

    fn print_permissions(permissions: &Permissions) {
        if permissions.is_empty() {
            println!("No rules: everyone may read and write everything.");
        }
        for (path, rule) in permissions.rules() {
            let grants: Vec<String> = rule
                .all
                .map(|access| format!("all: {access}"))
                .into_iter()
                .chain(
                    rule.users
                        .iter()
                        .map(|(user, access)| format!("user:{user}: {access}")),
                )
                .chain(
                    rule.groups
                        .iter()
                        .map(|(group, access)| format!("group:{group}: {access}")),
                )
                .collect();
            println!("{path}  {}", grants.join(", "));
        }
    }

    /// Makes a remote `url` independent of the current directory: local
    /// paths become absolute, URLs are kept.
    fn absolute_url(url: &str) -> Result<String, WorkspaceError> {
        if url.starts_with("http://") || url.starts_with("https://") {
            return Ok(url.to_owned());
        }
        let path =
            path::absolute(url).map_err(|source| WorkspaceError::io(Path::new(url), source))?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// `origin pull`: fast-forwards the checked-out branch to the remote's.
    ///
    /// # Errors
    ///
    /// Fails like [`Sync::pull`].
    pub async fn pull(&self) -> Result<(), SyncError> {
        match Sync::new(self)?.pull().await? {
            Pulled::UpToDate => println!("Already up to date."),
            Pulled::FastForwarded { to, files } => {
                println!("Fast-forwarded to {}: {files} files changed", to.short());
            }
        }
        Ok(())
    }

    /// `origin push`: sends the checked-out branch's new commits to the
    /// remote.
    ///
    /// # Errors
    ///
    /// Fails like [`Sync::push`].
    pub async fn push(&self) -> Result<(), SyncError> {
        match Sync::new(self)?.push().await? {
            Pushed::UpToDate => println!("Everything up to date."),
            Pushed::Pushed { to, commits } => {
                println!(
                    "Pushed {commits} commits: {} is at {}",
                    self.branch(),
                    to.short()
                );
            }
        }
        Ok(())
    }
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
    /// A change touches a file the remote does not let the user write.
    #[error("you may not change `{0}`: the remote would reject it")]
    ReadOnly(RepoPath),
    /// A metadata file is invalid.
    #[error("{} is corrupt", path.display())]
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        source: serde_json::Error,
    },
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
    /// The user configuration is unusable.
    #[error(transparent)]
    User(#[from] UserConfigError),
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
