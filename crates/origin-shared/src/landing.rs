//! Landings: what a target's queue is asked to land, and how it went.

use std::fmt;

use crate::policy::Requirement;
use crate::{ChangeId, Head, RecordId, RepoPath, Source, StatementId, TargetName};

/// What a landing lands.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Proposal {
    /// A change, as of its head.
    Change(ChangeId),
    /// Another target, as of its tip.
    Target(TargetName),
}

/// How a landing went.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Landing {
    /// It landed.
    Landed {
        /// What landed.
        proposal: Proposal,
        /// The revision or acceptance the acceptance names.
        source: Source,
        /// The acceptance.
        acceptance: Head,
    },
    /// The target already has it, so nothing landed.
    Unchanged(Proposal),
    /// It waits, and the queue keeps it.
    Pending(Pending),
}

/// A landing that waits, kept in the queue to be listed and retried.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Pending {
    /// The target it lands on.
    pub onto: TargetName,
    /// What lands.
    pub proposal: Proposal,
    /// What the evidence must be about: the revision that would land, or
    /// the merged snapshot of a target. A conflict is about the revision,
    /// or the acceptance, that didn't merge.
    pub subject: RecordId,
    /// Why it waits.
    pub blocked: Blocked,
}

/// Why a landing waits.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Blocked {
    /// The target requires these statements about the subject.
    Evidence(Vec<Requirement>),
    /// Both sides changed these paths. The `core.conflict` statement lists
    /// them too.
    Conflict {
        /// The statement.
        statement: StatementId,
        /// The paths, sorted.
        paths: Vec<RepoPath>,
    },
}

impl fmt::Display for Proposal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Change(change) => write!(f, "change {}", change.short()),
            Self::Target(target) => write!(f, "`{target}`"),
        }
    }
}
