use std::borrow::Cow;

use super::{DecodeError, Field, ObjectKind, Reader, Record, fields, tagged};
use crate::name::names;
use crate::{AcceptanceId, BlobId, Digest, Principal, RepoPath, RevisionId, StatementId, TreeId};

/// An attributable claim about another record: a check result, a review, a
/// conflict, an abandonment.
///
/// Statements are permanent. An issuer revises one by issuing another that
/// supersedes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    /// The record the claim is about.
    pub subject: RecordId,
    /// What kind of claim it is.
    pub kind: Kind,
    /// The outcome, the only field core interprets.
    pub verdict: Option<Verdict>,
    /// Who makes the claim.
    pub issuer: Principal,
    /// What the claim depended on, which lets it be promoted to revisions
    /// with the same inputs.
    pub inputs: Option<Inputs>,
    /// Free-form details, defined by whoever owns the kind.
    pub payload: Option<BlobId>,
    /// A statement from the same issuer that this one replaces.
    pub supersedes: Option<StatementId>,
}

fields!(Statement {
    subject,
    kind,
    verdict,
    issuer,
    inputs,
    payload,
    supersedes,
});

impl Record for Statement {
    const KIND: ObjectKind = ObjectKind::Statement;
}

/// A record a statement can be about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RecordId {
    /// A snapshot.
    Snapshot(TreeId),
    /// A revision.
    Revision(RevisionId),
    /// Another statement, such as a failed check marked flaky.
    Statement(StatementId),
    /// An acceptance.
    Acceptance(AcceptanceId),
}

// The tags are the records' object kinds.
tagged!(RecordId {
    Snapshot = 2,
    Revision = 3,
    Statement = 4,
    Acceptance = 5,
});

names! {
    /// The kind of a statement, namespaced by its owner: `ci.test`,
    /// `review`, `core.conflict`. Dot-separated segments of lowercase ASCII
    /// letters, digits, `_` and `-`.
    Kind("statement kind");
}

impl Kind {
    /// The author gave the change up. The payload says why.
    pub const ABANDON: Self = Self(Cow::Borrowed("core.abandon"));
    /// A landing could not rebase. The payload lists the conflicting paths.
    pub const CONFLICT: Self = Self(Cow::Borrowed("core.conflict"));
    /// The change has more than one head.
    pub const DIVERGENT: Self = Self(Cow::Borrowed("core.divergent"));

    fn is_valid(name: &str) -> bool {
        name.split('.').all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
                })
        })
    }
}

/// The outcome of a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Verdict {
    /// The check passed, or the reviewer approves.
    Pass = 1,
    /// The check failed, or the reviewer objects.
    Fail = 2,
    /// Neither: informational.
    Neutral = 3,
}

impl Field for Verdict {
    fn write(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match reader.u8()? {
            1 => Ok(Self::Pass),
            2 => Ok(Self::Fail),
            3 => Ok(Self::Neutral),
            tag => Err(DecodeError::UnknownTag {
                field: "Verdict",
                tag,
            }),
        }
    }
}

/// What a statement depended on.
///
/// The input digest of a snapshot hashes the ids it stores at `paths`, with
/// `definition` and `toolchain`. A statement that declares inputs and isn't
/// `external` is promoted to every revision with the same input digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inputs {
    /// The files and directories read, each covering everything under it.
    pub paths: Vec<RepoPath>,
    /// The check's command and configuration.
    pub definition: Digest,
    /// The tools that ran it, such as the hash of `rustc -V`.
    pub toolchain: Digest,
    /// Whether the claim also depended on things outside the snapshot, such
    /// as the network, which makes it never promotable.
    pub external: bool,
}

fields!(Inputs {
    paths,
    definition,
    toolchain,
    external,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_validated() {
        for valid in ["ci.test", "review", "semantic.diff", "a_b.c-d.e1"] {
            assert!(Kind::new(valid).is_ok(), "{valid:?}");
        }
        for invalid in [
            "", "ci.", ".ci", "ci..test", "CI.test", "ci test", "ci/test",
        ] {
            assert!(Kind::new(invalid).is_err(), "{invalid:?}");
        }
        for reserved in [Kind::ABANDON, Kind::CONFLICT, Kind::DIVERGENT] {
            assert!(Kind::is_valid(reserved.as_str()), "{reserved:?}");
        }
    }
}
