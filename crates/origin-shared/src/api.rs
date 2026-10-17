//! Contracts of the HTTP API, shared by the server and the SDK: requests,
//! responses and errors that aren't records or events themselves.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::access::Capability;
use crate::landing::{Landing, Proposal};
use crate::policy::{KindPattern, Requirement};
use crate::text::Period;
use crate::{
    Acceptance, AcceptanceId, ChangeId, EntryKind, EntryName, Head, ObjectId, Principal,
    PrincipalName, RecordId, RepoPath, Revision, RevisionId, Statement, StatementId, TargetName,
    TreeId,
};

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
}

/// Asks for records by id: revisions, statements and acceptances. Each
/// comes with the attachment it carries, if the repository has it: a
/// revision's description, a statement's payload. Snapshots are fetched
/// with [`FetchSnapshot`] instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FetchRecords {
    /// The records, returned in this order, each followed by its
    /// attachment. Those the caller may not read are left out.
    pub ids: Vec<ObjectId>,
}

/// Asks for the objects of a snapshot that the caller lacks: its trees and
/// blobs, walked from its root. This is the only way blobs and trees leave
/// a repository, so what each caller may read is checked by path: the
/// content of paths they may not read is withheld, though it keeps its
/// place in the trees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FetchSnapshot {
    /// The record whose snapshot it is: a revision or an acceptance. Only a
    /// record's snapshot is walked, never a tree within one, whose paths
    /// would be read as others.
    #[schema(value_type = String)]
    pub of: RecordId,
    /// Snapshots the caller holds whole. Whatever one of them holds at the
    /// same path is skipped, so only what changed since comes. Those the
    /// repository doesn't have are ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub have: Vec<TreeId>,
    /// Only these files and directories, with everything under them, and
    /// the trees on the way to them. Everything by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<RepoPath>,
}

/// Asks what a repository holds of some changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InventoryRequest {
    /// The changes.
    pub changes: Vec<ChangeId>,
}

/// What a repository holds of a change: its records, by id, to compare
/// with another node's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Inventory {
    /// The change.
    pub change: ChangeId,
    /// Its revisions, in the order the repository received them: none if it
    /// has never heard of the change.
    pub revisions: Vec<RevisionId>,
    /// Those no other revision of the change evolves from.
    pub heads: Vec<RevisionId>,
    /// The statements about its revisions, and about those statements, in
    /// no particular order.
    pub statements: Vec<StatementId>,
}

/// What putting a batch of records did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Stored {
    /// How many of the objects the repository didn't have yet.
    pub new: u64,
}

/// Asks for the statements about a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatementsQuery {
    /// The record, such as `revision:<id>`.
    #[param(value_type = String)]
    pub subject: RecordId,
}

/// A statement, and whether its issuer superseded it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Annotation {
    /// The statement's id.
    pub id: StatementId,
    /// The statement.
    pub statement: Statement,
    /// The first statement from the same issuer that supersedes it.
    pub superseded_by: Option<StatementId>,
}

/// The revisions of a change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Revisions {
    /// Every revision, in the order the repository received them.
    pub revisions: Vec<RevisionEntry>,
    /// Those that no other revision of the change evolves from: more than
    /// one is divergence.
    pub heads: Vec<RevisionId>,
}

/// A revision and its id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RevisionEntry {
    /// The revision's id.
    pub id: RevisionId,
    /// The revision.
    pub revision: Revision,
}

/// A target, where it is, and who orders it under the policy in force.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TargetInfo {
    /// The target's name.
    pub name: TargetName,
    /// Its latest acceptance.
    pub head: Head,
    /// The node that orders it, if the policy names one.
    pub authority: Option<PrincipalName>,
    /// The statements landing on it needs.
    pub requirements: Vec<Requirement>,
}

/// Asks for the acceptances of a target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogQuery {
    /// The target.
    #[param(value_type = String)]
    pub target: TargetName,
    /// The position of the first acceptance to return: 0, the first, by
    /// default.
    #[serde(default)]
    pub since: u64,
    /// Return at most this many acceptances: all by default.
    pub limit: Option<u64>,
}

/// One acceptance of a target's log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LogEntry {
    /// Its id.
    pub id: AcceptanceId,
    /// The acceptance, whose `seq` is its position in the log.
    pub acceptance: Acceptance,
}

/// Asks the queue to land something.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LandRequest {
    /// What to land.
    pub proposal: Proposal,
    /// The target to land on. For a change, the target its stack is on by
    /// default; a target needs one.
    pub onto: Option<TargetName>,
}

/// How a landing went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Landings {
    /// The target landed on.
    pub onto: TargetName,
    /// How each landing went: one for each change of a stack that the
    /// target didn't have, from the bottom, up to the first that waits.
    pub landings: Vec<Landing>,
}

/// Asks for a new target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NewTarget {
    /// Its name.
    pub name: TargetName,
    /// The target whose tip it starts at.
    pub from: TargetName,
}

/// Asks for the events after a cursor, waiting for some if there are none
/// yet: a long-poll.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EventsQuery {
    /// The cursor to read after: the `next` of the previous page. By
    /// default, the end of the stream, so only new events come.
    pub cursor: Option<u64>,
    /// How long to wait for events, such as `30s`. Without it, the answer
    /// comes at once, possibly empty.
    #[param(value_type = Option<String>)]
    pub wait: Option<Period>,
    /// Stop after about this many events: an entry's events are delivered
    /// whole.
    pub limit: Option<u32>,
    /// Only events of these names or statement kinds, comma-separated, such
    /// as `check.requested,ci.*`.
    #[serde(default, skip_serializing_if = "List::is_empty")]
    #[param(value_type = Option<String>)]
    pub kind: List<KindPattern>,
    /// Only events whose subject starts with one of these, comma-separated,
    /// such as a revision id prefix, `change:3fa2` or `main`.
    #[serde(default, skip_serializing_if = "List::is_empty")]
    #[param(value_type = Option<String>)]
    pub subject: List<String>,
}

/// Values written as one comma-separated string, as query parameters take
/// lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct List<T>(pub Vec<T>);

impl<T> Default for List<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> List<T> {
    /// Whether the list has no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: fmt::Display> Serialize for List<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let values: Vec<_> = self.0.iter().map(ToString::to_string).collect();
        serializer.serialize_str(&values.join(","))
    }
}

impl<'de, T> Deserialize<'de> for List<T>
where
    T: FromStr<Err: fmt::Display>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.split(',')
            .filter(|value| !value.is_empty())
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Asks a server for a token with a narrower scope, for an agent or a CI
/// runner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TokenRequest {
    /// What it may do: a part of what the caller may do.
    pub scope: Vec<Capability>,
    /// How long it lasts, such as `1h`.
    #[schema(value_type = String)]
    pub ttl: Period,
}

/// A token a server issued.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IssuedToken {
    /// The token, to send as `Authorization: Bearer <token>`.
    pub token: String,
    /// Who it acts for.
    pub principal: Principal,
    /// When it expires, in seconds since the Unix epoch.
    pub expires: u64,
}

/// A snapshot to browse: a target's tip, `main`, or a record's,
/// `revision:<id>` or `acceptance:<id>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum At {
    /// The tip of a target.
    Target(TargetName),
    /// The snapshot of a revision or an acceptance.
    Record(RecordId),
}

impl FromStr for At {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.contains(':') {
            input
                .parse()
                .map(Self::Record)
                .map_err(|error: crate::ParseIdError| error.to_string())
        } else {
            input
                .parse()
                .map(Self::Target)
                .map_err(|error: crate::InvalidName| error.to_string())
        }
    }
}

impl fmt::Display for At {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Target(target) => target.fmt(f),
            Self::Record(record) => record.fmt(f),
        }
    }
}

/// Asks for a directory or a file of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BrowseQuery {
    /// The snapshot: a target, such as `main`, or `revision:<id>` or
    /// `acceptance:<id>`.
    #[param(value_type = String)]
    pub at: At,
    /// The directory or file: the root by default.
    #[serde(default)]
    #[param(value_type = Option<String>)]
    pub path: RepoPath,
}

/// A directory of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Listing {
    /// The snapshot's root tree.
    pub snapshot: TreeId,
    /// The directory.
    pub path: RepoPath,
    /// Its entries, sorted by name.
    pub entries: Vec<Listed>,
}

/// An entry of a [`Listing`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Listed {
    /// Its name.
    pub name: EntryName,
    /// A file, an executable, a symlink or a directory.
    pub kind: EntryKind,
    /// Its object.
    pub id: ObjectId,
    /// Whether its content is withheld from the caller: it keeps its
    /// place, but can't be read.
    pub withheld: bool,
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
    /// The request is malformed, or names records the repository can't
    /// take.
    BadRequest,
    /// A repository, target, change or record does not exist.
    NotFound,
    /// The request is valid, but the repository's state refuses it: a
    /// change that diverged, a target that exists, another node's target.
    Conflict,
    /// The event cursor is older than anything the server still keeps.
    Expired,
    /// The credentials are invalid or expired.
    Unauthorized,
    /// The caller may not do this.
    Forbidden,
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

    #[test]
    fn lists_are_comma_separated() {
        let list: List<KindPattern> = serde_json::from_str("\"ci.*,review\"").unwrap();
        assert_eq!(list.0.len(), 2);
        assert_eq!(serde_json::to_string(&list).unwrap(), "\"ci.*,review\"");
        let empty: List<String> = serde_json::from_str("\"\"").unwrap();
        assert!(empty.is_empty());
        assert!(serde_json::from_str::<List<KindPattern>>("\"CI\"").is_err());
    }
}
