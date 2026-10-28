use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use super::{DecodeError, Field, ObjectKind, Reader, Record, fields, tagged};
use crate::name::names;
use crate::{
    AcceptanceId, BlobId, Digest, InvalidName, ObjectId, ParseIdError, Principal, RepoPath,
    RevisionId, StatementId, TreeEntry, TreeId,
};

/// An attributable claim about another record: a check result, a review, a
/// conflict, an abandonment.
///
/// Statements are permanent. An issuer revises one by issuing another that
/// supersedes it. A promoted statement carries another one over to a record
/// with the same input digest, pointing at it instead of copying its issuer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
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
    /// The statement this one promotes: its kind, verdict, inputs and
    /// payload hold here too, because the input digests are equal. The
    /// issuer is whoever promoted it, the original's stays on the original.
    pub promoted_from: Option<StatementId>,
}

fields!(Statement {
    subject,
    kind,
    verdict,
    issuer,
    inputs,
    payload,
    supersedes,
    promoted_from,
});

impl Record for Statement {
    const KIND: ObjectKind = ObjectKind::Statement;
}

impl Statement {
    /// The BLAKE3 key derivation context of promotion groups.
    const GROUP_CONTEXT: &str = "origin 2026-10 promotion group";

    /// Whether the statement can be promoted to other records: it declares
    /// inputs and depends on nothing outside them, its subject has a
    /// snapshot to take the input digest of (it isn't a statement), it
    /// isn't itself a promotion, whose original is promoted instead, and
    /// its kind isn't reserved: abandonments, conflicts and divergence are
    /// about one revision, whatever its inputs.
    #[must_use]
    pub fn is_promotable(&self) -> bool {
        self.promoted_from.is_none()
            && !self.kind.is_reserved()
            && !matches!(self.subject, RecordId::Statement(_))
            && self.inputs.as_ref().is_some_and(|inputs| !inputs.external)
    }

    /// Identifies the statements this one is interchangeable with when
    /// promoting, if it's promotable: those of its kind, from its issuer,
    /// declaring the same inputs. A record needs only one result of a
    /// group. It hashes the [`Inputs::spec`], then the kind and the issuer
    /// as they're encoded.
    #[must_use]
    pub fn promotion_group(&self) -> Option<Digest> {
        let inputs = self.inputs.as_ref().filter(|_| self.is_promotable())?;
        let mut encoded = inputs.spec().as_bytes().to_vec();
        self.kind.write(&mut encoded);
        self.issuer.write(&mut encoded);
        Some(Digest::from_bytes(blake3::derive_key(
            Self::GROUP_CONTEXT,
            &encoded,
        )))
    }
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

impl RecordId {
    /// The kind of the record's object: a snapshot is a tree.
    #[must_use]
    pub fn kind(self) -> ObjectKind {
        match self {
            Self::Snapshot(_) => ObjectKind::Tree,
            Self::Revision(_) => ObjectKind::Revision,
            Self::Statement(_) => ObjectKind::Statement,
            Self::Acceptance(_) => ObjectKind::Acceptance,
        }
    }
}

impl From<RecordId> for ObjectId {
    fn from(record: RecordId) -> Self {
        match record {
            RecordId::Snapshot(id) => id.into(),
            RecordId::Revision(id) => id.into(),
            RecordId::Statement(id) => id.into(),
            RecordId::Acceptance(id) => id.into(),
        }
    }
}

/// `revision:<id>`: the record's kind, then its full id. A snapshot is
/// `snapshot:<id>`.
impl fmt::Display for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Snapshot(_) => "snapshot",
            Self::Revision(_) => "revision",
            Self::Statement(_) => "statement",
            Self::Acceptance(_) => "acceptance",
        };
        write!(f, "{kind}:{}", ObjectId::from(*self))
    }
}

/// Parses what [`RecordId`] displays as.
impl FromStr for RecordId {
    type Err = ParseIdError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = || ParseIdError {
            what: "record",
            input: input.to_owned(),
        };
        let (kind, id) = input.split_once(':').ok_or_else(invalid)?;
        let id: ObjectId = id.parse().map_err(|_| invalid())?;
        Ok(match kind {
            "snapshot" => Self::Snapshot(id.into()),
            "revision" => Self::Revision(id.into()),
            "statement" => Self::Statement(id.into()),
            "acceptance" => Self::Acceptance(id.into()),
            _ => return Err(invalid()),
        })
    }
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

    /// Whether the kind is reserved for origin itself: `core.` and below.
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        self.0.starts_with("core.")
    }

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

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Neutral => "neutral",
        })
    }
}

/// Parses the names [`Verdict`] displays as: `pass`, `fail` and `neutral`.
impl FromStr for Verdict {
    type Err = InvalidName;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "pass" => Ok(Self::Pass),
            "fail" => Ok(Self::Fail),
            "neutral" => Ok(Self::Neutral),
            _ => Err(InvalidName {
                what: "verdict",
                name: input.to_owned(),
            }),
        }
    }
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
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

impl Inputs {
    /// The BLAKE3 key derivation contexts, which keep these hashes apart
    /// from every other.
    const SPEC_CONTEXT: &str = "origin 2026-10 input spec";
    const DIGEST_CONTEXT: &str = "origin 2026-10 input digest";

    /// The paths, sorted and without duplicates: the order they're declared
    /// in doesn't matter.
    fn sorted_paths(&self) -> Vec<&RepoPath> {
        let mut paths: Vec<_> = self.paths.iter().collect();
        paths.sort_unstable();
        paths.dedup();
        paths
    }

    /// Identifies what the inputs declare: equal for statements reading the
    /// same paths with the same definition and toolchain, whatever their
    /// order. It hashes the definition, the toolchain, and each path's
    /// `u32` length and bytes.
    #[must_use]
    pub fn spec(&self) -> Digest {
        let mut hasher = blake3::Hasher::new_derive_key(Self::SPEC_CONTEXT);
        hasher.update(self.definition.as_bytes());
        hasher.update(self.toolchain.as_bytes());
        for path in self.sorted_paths() {
            let mut encoded = Vec::new();
            Reader::write_str(path.as_str(), &mut encoded);
            hasher.update(&encoded);
        }
        Digest::from_bytes(*hasher.finalize().as_bytes())
    }

    /// The input digest of a snapshot: the [`Inputs::spec`], then for each
    /// path, in its order, a 0 byte if the snapshot has nothing there, or a
    /// 1 byte, the entry's kind and its id. `entry_at` looks up what the
    /// snapshot stores at a path; at the root, that's the snapshot's tree.
    ///
    /// Records with equal digests agree on everything the inputs cover, so
    /// a promotable statement about one holds for the other.
    ///
    /// # Errors
    ///
    /// Fails if `entry_at` does.
    pub fn digest<E>(
        &self,
        mut entry_at: impl FnMut(&RepoPath) -> Result<Option<TreeEntry>, E>,
    ) -> Result<Digest, E> {
        let mut hasher = blake3::Hasher::new_derive_key(Self::DIGEST_CONTEXT);
        hasher.update(self.spec().as_bytes());
        for path in self.sorted_paths() {
            match entry_at(path)? {
                None => hasher.update(&[0]),
                Some(entry) => hasher
                    .update(&[1, entry.kind as u8])
                    .update(entry.id.as_bytes()),
            };
        }
        Ok(Digest::from_bytes(*hasher.finalize().as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::Infallible;

    use super::*;
    use crate::{EncodedObject, EntryKind};

    fn inputs(paths: &[&str], definition: &str) -> Inputs {
        Inputs {
            paths: paths
                .iter()
                .map(|path| RepoPath::new(path).unwrap())
                .collect(),
            definition: Digest::of(definition.as_bytes()),
            toolchain: Digest::of(b"rustc 1.99.0"),
            external: false,
        }
    }

    /// The digest of `inputs` over a snapshot holding `files`.
    fn digest(inputs: &Inputs, files: &[(&str, &str)]) -> Digest {
        let files: HashMap<_, _> = files
            .iter()
            .map(|&(path, content)| {
                let entry = TreeEntry {
                    kind: EntryKind::File,
                    id: EncodedObject::blob(content.as_bytes()).id(),
                };
                (RepoPath::new(path).unwrap(), entry)
            })
            .collect();
        inputs
            .digest(|path| Ok::<_, Infallible>(files.get(path).copied()))
            .unwrap()
    }

    #[test]
    fn digests_cover_exactly_the_inputs() {
        let lint = inputs(&["src", "Cargo.toml"], "cargo clippy");
        let base = digest(&lint, &[("src", "v1"), ("Cargo.toml", "a"), ("docs", "x")]);
        assert_eq!(
            digest(&lint, &[("src", "v1"), ("Cargo.toml", "a"), ("docs", "y")]),
            base,
            "docs isn't an input"
        );
        assert_ne!(
            digest(&lint, &[("src", "v2"), ("Cargo.toml", "a")]),
            base,
            "src is"
        );
        assert_ne!(
            digest(&lint, &[("src", "v1")]),
            base,
            "a missing path differs from any content"
        );
        let reordered = inputs(&["Cargo.toml", "src", "src"], "cargo clippy");
        assert_eq!(
            reordered.spec(),
            lint.spec(),
            "order and repeats don't matter"
        );
        assert_eq!(
            digest(&reordered, &[("src", "v1"), ("Cargo.toml", "a")]),
            base
        );

        let test = inputs(&["src", "Cargo.toml"], "cargo test");
        assert_ne!(test.spec(), lint.spec());
        assert_ne!(digest(&test, &[("src", "v1"), ("Cargo.toml", "a")]), base);
    }

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
            assert!(reserved.is_reserved(), "{reserved:?}");
        }
        assert!(!Kind::new("corex.test").unwrap().is_reserved());
    }

    #[test]
    fn verdicts_parse_as_they_display() {
        for verdict in [Verdict::Pass, Verdict::Fail, Verdict::Neutral] {
            assert_eq!(verdict.to_string().parse::<Verdict>().unwrap(), verdict);
        }
        assert!("Pass".parse::<Verdict>().is_err());
    }
}
