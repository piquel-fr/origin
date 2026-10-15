//! Contracts of the HTTP API, shared by the server and the SDK.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{BranchName, EntryKind, ObjectId, RepoPath};

/// The name of a repository on a server: ASCII letters, digits, `.`, `_`
/// and `-`, not starting with `.`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String, example = "origin")]
pub struct RepoName(String);

impl RepoName {
    /// Validates a repository name.
    ///
    /// # Errors
    ///
    /// Fails if the name breaks the rules above.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidRepoName> {
        let name = name.into();
        let valid = !name.is_empty()
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
        if valid {
            Ok(Self(name))
        } else {
            Err(InvalidRepoName(name))
        }
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RepoName {
    type Error = InvalidRepoName;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl From<RepoName> for String {
    fn from(name: RepoName) -> Self {
        name.0
    }
}

impl FromStr for RepoName {
    type Err = InvalidRepoName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::new(name)
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// A repository name that [`RepoName::new`] rejected.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid repository name {0:?}")]
pub struct InvalidRepoName(pub String);

/// A repository, as listed by the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RepoSummary {
    /// The repository's name.
    pub name: RepoName,
    /// What the repository holds.
    pub description: String,
}

/// A repository with its branches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RepoInfo {
    /// The repository's name.
    pub name: RepoName,
    /// What the repository holds.
    pub description: String,
    /// Every branch, sorted by name.
    pub branches: Vec<Branch>,
}

/// A branch and the commit it points to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Branch {
    /// The branch's name.
    pub name: BranchName,
    /// The commit it points to.
    pub commit: ObjectId,
}

/// Where to look in a repository: a path on a branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct Location {
    /// The branch to read.
    pub branch: BranchName,
    /// The path to read, empty for the root.
    #[serde(default)]
    pub path: RepoPath,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ListedEntry {
    /// The entry's name.
    pub name: String,
    /// What the entry is.
    pub kind: EntryKind,
    /// The object it points to.
    pub id: ObjectId,
}

/// Asks for commits and their ancestors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CommitsRequest {
    /// The commits to start from.
    pub ids: Vec<ObjectId>,
    /// How many generations to return: 1 returns only `ids`, 2 adds their
    /// parents, and so on.
    pub depth: u32,
}

/// Asks for the objects at some paths of a commit: trees for directories,
/// blobs for files.
///
/// Objects are requested by path rather than by id so the server knows
/// where in the repository each one lives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ObjectsRequest {
    /// The commit whose tree the paths are in.
    pub commit: ObjectId,
    /// The paths to read. The empty path is the root tree.
    pub paths: Vec<RepoPath>,
}

/// What went wrong with a request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ApiError {
    /// The kind of failure.
    pub code: ErrorCode,
    /// A human-readable explanation.
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// The kind of an [`ApiError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request is malformed.
    BadRequest,
    /// A repository, branch, path, or object does not exist.
    NotFound,
    /// A branch is not where a push expected it: someone else pushed first.
    Conflict,
    /// The server failed.
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_are_validated() {
        for valid in ["origin", "my-repo.v2", "a_b"] {
            assert!(RepoName::new(valid).is_ok(), "{valid:?}");
        }
        for invalid in ["", ".hidden", "a/b", "sp ace"] {
            assert!(RepoName::new(invalid).is_err(), "{invalid:?}");
        }
    }
}
