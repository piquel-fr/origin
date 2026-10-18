use super::{ObjectKind, Record, fields, tagged};
use crate::{AcceptanceId, BlobId, ChangeId, Principal, RevisionId, TreeId};

/// One immutable version of a change.
///
/// Editing, rebasing or handing a change to someone else creates a new
/// revision of the same change, whose predecessors are the revisions it
/// evolves from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    /// The change this is a version of.
    pub change: ChangeId,
    /// The revisions this one evolves from. Two make it a convergence of
    /// divergent heads.
    pub predecessors: Vec<RevisionId>,
    /// What the revision was made on top of.
    pub base: Base,
    /// The exact revisions this one requires: the changes below it in a
    /// stack.
    pub deps: Vec<RevisionId>,
    /// The root tree of the files.
    pub snapshot: TreeId,
    /// The intent, written for humans and agents.
    pub description: BlobId,
    /// Who wrote the revision.
    pub author: Principal,
}

fields!(Revision {
    change,
    predecessors,
    base,
    deps,
    snapshot,
    description,
    author,
});

impl Record for Revision {
    const KIND: ObjectKind = ObjectKind::Revision;
}

/// What a revision was made on top of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Base {
    /// The state of a target after one of its acceptances.
    Acceptance(AcceptanceId),
    /// Another revision, such as the one below in a stack.
    Revision(RevisionId),
}

tagged!(Base {
    Acceptance = 1,
    Revision = 2,
});
