//! Cloning, pulling, and pushing.
//!
//! Workspaces fetch lazily: a clone downloads one branch's latest commit and
//! the files it checks out, not the history. Older commits are fetched when
//! something, like `origin log`, needs them.
//!
//! Every sync also records what the remote lets the user do, so commits
//! touching files the user may not write fail right away instead of at push
//! time, and which files the remote withheld.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use origin_shared::api::{CommitsRequest, ObjectsRequest};
use origin_shared::pack::{Pack, PushRequest};
use origin_shared::{BranchName, Commit, EntryKind, ObjectId, RefUpdate, RepoPath, TreeEntry};

use crate::parallel::par_map;
use crate::remote::{Remote, RemoteError};
use crate::store::StoreError;
use crate::tree::TreeSource;
use crate::workspace::{Workspace, WorkspaceError};

/// A workspace and its remote.
pub struct Sync<'a> {
    workspace: &'a Workspace,
    remote: Option<Remote>,
}

/// What a pull did.
#[derive(Debug, PartialEq, Eq)]
pub enum Pulled {
    /// The branch already matched the remote.
    UpToDate,
    /// The branch moved to the remote's commit.
    FastForwarded {
        /// The new commit.
        to: ObjectId,
        /// How many files changed.
        files: usize,
    },
}

/// What a push did.
#[derive(Debug, PartialEq, Eq)]
pub enum Pushed {
    /// The remote already had every commit.
    UpToDate,
    /// The remote's branch moved to the local commit.
    Pushed {
        /// The new commit.
        to: ObjectId,
        /// How many commits were sent.
        commits: usize,
    },
}

impl<'a> Sync<'a> {
    /// How many commits to fetch at once when walking history.
    const HISTORY_PAGE: u32 = 100;

    /// Syncs `workspace` with its remote, if it has one.
    ///
    /// # Errors
    ///
    /// Fails if the remote URL is invalid.
    pub fn new(workspace: &'a Workspace) -> Result<Self, SyncError> {
        let remote = workspace.remote().map(Remote::open).transpose()?;
        Ok(Self { workspace, remote })
    }

    /// The remote.
    ///
    /// # Errors
    ///
    /// Fails if the workspace has no remote.
    pub fn remote(&self) -> Result<&Remote, SyncError> {
        self.remote.as_ref().ok_or(SyncError::NoRemote)
    }

    /// Clones `branch` of the repository at `url` into `root`, which must
    /// not exist or be empty. A local `url` must be absolute.
    ///
    /// # Errors
    ///
    /// Fails if `root` is not empty, the remote cannot be reached, or the
    /// branch does not exist.
    pub async fn clone(
        url: &str,
        root: &Path,
        branch: Option<BranchName>,
    ) -> Result<Workspace, SyncError> {
        if fs::read_dir(root).is_ok_and(|mut entries| entries.next().is_some()) {
            return Err(SyncError::NotEmpty(root.to_owned()));
        }
        let remote = Remote::open(url)?;
        let info = remote.info().await?;
        let explicit = branch.is_some();
        let branch = branch.unwrap_or_default();
        let head = info
            .branches
            .iter()
            .find(|candidate| candidate.name == branch)
            .map(|branch| branch.commit);
        if head.is_none() && explicit {
            return Err(SyncError::NoSuchBranch(branch));
        }
        fs::create_dir_all(root).map_err(|source| WorkspaceError::Io {
            path: root.to_owned(),
            source,
        })?;
        let workspace = Workspace::create(root, branch, Some(url.to_owned()))?;
        if let Some(head) = head {
            let sync = Sync {
                workspace: &workspace,
                remote: Some(remote),
            };
            let tree = sync.commit(head).await?.tree;
            sync.update(head, None, tree).await?;
            workspace.move_head(None, head)?;
            workspace.set_tracking(None, head)?;
        }
        Ok(workspace)
    }

    /// Fast-forwards the checked-out branch to the remote's.
    ///
    /// # Errors
    ///
    /// Fails if there are commits to push or changes in the working copy:
    /// pulls never merge.
    pub async fn pull(&self) -> Result<Pulled, SyncError> {
        let remote_head = self
            .remote_head()
            .await?
            .ok_or_else(|| SyncError::NoSuchBranch(self.workspace.branch().clone()))?;
        let (head, tracking) = (self.workspace.head()?, self.workspace.tracking()?);
        if head == Some(remote_head) {
            if tracking != head {
                self.workspace.set_tracking(tracking, remote_head)?;
            }
            // Access may have changed even if the branch did not.
            let tree = self.commit(remote_head).await?.tree;
            self.update(remote_head, Some(tree), tree).await?;
            return Ok(Pulled::UpToDate);
        }
        if head != tracking {
            return Err(SyncError::UnpushedCommits);
        }
        if !self.workspace.changes()?.is_empty() {
            return Err(SyncError::UncommittedChanges);
        }
        if let Some(head) = head
            && !self.descends(remote_head, head).await?
        {
            return Err(SyncError::Diverged);
        }
        let old_tree = self.workspace.head_tree(head)?;
        let new_tree = self.commit(remote_head).await?.tree;
        let files = self.update(remote_head, old_tree, new_tree).await?;
        self.workspace.move_head(head, remote_head)?;
        self.workspace.set_tracking(tracking, remote_head)?;
        Ok(Pulled::FastForwarded {
            to: remote_head,
            files,
        })
    }

    /// Pushes the checked-out branch's new commits, as one transaction.
    ///
    /// # Errors
    ///
    /// Fails with [`SyncError::Remote`] wrapping [`RemoteError::Conflict`]
    /// if someone else pushed first.
    pub async fn push(&self) -> Result<Pushed, SyncError> {
        let remote = self.remote()?;
        let head = self.workspace.head()?.ok_or(SyncError::NothingToPush)?;
        let tracking = self.workspace.tracking()?;
        if Some(head) == tracking {
            return Ok(Pushed::UpToDate);
        }
        let store = self.workspace.objects();
        let mut pack = Pack::default();
        let mut commits = 0;
        let mut next = Some(head);
        while next != tracking {
            let Some(id) = next else {
                // The walk passed the remote's commit without meeting it.
                return Err(SyncError::Diverged);
            };
            let commit = store.commit(id)?;
            let parent = commit.parents.first().copied();
            let parent_tree = self.workspace.head_tree(parent)?;
            store.for_each_change(commit.tree, parent_tree, &mut |_, entry| {
                pack.objects.push(store.read(entry.id)?);
                Ok(())
            })?;
            pack.objects.push(store.read(id)?);
            commits += 1;
            next = parent;
        }
        remote
            .push(PushRequest {
                updates: vec![RefUpdate {
                    branch: self.workspace.branch().clone(),
                    expected: tracking,
                    new: Some(head),
                }],
                pack,
            })
            .await?;
        self.workspace.set_tracking(tracking, head)?;
        self.workspace.set_access(remote.access().await?.as_ref())?;
        Ok(Pushed::Pushed { to: head, commits })
    }

    /// Moves the working copy from the tree `old` to the tree `new` of
    /// `commit`, fetching what it needs, and refreshes what the user may do.
    /// Returns how many files changed.
    async fn update(
        &self,
        commit: ObjectId,
        old: Option<ObjectId>,
        new: ObjectId,
    ) -> Result<usize, SyncError> {
        let store = self.workspace.objects();
        let mut withheld = self.fetch_tree(commit, new, old).await?;
        // Files withheld before and unchanged since are asked for again, in
        // case the user may read them now.
        let mut unchanged = Vec::new();
        for (path, entry) in self.workspace.withheld()? {
            if store.resolve(new, &path)? == Some(entry) {
                unchanged.push((path, entry));
            }
        }
        let mut restored = Vec::new();
        if !unchanged.is_empty() {
            let paths = unchanged.iter().map(|(path, _)| path.clone()).collect();
            let pack = self
                .remote()?
                .objects(&ObjectsRequest { commit, paths })
                .await?;
            par_map(&pack.objects, |object| store.write(object))?;
            for (path, entry) in unchanged {
                if store.contains(entry.id) {
                    restored.push((path, entry));
                } else {
                    withheld.push((path, entry));
                }
            }
        }
        self.workspace
            .set_access(self.remote()?.access().await?.as_ref())?;
        let files = store.diff(old, Some(new))?.len() + restored.len();
        // Only recorded once the working copy matches it, so a failed
        // checkout is retried in full by the next pull.
        self.workspace.checkout(old, new, &withheld, &restored)?;
        self.workspace.set_withheld(&withheld)?;
        Ok(files)
    }

    /// Reads a commit, fetching it and a page of its ancestors from the
    /// remote if it is not local.
    ///
    /// # Errors
    ///
    /// Fails if the commit is not local and cannot be fetched.
    pub async fn commit(&self, id: ObjectId) -> Result<Commit, SyncError> {
        let store = self.workspace.objects();
        if !store.contains(id) {
            let request = CommitsRequest {
                ids: vec![id],
                depth: Self::HISTORY_PAGE,
            };
            for object in self.remote()?.commits(&request).await?.objects {
                store.write(&object)?;
            }
        }
        Ok(store.commit(id)?)
    }

    /// Whether `ancestor` is `commit` or one of its ancestors, fetching
    /// history as needed.
    async fn descends(&self, commit: ObjectId, ancestor: ObjectId) -> Result<bool, SyncError> {
        let mut seen = HashSet::new();
        let mut queue = vec![commit];
        while let Some(id) = queue.pop() {
            if id == ancestor {
                return Ok(true);
            }
            if seen.insert(id) {
                queue.extend(self.commit(id).await?.parents);
            }
        }
        Ok(false)
    }

    async fn remote_head(&self) -> Result<Option<ObjectId>, SyncError> {
        let info = self.remote()?.info().await?;
        Ok(info
            .branches
            .into_iter()
            .find(|branch| branch.name == *self.workspace.branch())
            .map(|branch| branch.commit))
    }

    /// Fetches the objects the tree `new` of `commit` adds to the tree
    /// `old`, which must be complete locally, and returns the files the
    /// remote withheld. Each request fetches one depth of the tree, so a
    /// fetch takes as many round trips as the tree is deep.
    async fn fetch_tree(
        &self,
        commit: ObjectId,
        new: ObjectId,
        old: Option<ObjectId>,
    ) -> Result<Vec<(RepoPath, TreeEntry)>, SyncError> {
        let store = self.workspace.objects();
        let root = TreeEntry {
            kind: EntryKind::Tree,
            id: new,
        };
        let mut depth = vec![(RepoPath::root(), root, old)];
        let mut withheld = Vec::new();
        while !depth.is_empty() {
            let missing: Vec<&(RepoPath, TreeEntry, Option<ObjectId>)> = depth
                .iter()
                .filter(|(_, entry, _)| !store.contains(entry.id))
                .collect();
            if !missing.is_empty() {
                let paths = missing.iter().map(|(path, ..)| path.clone()).collect();
                let request = ObjectsRequest { commit, paths };
                let pack = self.remote()?.objects(&request).await?;
                par_map(&pack.objects, |object| store.write(object))?;
                // A file is withheld only if its content did not arrive: the
                // same content may be readable at another path.
                withheld.extend(
                    missing
                        .iter()
                        .filter(|(_, entry, _)| !store.contains(entry.id))
                        .map(|(path, entry, _)| (path.clone(), *entry)),
                );
            }
            let mut next = Vec::new();
            for (path, entry, old) in depth {
                if !entry.kind.is_tree() || Some(entry.id) == old {
                    continue;
                }
                let old = old
                    .map(|old| store.tree(old))
                    .transpose()?
                    .unwrap_or_default();
                for (name, child) in &store.tree(entry.id)? {
                    let old_child = old.get(name.as_str());
                    if old_child == Some(child) {
                        continue;
                    }
                    let old_tree = old_child
                        .filter(|old| old.kind.is_tree() && child.kind.is_tree())
                        .map(|old| old.id);
                    next.push((path.join(name), *child, old_tree));
                }
            }
            depth = next;
        }
        Ok(withheld)
    }
}

/// A failure to sync with a remote.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The workspace has no remote.
    #[error("no remote: set one with `origin remote <url>`")]
    NoRemote,
    /// The remote has no such branch.
    #[error("the remote has no branch `{0}`")]
    NoSuchBranch(BranchName),
    /// A clone was given no path, and the URL does not end in a name.
    #[error("cannot tell the repository's name from `{0}`: pass a path")]
    Unnamed(String),
    /// A clone was pointed at a directory with files in it.
    #[error("{} is not empty", .0.display())]
    NotEmpty(std::path::PathBuf),
    /// Pulling would need a merge.
    #[error("there are local commits to push first: pulls only fast-forward for now")]
    UnpushedCommits,
    /// Pulling would overwrite changes.
    #[error("commit your changes first: pulls only update a clean working copy")]
    UncommittedChanges,
    /// There is nothing to push.
    #[error("nothing to push: the branch has no commits")]
    NothingToPush,
    /// Neither branch descends from the other.
    #[error("the local and remote branches have diverged")]
    Diverged,
    /// The remote failed.
    #[error(transparent)]
    Remote(#[from] RemoteError),
    /// The workspace failed.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// Objects cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}
