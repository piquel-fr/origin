//! `OriginService`: everything a server can do, independent of transport.
//!
//! HTTP (and later SSH, MCP, and the git protocol) only translate requests
//! into these methods. The CLI also calls them in-process for repositories on
//! a local path, so local and remote repositories behave the same.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use origin_shared::access::{Access, Identity, Permissions};
use origin_shared::api::{
    Branch, CommitsRequest, ListedEntry, Location, ObjectsRequest, RepoInfo, RepoName, RepoSummary,
};
use origin_shared::pack::{Pack, PackError, PushRequest};
use origin_shared::{
    BranchName, Commit, EncodedObject, EntryKind, ObjectId, ObjectKind, RepoPath, Tree, TreeEntry,
};

use crate::parallel::par_map;
use crate::refs::RefError;
use crate::repository::{RepoConfig, Repository, RepositoryError};
use crate::store::{ObjectStore, StoreError};
use crate::tree::{CachedTrees, TreeSource};

/// Who is calling the service.
#[derive(Clone, Debug)]
pub enum Caller {
    /// Someone with access to the repositories' files, like the CLI on a
    /// local path: permissions do not apply.
    Owner,
    /// A client, authenticated or anonymous.
    Client(Option<Identity>),
}

impl Caller {
    fn access(&self, permissions: &Permissions, path: &RepoPath) -> Access {
        match self {
            Self::Owner => Access::Write,
            Self::Client(identity) => permissions.access(identity.as_ref(), path),
        }
    }

    fn can_see(&self, permissions: &Permissions) -> bool {
        match self {
            Self::Owner => true,
            Self::Client(identity) => permissions.can_see(identity.as_ref()),
        }
    }
}

/// A repository opened for a caller allowed to see it.
struct Opened<'a> {
    repository: Repository,
    config: RepoConfig,
    caller: &'a Caller,
}

impl Opened<'_> {
    fn access(&self, path: &RepoPath) -> Access {
        self.caller.access(&self.config.permissions, path)
    }

    /// Fails unless the caller may write `path`.
    fn require_write(&self, path: &RepoPath) -> Result<(), ServiceError> {
        if self.access(path) < Access::Write {
            return Err(ServiceError::Forbidden(path.clone()));
        }
        Ok(())
    }
}

/// The repositories a server hosts, and what can be done with them.
///
/// It holds no state besides where each repository is: every call reads
/// from disk, so repositories and their permissions can be edited while the
/// server runs.
///
/// Repositories a caller may not see do not exist for them, and files they
/// may not read are withheld: their names and ids are visible, their
/// content is not.
#[derive(Clone, Debug)]
pub struct OriginService {
    repos: BTreeMap<RepoName, PathBuf>,
}

impl OriginService {
    /// Serves the repositories at the given paths.
    #[must_use]
    pub fn new(repos: BTreeMap<RepoName, PathBuf>) -> Self {
        Self { repos }
    }

    fn open<'a>(&self, caller: &'a Caller, repo: &RepoName) -> Result<Opened<'a>, ServiceError> {
        let not_found = || ServiceError::RepoNotFound(repo.clone());
        let repository = Repository::open(self.repos.get(repo).ok_or_else(not_found)?)?;
        let config = repository.config()?;
        if !caller.can_see(&config.permissions) {
            return Err(not_found());
        }
        Ok(Opened {
            repository,
            config,
            caller,
        })
    }

    /// Lists the repositories the caller may see.
    ///
    /// # Errors
    ///
    /// Fails if a repository cannot be read.
    pub fn list(&self, caller: &Caller) -> Result<Vec<RepoSummary>, ServiceError> {
        let mut repos = Vec::new();
        for name in self.repos.keys() {
            match self.open(caller, name) {
                Ok(opened) => repos.push(RepoSummary {
                    name: name.clone(),
                    description: opened.config.description,
                }),
                Err(ServiceError::RepoNotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(repos)
    }

    /// Describes a repository and its branches.
    ///
    /// # Errors
    ///
    /// Fails if the repository does not exist or cannot be read.
    pub fn info(&self, caller: &Caller, repo: &RepoName) -> Result<RepoInfo, ServiceError> {
        let opened = self.open(caller, repo)?;
        Ok(RepoInfo {
            name: repo.clone(),
            branches: opened
                .repository
                .branches()
                .list()?
                .into_iter()
                .map(|(name, commit)| Branch { name, commit })
                .collect(),
            description: opened.config.description,
        })
    }

    /// The repository's permissions.
    ///
    /// # Errors
    ///
    /// Fails if the repository does not exist or cannot be read.
    pub fn permissions(
        &self,
        caller: &Caller,
        repo: &RepoName,
    ) -> Result<Permissions, ServiceError> {
        Ok(self.open(caller, repo)?.config.permissions)
    }

    /// Replaces the repository's permissions. Takes write access to the
    /// root.
    ///
    /// # Errors
    ///
    /// Fails with [`ServiceError::Forbidden`] if the caller may not write the
    /// root.
    pub fn set_permissions(
        &self,
        caller: &Caller,
        repo: &RepoName,
        permissions: Permissions,
    ) -> Result<(), ServiceError> {
        let mut opened = self.open(caller, repo)?;
        opened.require_write(&RepoPath::root())?;
        opened.config.permissions = permissions;
        Ok(opened.repository.save_config(&opened.config)?)
    }

    /// Finds what is at a location.
    fn resolve(repository: &Repository, location: &Location) -> Result<TreeEntry, ServiceError> {
        let not_found =
            || ServiceError::PathNotFound(location.branch.clone(), location.path.clone());
        let commit = repository
            .branches()
            .get(&location.branch)?
            .ok_or_else(|| ServiceError::BranchNotFound(location.branch.clone()))?;
        let tree = repository.objects().commit(commit)?.tree;
        repository
            .objects()
            .resolve(tree, &location.path)?
            .ok_or_else(not_found)
    }

    /// Lists a directory. Every name is visible, even of files the caller
    /// may not read.
    ///
    /// # Errors
    ///
    /// Fails if the branch or directory does not exist.
    pub fn tree(
        &self,
        caller: &Caller,
        repo: &RepoName,
        location: &Location,
    ) -> Result<Vec<ListedEntry>, ServiceError> {
        let opened = self.open(caller, repo)?;
        let entry = Self::resolve(&opened.repository, location)?;
        if !entry.kind.is_tree() {
            return Err(ServiceError::NotADirectory(location.path.clone()));
        }
        Ok(opened
            .repository
            .objects()
            .tree(entry.id)?
            .iter()
            .map(|(name, entry)| ListedEntry {
                name: name.to_string(),
                kind: entry.kind,
                id: entry.id,
            })
            .collect())
    }

    /// Reads a file's content.
    ///
    /// # Errors
    ///
    /// Fails if the branch or file does not exist, or the caller may not
    /// read the file, which looks the same.
    pub fn file(
        &self,
        caller: &Caller,
        repo: &RepoName,
        location: &Location,
    ) -> Result<Vec<u8>, ServiceError> {
        let opened = self.open(caller, repo)?;
        let entry = Self::resolve(&opened.repository, location)?;
        if entry.kind == EntryKind::Tree {
            return Err(ServiceError::NotAFile(location.path.clone()));
        }
        if opened.access(&location.path) < Access::Read {
            return Err(ServiceError::PathNotFound(
                location.branch.clone(),
                location.path.clone(),
            ));
        }
        let blob = opened.repository.objects().read(entry.id)?;
        Ok(blob
            .blob_content()
            .map_err(|source| StoreError::Decode {
                id: entry.id,
                source,
            })?
            .to_vec())
    }

    /// Returns commits and their ancestors, breadth first.
    ///
    /// # Errors
    ///
    /// Fails if a commit does not exist.
    pub fn commits(
        &self,
        caller: &Caller,
        repo: &RepoName,
        request: &CommitsRequest,
    ) -> Result<Pack, ServiceError> {
        let opened = self.open(caller, repo)?;
        let objects = opened.repository.objects();
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<(ObjectId, u32)> = request.ids.iter().map(|&id| (id, 1)).collect();
        let mut pack = Pack::default();
        while let Some((id, depth)) = queue.pop_front() {
            if depth > request.depth || !seen.insert(id) {
                continue;
            }
            let object = objects
                .try_read(id)?
                .ok_or(ServiceError::ObjectNotFound(id))?;
            let commit = object
                .decode_commit()
                .map_err(|source| StoreError::Decode { id, source })?;
            queue.extend(commit.parents.iter().map(|&parent| (parent, depth + 1)));
            pack.objects.push(object);
        }
        Ok(pack)
    }

    /// Returns the object at each path of a commit. Files the caller may
    /// not read are withheld.
    ///
    /// # Errors
    ///
    /// Fails if the commit or a path does not exist.
    pub fn objects(
        &self,
        caller: &Caller,
        repo: &RepoName,
        request: &ObjectsRequest,
    ) -> Result<Pack, ServiceError> {
        let opened = self.open(caller, repo)?;
        let objects = opened.repository.objects();
        let root = objects
            .try_read(request.commit)?
            .ok_or(ServiceError::ObjectNotFound(request.commit))?
            .decode_commit()
            .map_err(|source| StoreError::Decode {
                id: request.commit,
                source,
            })?
            .tree;
        let trees = CachedTrees::new(objects);
        let mut ids = Vec::new();
        let mut withheld = Vec::new();
        for path in &request.paths {
            let entry = trees
                .resolve(root, path)?
                .ok_or_else(|| ServiceError::NotInCommit {
                    commit: request.commit,
                    path: path.clone(),
                })?;
            if entry.kind.is_tree() || opened.access(path) >= Access::Read {
                ids.push(entry.id);
            } else {
                withheld.push(entry.id);
            }
        }
        let objects = par_map(&ids, |&id| objects.read(id))?;
        Ok(Pack { objects, withheld })
    }

    /// Applies a push as one transaction: validates it, stores its objects,
    /// and moves every branch, or none if any moved since the pusher looked.
    ///
    /// Each new commit must come with every object it adds relative to its
    /// first parent, even ones the server already has: pushing an object
    /// proves the pusher knows its content, so nobody can copy a file they
    /// may not read by referring to its id. The caller must be allowed to
    /// write every path the new commits change and every path that differs
    /// between a branch's old and new commit, and the root to delete a
    /// branch.
    ///
    /// # Errors
    ///
    /// Fails with [`ServiceError::Conflict`] if a branch moved,
    /// [`ServiceError::Forbidden`] if the caller may not write a changed
    /// path, or [`ServiceError::InvalidPush`] if the push is incomplete or
    /// malformed.
    pub fn push(
        &self,
        caller: &Caller,
        repo: &RepoName,
        push: PushRequest,
    ) -> Result<(), ServiceError> {
        let opened = self.open(caller, repo)?;
        let store = opened.repository.objects();
        let pushed = push
            .pack
            .objects
            .into_iter()
            .map(|object| (object.id(), object))
            .collect();
        let trees = PushedTrees {
            store,
            pushed: &pushed,
            opened: &opened,
        };
        let mut queue = Vec::new();
        for update in &push.updates {
            match update.new {
                Some(new) => {
                    trees.commit(new)?;
                    queue.push(new);
                }
                None => opened.require_write(&RepoPath::root())?,
            }
        }
        // Only objects of checked commits are stored: anything else in the
        // pack is ignored, so the store never holds unchecked objects.
        let mut accepted = HashSet::new();
        while let Some(id) = queue.pop() {
            if store.contains(id) || accepted.contains(&id) {
                continue;
            }
            queue.extend(trees.check_commit(id, &mut accepted)?);
        }
        // What readers of a branch see change is what the caller must be
        // allowed to write, whichever commits get there.
        for update in &push.updates {
            let Some(new) = update.new else { continue };
            let old_tree = update
                .expected
                .map(|old| trees.commit(old))
                .transpose()?
                .map(|old| old.tree);
            for change in trees.diff(old_tree, Some(trees.commit(new)?.tree))? {
                opened.require_write(&change.path)?;
            }
        }
        let accepted: Vec<&EncodedObject> =
            accepted.iter().filter_map(|id| pushed.get(id)).collect();
        par_map(&accepted, |object| store.write(object))?;
        opened
            .repository
            .branches()
            .update(&push.updates)
            .map_err(|error| match error {
                RefError::Conflict { .. } => ServiceError::Conflict(error),
                error => error.into(),
            })
    }
}

/// Reads objects from a push, falling back to the repository.
struct PushedTrees<'a> {
    store: &'a ObjectStore,
    pushed: &'a HashMap<ObjectId, EncodedObject>,
    opened: &'a Opened<'a>,
}

impl PushedTrees<'_> {
    fn read(&self, id: ObjectId, expected: ObjectKind) -> Result<EncodedObject, ServiceError> {
        let object = match self.pushed.get(&id) {
            Some(object) => object.clone(),
            None => self
                .store
                .try_read(id)?
                .ok_or(PushError::Missing { id, expected })?,
        };
        if object.kind().ok() != Some(expected) {
            return Err(PushError::WrongKind { id, expected }.into());
        }
        Ok(object)
    }

    fn commit(&self, id: ObjectId) -> Result<Commit, ServiceError> {
        let object = self.read(id, ObjectKind::Commit)?;
        Ok(object
            .decode_commit()
            .map_err(|source| StoreError::Decode { id, source })?)
    }

    /// Checks that a pushed commit comes with every object it adds and only
    /// changes paths the caller may write, adds its objects and itself to
    /// `accepted`, and returns its parents.
    fn check_commit(
        &self,
        id: ObjectId,
        accepted: &mut HashSet<ObjectId>,
    ) -> Result<Vec<ObjectId>, ServiceError> {
        if !self.pushed.contains_key(&id) {
            return Err(PushError::Missing {
                id,
                expected: ObjectKind::Commit,
            }
            .into());
        }
        let commit = self.commit(id)?;
        let mut parents = commit.parents.iter();
        let old_tree = parents
            .next()
            .map(|&parent| self.commit(parent))
            .transpose()?
            .map(|parent| parent.tree);
        for &parent in parents {
            self.commit(parent)?;
        }
        self.for_each_change(commit.tree, old_tree, &mut |path, entry| {
            let expected = if entry.kind.is_tree() {
                ObjectKind::Tree
            } else {
                ObjectKind::Blob
            };
            let object = self
                .pushed
                .get(&entry.id)
                .ok_or_else(|| PushError::NotIncluded {
                    commit: id,
                    path: path.clone(),
                    id: entry.id,
                })?;
            if object.kind().ok() != Some(expected) {
                return Err(PushError::WrongKind {
                    id: entry.id,
                    expected,
                }
                .into());
            }
            accepted.insert(entry.id);
            Ok(())
        })?;
        for change in self.diff(old_tree, Some(commit.tree))? {
            self.opened.require_write(&change.path)?;
        }
        accepted.insert(id);
        Ok(commit.parents)
    }
}

impl TreeSource for PushedTrees<'_> {
    type Error = ServiceError;

    fn read_tree(&self, id: ObjectId) -> Result<Tree, ServiceError> {
        Ok(self
            .read(id, ObjectKind::Tree)?
            .decode_tree()
            .map_err(|source| StoreError::Decode { id, source })?)
    }
}

/// Why a push was rejected before it touched the repository.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    /// The request is not a valid push.
    #[error("malformed push")]
    Malformed(#[from] PackError),
    /// An object is neither pushed nor in the repository.
    #[error("{expected} {id} is missing")]
    Missing {
        /// The object.
        id: ObjectId,
        /// What it should be.
        expected: ObjectKind,
    },
    /// An object is not of the kind its reference requires.
    #[error("{id} is not a {expected}")]
    WrongKind {
        /// The object.
        id: ObjectId,
        /// What it should be.
        expected: ObjectKind,
    },
    /// A pushed commit changes a path without including the new object.
    #[error("commit {commit} changes `{path}` but the push does not include {id}")]
    NotIncluded {
        /// The commit.
        commit: ObjectId,
        /// The changed path.
        path: RepoPath,
        /// The object it changed to.
        id: ObjectId,
    },
}

/// A failed service call.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The server has no such repository.
    #[error("repository `{0}` not found")]
    RepoNotFound(RepoName),
    /// The repository has no such branch.
    #[error("branch `{0}` not found")]
    BranchNotFound(BranchName),
    /// Nothing is at a path.
    #[error("`{1}` not found on branch `{0}`")]
    PathNotFound(BranchName, RepoPath),
    /// A path expected to be a directory is a file.
    #[error("`{0}` is not a directory")]
    NotADirectory(RepoPath),
    /// A path expected to be a file is a directory.
    #[error("`{0}` is not a file")]
    NotAFile(RepoPath),
    /// An object does not exist.
    #[error("object {0} not found")]
    ObjectNotFound(ObjectId),
    /// Nothing is at a path of a commit.
    #[error("`{path}` not found in commit {commit}")]
    NotInCommit {
        /// The commit.
        commit: ObjectId,
        /// The path.
        path: RepoPath,
    },
    /// The caller may not change a path.
    #[error("you may not write `{0}`")]
    Forbidden(RepoPath),
    /// A branch moved since the pusher last looked.
    #[error("push rejected: {0}")]
    Conflict(RefError),
    /// A push is incomplete or malformed.
    #[error("push rejected: {0}")]
    InvalidPush(#[from] PushError),
    /// A repository cannot be opened.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    /// Objects cannot be read.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Branches cannot be read.
    #[error(transparent)]
    Refs(#[from] RefError),
}
