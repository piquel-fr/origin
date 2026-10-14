//! `OriginService`: everything a server can do, independent of transport.
//!
//! HTTP (and later SSH, MCP, and the git protocol) only translate requests
//! into these methods. The CLI also calls them in-process for repositories on
//! a local path, so local and remote repositories behave the same.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use origin_shared::api::{
    Branch, CommitsRequest, ListedEntry, Location, ObjectsRequest, RepoInfo, RepoName, RepoSummary,
};
use origin_shared::pack::Pack;
use origin_shared::{BranchName, EntryKind, ObjectId, RepoPath, TreeEntry};

use crate::refs::RefError;
use crate::repository::{Repository, RepositoryError};
use crate::store::StoreError;

/// The repositories a server hosts, and what can be done with them.
///
/// It holds no state besides where each repository is: every call reads
/// from disk, so repositories can be edited while the server runs.
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

    fn open(&self, repo: &RepoName) -> Result<Repository, ServiceError> {
        let path = self
            .repos
            .get(repo)
            .ok_or_else(|| ServiceError::RepoNotFound(repo.clone()))?;
        Ok(Repository::open(path)?)
    }

    /// Lists every repository.
    ///
    /// # Errors
    ///
    /// Fails if a repository cannot be read.
    pub fn list(&self) -> Result<Vec<RepoSummary>, ServiceError> {
        self.repos
            .keys()
            .map(|name| {
                Ok(RepoSummary {
                    name: name.clone(),
                    description: self.open(name)?.config()?.description,
                })
            })
            .collect()
    }

    /// Describes a repository and its branches.
    ///
    /// # Errors
    ///
    /// Fails if the repository does not exist or cannot be read.
    pub fn info(&self, repo: &RepoName) -> Result<RepoInfo, ServiceError> {
        let repository = self.open(repo)?;
        Ok(RepoInfo {
            name: repo.clone(),
            description: repository.config()?.description,
            branches: repository
                .branches()
                .list()?
                .into_iter()
                .map(|(name, commit)| Branch { name, commit })
                .collect(),
        })
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

    /// Lists a directory.
    ///
    /// # Errors
    ///
    /// Fails if the branch or directory does not exist.
    pub fn tree(
        &self,
        repo: &RepoName,
        location: &Location,
    ) -> Result<Vec<ListedEntry>, ServiceError> {
        let repository = self.open(repo)?;
        let entry = Self::resolve(&repository, location)?;
        if !entry.kind.is_tree() {
            return Err(ServiceError::NotADirectory(location.path.clone()));
        }
        Ok(repository
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
    /// Fails if the branch or file does not exist.
    pub fn file(&self, repo: &RepoName, location: &Location) -> Result<Vec<u8>, ServiceError> {
        let repository = self.open(repo)?;
        let entry = Self::resolve(&repository, location)?;
        if entry.kind == EntryKind::Tree {
            return Err(ServiceError::NotAFile(location.path.clone()));
        }
        let blob = repository.objects().read(entry.id)?;
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
    pub fn commits(&self, repo: &RepoName, request: &CommitsRequest) -> Result<Pack, ServiceError> {
        let repository = self.open(repo)?;
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<(ObjectId, u32)> = request.ids.iter().map(|&id| (id, 1)).collect();
        let mut pack = Pack::default();
        while let Some((id, depth)) = queue.pop_front() {
            if depth > request.depth || !seen.insert(id) {
                continue;
            }
            let object = repository
                .objects()
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

    /// Returns the object at each path of a commit, in order.
    ///
    /// # Errors
    ///
    /// Fails if the commit or a path does not exist.
    pub fn objects(&self, repo: &RepoName, request: &ObjectsRequest) -> Result<Pack, ServiceError> {
        let repository = self.open(repo)?;
        let objects = repository.objects();
        let root = objects
            .try_read(request.commit)?
            .ok_or(ServiceError::ObjectNotFound(request.commit))?
            .decode_commit()
            .map_err(|source| StoreError::Decode {
                id: request.commit,
                source,
            })?
            .tree;
        let objects = request
            .paths
            .iter()
            .map(|path| {
                let entry =
                    objects
                        .resolve(root, path)?
                        .ok_or_else(|| ServiceError::NotInCommit {
                            commit: request.commit,
                            path: path.clone(),
                        })?;
                Ok(objects.read(entry.id)?)
            })
            .collect::<Result<_, ServiceError>>()?;
        Ok(Pack { objects })
    }
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
