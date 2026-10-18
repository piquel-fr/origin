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
//! A binary file, or a path a directory is on either side of, keeps one
//! side instead. The merge then issues a `core.conflict` statement about
//! the new revision, as its author, a user, listing each conflicted path
//! and the entry it wrote there. That list is the conflict, not the
//! markers, which only help to edit the file: a path stays unresolved while
//! a revision has the entry written there. Changing the file resolves it,
//! and so does `origin resolve`, which accepts it as it is, such as the
//! side a binary file kept.
//!
//! Every revision made from one with unresolved paths, by a snapshot,
//! `describe`, a new change on top, or a merge, carries those it still has
//! the entries of over, in a `core.conflict` of its own, published before
//! the revision itself, so it's never found without it. The landing queue
//! refuses a revision whose author's latest `core.conflict` lists a path it
//! still has the entry of.
//!
//! The landing queue also issues `core.conflict`, as a node, when a revision
//! doesn't rebase onto its target, even one the queue rebased itself. That
//! one only blocks the landing on that target, which merges again whenever
//! it's retried.

use origin_platform::Platform;
use origin_shared::{
    EncodedObject, Kind, ObjectId, Principal, RecordId, RepoPath, Revision, Statement, StatementId,
    TreeId,
};

use crate::annotations::Claim;
use crate::store::{Store, StoreError};
use crate::tree::Snapshots;

/// A path a merge left conflicted, and the entry it wrote there.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Conflicted {
    /// The path.
    pub path: RepoPath,
    /// The id of the entry written there: both versions between markers,
    /// or the side kept. `None` if the merge left nothing there.
    pub written: Option<ObjectId>,
}

/// Conflicted paths, sorted, each at most once: those a merge left, or a
/// revision's unresolved ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conflicts(Vec<Conflicted>);

impl Conflicts {
    /// The line starting our side, then its label.
    const OURS: &str = "<<<<<<< ";
    /// The line between the sides.
    const BETWEEN: &str = "=======";
    /// The line ending their side, then its label.
    const THEIRS: &str = ">>>>>>> ";

    /// The conflicts of `conflicted`, the latest of each path counting.
    #[must_use]
    pub fn new(mut conflicted: Vec<Conflicted>) -> Self {
        // Stable, so the latest of each path comes last, and dedup keeps the
        // first of equal ones: reverse around it.
        conflicted.reverse();
        conflicted.sort_by(|a, b| a.path.cmp(&b.path));
        conflicted.dedup_by(|a, b| a.path == b.path);
        Self(conflicted)
    }

    /// Adds `conflicted`, which must come after every path listed.
    pub(crate) fn push(&mut self, conflicted: Conflicted) {
        debug_assert!(self.0.last().is_none_or(|last| last.path < conflicted.path));
        self.0.push(conflicted);
    }

    /// Those of these conflicts whose path `keep` keeps.
    #[must_use]
    pub fn only(mut self, keep: impl Fn(&RepoPath) -> bool) -> Self {
        self.0.retain(|conflicted| keep(&conflicted.path));
        self
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The conflicted paths, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<RepoPath> {
        self.0
            .iter()
            .map(|conflicted| conflicted.path.clone())
            .collect()
    }

    /// These conflicts, then `newer` ones, which replace those of the same
    /// paths: what a merge of a revision with these conflicts leaves.
    #[must_use]
    pub fn with(mut self, newer: Self) -> Self {
        self.0.extend(newer.0);
        Self::new(self.0)
    }

    /// Resolves the conflicts of `paths`, returning those it had.
    pub fn resolve(&mut self, paths: &[RepoPath]) -> Vec<RepoPath> {
        let mut resolved = Vec::new();
        self.0.retain(|conflicted| {
            let listed = paths.contains(&conflicted.path);
            if listed {
                resolved.push(conflicted.path.clone());
            }
            !listed
        });
        resolved
    }

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

    /// The `core.conflict` statement about `subject`, from `issuer`,
    /// listing these conflicts in its payload, and the payload: each one
    /// as the id written, or `-` for none, a space and the path, ended by a
    /// NUL byte, which no path holds.
    #[must_use]
    pub fn statement(
        &self,
        subject: RecordId,
        issuer: Principal,
        supersedes: Option<StatementId>,
    ) -> (EncodedObject, Statement) {
        let mut list = String::new();
        for conflicted in &self.0 {
            match conflicted.written {
                Some(id) => list.push_str(&id.to_string()),
                None => list.push('-'),
            }
            list.push(' ');
            list.push_str(conflicted.path.as_str());
            list.push('\0');
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

    /// The conflicts a `core.conflict` statement lists: none if retention
    /// dropped its payload.
    ///
    /// # Errors
    ///
    /// Fails if the payload cannot be read.
    pub fn listed<P: Platform>(
        store: &Store<P>,
        statement: &Statement,
    ) -> Result<Self, StoreError> {
        let Some(payload) = statement.payload else {
            return Ok(Self::default());
        };
        let content = store.try_attachment(payload)?.unwrap_or_default();
        let conflicted = String::from_utf8_lossy(&content)
            .split_terminator('\0')
            .filter_map(|line| {
                let (written, path) = line.split_once(' ')?;
                let written = match written {
                    "-" => None,
                    id => Some(id.parse().ok()?),
                };
                let path = RepoPath::new(path).ok()?;
                Some(Conflicted { path, written })
            })
            .collect();
        Ok(Self::new(conflicted))
    }

    /// The unresolved conflicts of `revision`, from `claims`, those about it
    /// that hold: of the latest `core.conflict` from its author, if that's
    /// a user, the paths where it still has the entry written. A node's
    /// says only that a landing queue could not rebase it.
    ///
    /// # Errors
    ///
    /// Fails if the statement's payload, or a tree on the way to a path,
    /// cannot be read.
    pub fn of<P: Platform>(
        store: &Store<P>,
        claims: &[Claim],
        revision: &Revision,
    ) -> Result<Self, StoreError> {
        Self::latest(store, claims.iter().map(Claim::statement), revision)
    }

    /// The unresolved conflicts of `revision`, as [`Conflicts::of`] finds
    /// them, from `statements` about it, in arrival order.
    ///
    /// # Errors
    ///
    /// Fails like [`Conflicts::of`].
    pub fn latest<'s, P: Platform>(
        store: &Store<P>,
        statements: impl IntoIterator<Item = &'s Statement>,
        revision: &Revision,
    ) -> Result<Self, StoreError> {
        if matches!(revision.author, Principal::Node(_)) {
            return Ok(Self::default());
        }
        let latest = statements.into_iter().filter(|statement| {
            statement.kind == Kind::CONFLICT && statement.issuer == revision.author
        });
        let Some(statement) = latest.last() else {
            return Ok(Self::default());
        };
        Self::listed(store, statement)?.remaining(store, revision.snapshot)
    }

    /// Those of these conflicts `snapshot` still has: where it has the
    /// entry written.
    ///
    /// # Errors
    ///
    /// Fails if a tree on the way to a path cannot be read.
    pub fn remaining<P: Platform>(
        mut self,
        store: &Store<P>,
        snapshot: TreeId,
    ) -> Result<Self, StoreError> {
        let mut snapshots = Snapshots::new(store);
        let mut remaining = Vec::with_capacity(self.0.len());
        for conflicted in self.0.drain(..) {
            let entry = snapshots.entry_at(snapshot, &conflicted.path)?;
            if entry.map(|entry| entry.id) == conflicted.written {
                remaining.push(conflicted);
            }
        }
        Ok(Self(remaining))
    }
}

#[cfg(test)]
mod tests {
    use origin_platform::Native;
    use origin_shared::{EntryKind, EntryName, Record, Tree, TreeEntry};

    use super::*;

    fn path(path: &str) -> RepoPath {
        RepoPath::new(path).unwrap()
    }

    #[test]
    fn a_path_resolves_once_its_entry_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let marked = EncodedObject::blob(b"<<<<<<< a\nours\n=======\ntheirs\n>>>>>>> b\n");
        let resolved = EncodedObject::blob(b"Heading\n=======\nresolved\n");
        let snapshot = |file: &EncodedObject| {
            let mut tree = Tree::default();
            tree.insert(
                EntryName::new("file").unwrap(),
                TreeEntry {
                    kind: EntryKind::File,
                    id: file.id(),
                },
            );
            let tree = tree.encode();
            store.write_all(&[file.clone(), tree.clone()]).unwrap();
            TreeId::from(tree.id())
        };
        let conflicts = Conflicts::new(vec![
            Conflicted {
                path: path("file"),
                written: Some(marked.id()),
            },
            Conflicted {
                path: path("removed"),
                written: None,
            },
        ]);
        let (payload, statement) = conflicts.statement(
            RecordId::Snapshot(snapshot(&marked)),
            "user:ada".parse().unwrap(),
            None,
        );
        store.write_all(&[payload]).unwrap();
        assert_eq!(Conflicts::listed(&store, &statement).unwrap(), conflicts);

        let unchanged = conflicts.clone().remaining(&store, snapshot(&marked));
        assert_eq!(unchanged.unwrap(), conflicts);
        // Even to content that looks like a marker.
        let remaining = conflicts.remaining(&store, snapshot(&resolved)).unwrap();
        assert_eq!(remaining.paths(), [path("removed")]);
    }

    #[test]
    fn only_a_user_author_leaves_a_revision_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        let tree = Tree::default().encode();
        store.write_all(std::slice::from_ref(&tree)).unwrap();
        let revision = |author: &str| Revision {
            change: origin_shared::ChangeId::from_bytes([1; 16]),
            predecessors: Vec::new(),
            base: origin_shared::Base::Acceptance(EncodedObject::blob(b"base").id().into()),
            deps: Vec::new(),
            snapshot: tree.id().into(),
            description: EncodedObject::blob(b"").id().into(),
            author: author.parse().unwrap(),
        };
        let subject = RecordId::Revision(revision("user:ada").encode().id().into());
        let conflicts = Conflicts::new(vec![Conflicted {
            path: path("file"),
            written: None,
        }]);
        let claims = |issuer: &str| {
            let (payload, statement) = conflicts.statement(subject, issuer.parse().unwrap(), None);
            store.write_all(&[payload]).unwrap();
            let id = statement.encode().id().into();
            [store.claim(id, statement).unwrap().unwrap()]
        };
        let ada = revision("user:ada");
        assert_eq!(
            Conflicts::of(&store, &claims("user:ada"), &ada).unwrap(),
            conflicts
        );
        assert!(
            Conflicts::of(&store, &claims("user:eve"), &ada)
                .unwrap()
                .is_empty()
        );
        // A queue that rebased a revision, then could not rebase it elsewhere.
        let queue = revision("node:queue");
        assert!(
            Conflicts::of(&store, &claims("node:queue"), &queue)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn newer_conflicts_replace_older_ones() {
        let older = Conflicts::new(vec![
            Conflicted {
                path: path("a"),
                written: None,
            },
            Conflicted {
                path: path("b"),
                written: None,
            },
        ]);
        let id = EncodedObject::blob(b"newer").id();
        let newer = Conflicts::new(vec![Conflicted {
            path: path("b"),
            written: Some(id),
        }]);
        let mut both = older.with(newer);
        assert_eq!(
            both,
            Conflicts::new(vec![
                Conflicted {
                    path: path("a"),
                    written: None,
                },
                Conflicted {
                    path: path("b"),
                    written: Some(id),
                },
            ])
        );
        assert_eq!(both.resolve(&[path("a"), path("c")]), [path("a")]);
        assert_eq!(both.paths(), [path("b")]);
    }

    #[test]
    fn marks_both_sides() {
        let marked = Conflicts::mark(b"ours\n", b"theirs", ["a", "b"]).unwrap();
        assert_eq!(
            String::from_utf8(marked).unwrap(),
            "<<<<<<< a\nours\n=======\ntheirs\n>>>>>>> b\n"
        );
        assert_eq!(
            Conflicts::mark(b"", b"kept\n", ["a", "b"]).unwrap(),
            b"<<<<<<< a\n=======\nkept\n>>>>>>> b\n",
            "a removed side is empty"
        );
        assert_eq!(Conflicts::mark(b"\0binary", b"text", ["a", "b"]), None);
    }
}
