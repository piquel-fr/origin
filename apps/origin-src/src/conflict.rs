//! Conflicts: what a merge leaves where both sides changed a file.
//!
//! A merge into a working copy, such as `origin converge`, writes both
//! versions of each such text file, between markers naming the sides:
//!
//! ```text
//! <<<<<<< 3f1c9a2e07b4
//! our version
//! =======
//! their version
//! >>>>>>> 8d02e5b1c4f9
//! ```
//!
//! It then issues a `core.conflict` statement about the revision, as its
//! author, a user, listing the paths. Such a revision is unresolved, and the
//! landing queue refuses it. A snapshot of a working copy that is an
//! unresolved revision checks the paths it lists: the new revision is
//! unresolved too, about the paths whose files still hold markers, until
//! none do. Editing the markers out is how a conflict is resolved.
//!
//! The landing queue also issues `core.conflict`, as a node, when a revision
//! doesn't rebase onto its target, even one the queue rebased itself. That
//! one only blocks the landing on that target, which merges again whenever
//! it's retried.

use origin_platform::Platform;
use origin_shared::{
    EncodedObject, Kind, Principal, RecordId, RepoPath, Statement, StatementId, TreeId,
};

use crate::annotations::Claim;
use crate::store::{Store, StoreError};
use crate::tree::Snapshots;

/// A revision's unresolved conflict: the `core.conflict` statement its
/// author issued about it that holds, and the paths it lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The statement.
    pub statement: StatementId,
    /// The paths whose files hold conflict markers.
    pub paths: Vec<RepoPath>,
}

impl Conflict {
    /// The line starting our side, then its label.
    const OURS: &str = "<<<<<<< ";
    /// The line between the sides.
    const BETWEEN: &str = "=======";
    /// The line ending their side, then its label.
    const THEIRS: &str = ">>>>>>> ";

    /// The content of a file both sides changed: `ours`, then `theirs`,
    /// between markers naming them with `labels`. A side that removed the
    /// file is empty. `None` if either side is binary, holding a NUL byte:
    /// markers would only corrupt it.
    #[must_use]
    pub fn mark(ours: &[u8], theirs: &[u8], labels: [&str; 2]) -> Option<Vec<u8>> {
        if ours.contains(&0) || theirs.contains(&0) {
            return None;
        }
        let mut marked = Vec::with_capacity(ours.len() + theirs.len() + 64);
        let side = |marked: &mut Vec<u8>, content: &[u8]| {
            marked.extend_from_slice(content);
            if !content.is_empty() && !content.ends_with(b"\n") {
                marked.push(b'\n');
            }
        };
        marked.extend_from_slice(format!("{}{}\n", Self::OURS, labels[0]).as_bytes());
        side(&mut marked, ours);
        marked.extend_from_slice(format!("{}\n", Self::BETWEEN).as_bytes());
        side(&mut marked, theirs);
        marked.extend_from_slice(format!("{}{}\n", Self::THEIRS, labels[1]).as_bytes());
        Some(marked)
    }

    /// Whether `content` still holds a conflict marker line.
    #[must_use]
    pub fn has_markers(content: &[u8]) -> bool {
        content.split(|&byte| byte == b'\n').any(|line| {
            line == Self::BETWEEN.as_bytes()
                || line.starts_with(Self::OURS.as_bytes())
                || line.starts_with(Self::THEIRS.as_bytes())
        })
    }

    /// The `core.conflict` statement about `subject`, from `issuer`,
    /// listing `paths` one per line in its payload, and the payload.
    #[must_use]
    pub fn statement(
        subject: RecordId,
        issuer: Principal,
        paths: &[RepoPath],
        supersedes: Option<StatementId>,
    ) -> (EncodedObject, Statement) {
        let mut list = String::new();
        for path in paths {
            list.push_str(path.as_str());
            list.push('\n');
        }
        let payload = EncodedObject::attachment(list.as_bytes());
        let statement = Statement {
            subject,
            kind: Kind::CONFLICT,
            verdict: None,
            issuer,
            inputs: None,
            payload: Some(payload.id().into()),
            supersedes,
            promoted_from: None,
        };
        (payload, statement)
    }

    /// The paths a `core.conflict` statement lists: none if retention
    /// dropped its payload.
    ///
    /// # Errors
    ///
    /// Fails if the payload cannot be read.
    pub fn paths<P: Platform>(
        store: &Store<P>,
        statement: &Statement,
    ) -> Result<Vec<RepoPath>, StoreError> {
        let Some(payload) = statement.payload else {
            return Ok(Vec::new());
        };
        let content = store.try_attachment(payload)?.unwrap_or_default();
        Ok(String::from_utf8_lossy(&content)
            .lines()
            .filter_map(|line| RepoPath::new(line).ok())
            .collect())
    }

    /// The conflict of a revision whose author is `author`, from `claims`,
    /// those about it that hold: the latest `core.conflict` from its
    /// author, if that's a user and there's one. A node's says only that a
    /// landing queue could not rebase it.
    ///
    /// # Errors
    ///
    /// Fails if the statement's payload cannot be read.
    pub fn of<P: Platform>(
        store: &Store<P>,
        claims: &[Claim],
        author: &Principal,
    ) -> Result<Option<Self>, StoreError> {
        if matches!(author, Principal::Node(_)) {
            return Ok(None);
        }
        let Some(claim) = claims.iter().rev().find(|claim| {
            claim.statement().kind == Kind::CONFLICT && claim.statement().issuer == *author
        }) else {
            return Ok(None);
        };
        Ok(Some(Self {
            statement: claim.id(),
            paths: Self::paths(store, claim.statement())?,
        }))
    }

    /// The paths of the conflict whose files in `snapshot` still hold
    /// markers.
    ///
    /// # Errors
    ///
    /// Fails if a tree or a file cannot be read.
    pub fn remaining<P: Platform>(
        &self,
        store: &Store<P>,
        snapshot: TreeId,
    ) -> Result<Vec<RepoPath>, StoreError> {
        let mut snapshots = Snapshots::new(store);
        let mut remaining = Vec::new();
        for path in &self.paths {
            let Some(entry) = snapshots.entry_at(snapshot, path)? else {
                continue;
            };
            if !entry.kind.is_tree()
                && store
                    .try_blob(entry.id)?
                    .is_some_and(|content| Self::has_markers(&content))
            {
                remaining.push(path.clone());
            }
        }
        Ok(remaining)
    }
}

#[cfg(test)]
mod tests {
    use origin_platform::Native;
    use origin_shared::Record;

    use super::*;

    #[test]
    fn only_a_user_author_leaves_a_revision_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let subject = RecordId::Snapshot(EncodedObject::blob(b"any").id().into());
        let path = RepoPath::new("file").unwrap();
        let conflicts = |issuer: &str| {
            let (payload, statement) = Conflict::statement(
                subject,
                issuer.parse().unwrap(),
                std::slice::from_ref(&path),
                None,
            );
            store.write_all(&[payload]).unwrap();
            let id = statement.encode().id().into();
            [store.claim(id, statement).unwrap().unwrap()]
        };
        let ada = "user:ada".parse().unwrap();
        let found = Conflict::of(&store, &conflicts("user:ada"), &ada).unwrap();
        assert_eq!(
            found.map(|conflict| conflict.paths),
            Some(vec![path.clone()])
        );
        assert_eq!(
            Conflict::of(&store, &conflicts("user:eve"), &ada).unwrap(),
            None
        );
        // A queue that rebased a revision, then could not rebase it elsewhere.
        let queue = "node:queue".parse().unwrap();
        assert_eq!(
            Conflict::of(&store, &conflicts("node:queue"), &queue).unwrap(),
            None
        );
    }

    #[test]
    fn marks_both_sides_and_finds_the_markers() {
        let marked = Conflict::mark(b"ours\n", b"theirs", ["a", "b"]).unwrap();
        assert_eq!(
            String::from_utf8(marked.clone()).unwrap(),
            "<<<<<<< a\nours\n=======\ntheirs\n>>>>>>> b\n"
        );
        assert!(Conflict::has_markers(&marked));
        assert_eq!(
            Conflict::mark(b"", b"kept\n", ["a", "b"]).unwrap(),
            b"<<<<<<< a\n=======\nkept\n>>>>>>> b\n",
            "a removed side is empty"
        );
        assert_eq!(Conflict::mark(b"\0binary", b"text", ["a", "b"]), None);
        assert!(!Conflict::has_markers(b"a\n== not a marker\n<<<<<<<\n"));
    }
}
