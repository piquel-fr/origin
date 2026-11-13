//! Statements: issuing them, reading them, and promoting them by input
//! digest.
//!
//! Core interprets only the reserved kinds: [`Kind::ABANDON`],
//! [`Kind::CONFLICT`] and [`Kind::DIVERGENT`], and only from the issuers
//! [`Store::authorizes`] names. Every other kind belongs to whoever issues
//! it. Supersession counts only within one issuer: a statement claiming to
//! supersede another issuer's is issued like any other, and the one it
//! names still stands. [`Annotations::supersede`] supersedes an issuer's
//! current statement atomically, so the statements of one kind about one
//! record it issues in one store form a single chain.

mod promotion;

use std::collections::HashSet;

use origin_platform::{Native, Platform};
use origin_shared::{
    ChangeId, EncodedObject, Kind, Principal, Record, RecordId, RevisionId, Statement, StatementId,
};

use crate::index::Index;
use crate::store::{Store, StoreError};

pub use promotion::{Claim, InvalidPromotion};

/// The statements of a store, through its index.
#[derive(Debug)]
pub struct Annotations<'a, P = Native> {
    store: &'a Store<P>,
    index: Index,
}

/// A statement, and whether its issuer superseded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
    /// The statement's id.
    pub id: StatementId,
    /// The statement.
    pub statement: Statement,
    /// The first statement from the same issuer that supersedes it.
    pub superseded_by: Option<StatementId>,
}

impl<'a, P: Platform> Annotations<'a, P> {
    /// The statements of `store`, as `index` knows them.
    pub fn new(store: &'a Store<P>, index: Index) -> Self {
        Self { store, index }
    }

    /// The statements of `store`, opening its index.
    ///
    /// # Errors
    ///
    /// Fails like [`Index::open`].
    pub fn open(store: &'a Store<P>) -> Result<Self, StoreError> {
        Ok(Self::new(store, Index::open(store)?))
    }

    /// The index the statements are found through.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Writes a statement and its payload. Issuing a statement the store
    /// has is a no-op.
    ///
    /// # Errors
    ///
    /// Fails if the statement supersedes one from another issuer, is a
    /// promotion that [`Store::claim`] refuses, is of a reserved kind its
    /// issuer may not issue, or the records cannot be read or written.
    pub fn issue(
        &self,
        statement: &Statement,
        payload: Option<EncodedObject>,
    ) -> Result<StatementId, AnnotateError> {
        if !self.store.authorizes(statement)? {
            return Err(AnnotateError::Reserved {
                kind: statement.kind.clone(),
                issuer: statement.issuer.clone(),
            });
        }
        if statement.promoted_from.is_some() {
            self.store
                .claim(statement.encode().id().into(), statement.clone())??;
        }
        if let Some(superseded) = statement.supersedes {
            let issuer = self.store.get(superseded)?.issuer;
            if issuer != statement.issuer {
                return Err(AnnotateError::ForeignSupersession {
                    superseded,
                    issuer,
                    by: statement.issuer.clone(),
                });
            }
        }
        Ok(self.write(statement, payload)?)
    }

    /// Issues `statement` as its issuer's latest of its kind about its
    /// subject, superseding the [current](Self::current) one, and returns
    /// it with its id. One without a verdict, of a kind that isn't
    /// reserved, withdraws the current one. It reads the current one and
    /// issues under a lock, from the index read again, so two statements
    /// issued at once in this store never supersede the same one: the
    /// second supersedes the first. Statements issued in other stores and
    /// put here chose what they supersede there, so two of one issuer's can
    /// still both supersede the same one.
    ///
    /// # Errors
    ///
    /// Fails like [`Annotations::issue`], or with
    /// [`AnnotateError::NothingToWithdraw`] if a withdrawal finds no
    /// current statement with a verdict.
    pub fn supersede(
        &mut self,
        mut statement: Statement,
        payload: Option<EncodedObject>,
    ) -> Result<(StatementId, Statement), AnnotateError> {
        let _lock = self.store.lock_supersession()?;
        self.index = Index::open(self.store)?;
        statement.supersedes =
            self.current(statement.subject, &statement.kind, &statement.issuer)?;
        if statement.verdict.is_none() && !statement.kind.is_reserved() {
            // A withdrawal takes back a statement with a verdict.
            let withdrawable = match statement.supersedes {
                Some(current) => !self.store.get(current)?.withdraws(),
                None => false,
            };
            if !withdrawable {
                return Err(AnnotateError::NothingToWithdraw {
                    kind: statement.kind,
                    subject: statement.subject,
                });
            }
        }
        let id = self.issue(&statement, payload)?;
        Ok((id, statement))
    }

    /// Writes a statement after its payload, which is durable before
    /// anything refers to it.
    fn write(
        &self,
        statement: &Statement,
        payload: Option<EncodedObject>,
    ) -> Result<StatementId, StoreError> {
        if let Some(payload) = payload {
            self.store.write_all(&[payload])?;
        }
        let encoded = statement.encode();
        self.store.write_all(std::slice::from_ref(&encoded))?;
        Ok(encoded.id().into())
    }

    /// The first statement superseding `statement`, `id`, that its issuer
    /// issued.
    fn superseded_by(
        &self,
        id: StatementId,
        statement: &Statement,
    ) -> Result<Option<StatementId>, StoreError> {
        for &superseder in self.index.superseders(id) {
            if self.store.get(superseder)?.issuer == statement.issuer {
                return Ok(Some(superseder));
            }
        }
        Ok(None)
    }

    /// The statements about `subject`, in arrival order.
    ///
    /// # Errors
    ///
    /// Fails if a statement cannot be read.
    pub fn about(&self, subject: RecordId) -> Result<Vec<Annotation>, StoreError> {
        self.index
            .statements(subject)
            .iter()
            .map(|&id| {
                let statement = self.store.get(id)?;
                Ok(Annotation {
                    id,
                    superseded_by: self.superseded_by(id, &statement)?,
                    statement,
                })
            })
            .collect()
    }

    /// The latest statement of `kind` from `issuer` about `subject` that
    /// nothing superseded: the one a new such statement supersedes.
    ///
    /// # Errors
    ///
    /// Fails if a statement cannot be read.
    pub fn current(
        &self,
        subject: RecordId,
        kind: &Kind,
        issuer: &Principal,
    ) -> Result<Option<StatementId>, StoreError> {
        Ok(self
            .about(subject)?
            .into_iter()
            .rev()
            .find(|annotation| {
                annotation.superseded_by.is_none()
                    && annotation.statement.kind == *kind
                    && annotation.statement.issuer == *issuer
            })
            .map(|annotation| annotation.id))
    }

    /// Issues a `core.divergent` statement as `issuer` if `change` has more
    /// than one head, returning it. It is about the head with the smallest
    /// id, and its payload lists every head, one id per line, so the same
    /// heads always make the same statement: flagging them again writes
    /// nothing.
    ///
    /// # Errors
    ///
    /// Fails if the statement cannot be written.
    pub fn flag_divergence(
        &self,
        change: ChangeId,
        issuer: Principal,
    ) -> Result<Option<StatementId>, StoreError> {
        let mut heads = self.index.heads(change);
        if heads.len() < 2 {
            return Ok(None);
        }
        heads.sort_unstable();
        let list = heads
            .iter()
            .map(|head| head.to_hex() + "\n")
            .collect::<String>();
        let payload = EncodedObject::blob(list.as_bytes());
        let statement = Statement {
            subject: RecordId::Revision(heads[0]),
            kind: Kind::DIVERGENT,
            verdict: None,
            issuer,
            inputs: None,
            payload: Some(payload.id().into()),
            supersedes: None,
            promoted_from: None,
        };
        self.write(&statement, Some(payload)).map(Some)
    }
}

impl<P: Platform> Store<P> {
    /// Whether `statement`'s issuer may issue a statement of its kind, if
    /// that kind is reserved:
    ///
    /// - `core.abandon`, about a revision, only the change's creator: the
    ///   author of its first revision, so no one who took it over, or
    ///   rebased it, gives it up for them;
    /// - `core.conflict`, a node, as a landing queue that could not rebase
    ///   the record, or the author of the revision it's about, listing
    ///   the conflicts its files still have;
    /// - `core.divergent`, a node.
    ///
    /// Reserved kinds are never promoted. Other kinds are anyone's, as far
    /// as core is concerned.
    ///
    /// # Errors
    ///
    /// Fails if a revision cannot be read.
    pub fn authorizes(&self, statement: &Statement) -> Result<bool, StoreError> {
        let issuer = &statement.issuer;
        if !statement.kind.is_reserved() {
            return Ok(true);
        }
        if statement.promoted_from.is_some() {
            return Ok(false);
        }
        let is_node = matches!(issuer, Principal::Node(_));
        let RecordId::Revision(subject) = statement.subject else {
            return Ok(statement.kind == Kind::CONFLICT && is_node);
        };
        let kind = &statement.kind;
        Ok(if *kind == Kind::ABANDON {
            self.created(subject, issuer)?
        } else if *kind == Kind::CONFLICT {
            is_node || self.get(subject)?.author == *issuer
        } else {
            *kind == Kind::DIVERGENT && is_node
        })
    }

    /// Whether `author` created the change of `revision`: made each
    /// revision it evolves from that evolves from none. Records being
    /// immutable, there is always one, and no one adds another later.
    fn created(&self, revision: RevisionId, author: &Principal) -> Result<bool, StoreError> {
        let mut seen = HashSet::new();
        let mut next = vec![revision];
        while let Some(id) = next.pop() {
            if !seen.insert(id) {
                continue;
            }
            let revision = self.get(id)?;
            if revision.predecessors.is_empty() && revision.author != *author {
                return Ok(false);
            }
            next.extend(revision.predecessors);
        }
        Ok(true)
    }
}

/// A failure to issue a statement.
#[derive(Debug, thiserror::Error)]
pub enum AnnotateError {
    /// The statement supersedes one from another issuer.
    #[error("{by} cannot supersede statement {}, which {issuer} issued", superseded.short())]
    ForeignSupersession {
        /// The statement it supersedes.
        superseded: StatementId,
        /// Who issued that one.
        issuer: Principal,
        /// Who issues the new one.
        by: Principal,
    },
    /// A promotion doesn't match its original.
    #[error("invalid promotion")]
    InvalidPromotion(#[from] InvalidPromotion),
    /// A reserved kind from an issuer that may not issue it.
    #[error("{issuer} may not issue `{kind}` statements about this record")]
    Reserved {
        /// The kind.
        kind: Kind,
        /// Who issued it.
        issuer: Principal,
    },
    /// A withdrawal found no statement of its issuer's to take back.
    #[error(
        "you have no `{kind}` statement about {} to withdraw",
        subject.describe()
    )]
    NothingToWithdraw {
        /// The kind.
        kind: Kind,
        /// What it would be about: a revision or a statement.
        subject: RecordId,
    },
    /// Records cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use origin_shared::{Base, BlobId, Digest, Inputs, ObjectId, RepoPath, Revision, Verdict};

    use super::*;
    use crate::tree::tests::build;

    pub(super) fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        (dir, store)
    }

    pub(super) fn principal(name: &str) -> Principal {
        name.parse().unwrap()
    }

    /// Writes a revision of `change` with `files`, evolving from
    /// `predecessors`.
    pub(super) fn revision(
        store: &Store,
        change: u8,
        predecessors: &[RevisionId],
        files: &[(&str, &str)],
    ) -> RevisionId {
        let encoded = Revision {
            change: ChangeId::from_bytes([change; 16]),
            predecessors: predecessors.to_vec(),
            base: Base::Acceptance(ObjectId::from_bytes([0; 32]).into()),
            deps: Vec::new(),
            snapshot: build(store, files),
            description: BlobId::from(EncodedObject::blob(b"").id()),
            author: principal("user:alice"),
        }
        .encode();
        store.write_all(std::slice::from_ref(&encoded)).unwrap();
        encoded.id().into()
    }

    /// A check result about `revision` from `issuer`.
    pub(super) fn check(
        revision: RevisionId,
        kind: &str,
        verdict: Verdict,
        issuer: &str,
        inputs: Option<(&[&str], bool)>,
    ) -> Statement {
        Statement {
            subject: RecordId::Revision(revision),
            kind: Kind::new(kind).unwrap(),
            verdict: Some(verdict),
            issuer: principal(issuer),
            inputs: inputs.map(|(paths, external)| Inputs {
                paths: paths
                    .iter()
                    .map(|path| RepoPath::new(path).unwrap())
                    .collect(),
                definition: Digest::of(kind.as_bytes()),
                toolchain: Digest::of(b"rustc 1.99.0"),
                external,
            }),
            payload: None,
            supersedes: None,
            promoted_from: None,
        }
    }

    /// Issues `statement` through a fresh index.
    pub(super) fn issue(store: &Store, statement: &Statement) -> StatementId {
        Annotations::open(store)
            .unwrap()
            .issue(statement, None)
            .unwrap()
    }

    /// The kinds and originals of what `promote` issues to `revision`.
    pub(super) fn promote(
        store: &Store,
        revision: RevisionId,
    ) -> Vec<(String, Option<StatementId>)> {
        let promoter = principal("node:queue");
        Annotations::open(store)
            .unwrap()
            .promote(revision, &promoter)
            .unwrap()
            .into_iter()
            .map(|(_, statement)| {
                assert_eq!(statement.subject, RecordId::Revision(revision));
                assert_eq!(statement.issuer, promoter);
                (statement.kind.to_string(), statement.promoted_from)
            })
            .collect()
    }

    pub(super) const FILES: &[(&str, &str)] = &[("src/lib.rs", "v1"), ("docs/guide.md", "v1")];

    #[test]
    fn statements_issued_at_once_supersede_one_another() {
        let (_dir, store) = store();
        let revision = revision(&store, 1, &[], FILES);
        let rounds = 50;
        for _ in 0..rounds {
            let barrier = Barrier::new(2);
            thread::scope(|scope| {
                // Different verdicts, so the two never make one statement.
                for verdict in [Verdict::Pass, Verdict::Fail] {
                    let (store, barrier) = (&store, &barrier);
                    scope.spawn(move || {
                        let mut annotations = Annotations::open(store).unwrap();
                        let statement = check(revision, "review", verdict, "user:alice", None);
                        barrier.wait();
                        annotations.supersede(statement, None).unwrap();
                    });
                }
            });
        }
        let about = Annotations::open(&store)
            .unwrap()
            .about(RecordId::Revision(revision))
            .unwrap();
        assert_eq!(about.len(), 2 * rounds);
        for pair in about.windows(2) {
            assert_eq!(pair[1].statement.supersedes, Some(pair[0].id));
        }
    }

    #[test]
    fn only_the_issuer_supersedes_its_results() {
        let (_dir, store) = store();
        let first = revision(&store, 1, &[], FILES);
        let inputs = Some((&["src"][..], false));
        let pass = issue(
            &store,
            &check(first, "lint", Verdict::Pass, "node:ci", inputs),
        );
        let target = revision(&store, 2, &[], FILES);

        // Another issuer's claim to supersede it doesn't count.
        let mut foreign = check(first, "lint", Verdict::Fail, "node:other", None);
        foreign.supersedes = Some(pass);
        let annotations = Annotations::open(&store).unwrap();
        assert!(matches!(
            annotations.issue(&foreign, None),
            Err(AnnotateError::ForeignSupersession { .. })
        ));
        let written = foreign.encode();
        store.write_all(std::slice::from_ref(&written)).unwrap();
        assert_eq!(
            Annotations::open(&store)
                .unwrap()
                .current(
                    RecordId::Revision(first),
                    &Kind::new("lint").unwrap(),
                    &principal("node:ci")
                )
                .unwrap(),
            Some(pass),
            "a fetched one stands as a statement of its own"
        );

        // The issuer's own does: the rerun's failure is promoted instead.
        let mut rerun = check(first, "lint", Verdict::Fail, "node:ci", inputs);
        rerun.supersedes = Some(pass);
        let fail = issue(&store, &rerun);
        let about = Annotations::open(&store)
            .unwrap()
            .about(RecordId::Revision(first))
            .unwrap();
        assert_eq!(about[0].superseded_by, Some(fail));
        assert_eq!(promote(&store, target), [("lint".to_owned(), Some(fail))]);
    }

    #[test]
    fn flags_each_divergence_once() {
        let (_dir, store) = store();
        let node = principal("node:laptop");
        let first = revision(&store, 1, &[], FILES);
        assert_eq!(
            Annotations::open(&store)
                .unwrap()
                .flag_divergence(ChangeId::from_bytes([1; 16]), node.clone())
                .unwrap(),
            None
        );
        let left = revision(&store, 1, &[first], &[("a", "left")]);
        let right = revision(&store, 1, &[first], &[("a", "right")]);
        let flag = || {
            Annotations::open(&store)
                .unwrap()
                .flag_divergence(ChangeId::from_bytes([1; 16]), node.clone())
                .unwrap()
                .unwrap()
        };
        let flagged = flag();
        assert_eq!(flag(), flagged, "the same heads, the same statement");

        let subject = RecordId::Revision(left.min(right));
        let about = Annotations::open(&store).unwrap().about(subject).unwrap();
        assert_eq!(about.len(), 1);
        assert_eq!(about[0].statement.kind, Kind::DIVERGENT);
        let heads = store
            .try_blob(about[0].statement.payload.unwrap().into())
            .unwrap()
            .unwrap();
        let mut expected = [left, right];
        expected.sort();
        assert_eq!(
            heads,
            format!("{}\n{}\n", expected[0], expected[1]).into_bytes()
        );
    }

    #[test]
    fn reserved_kinds_have_authors() {
        let (_dir, store) = store();
        let first = revision(&store, 1, &[], FILES);
        // The queue rebases it: its author is the queue, but alice created
        // the change.
        let rebased = Revision {
            predecessors: vec![first],
            author: principal("node:queue"),
            ..store.get(first).unwrap()
        }
        .encode();
        store.write_all(std::slice::from_ref(&rebased)).unwrap();
        let reserved = |kind: Kind, issuer: &str| Statement {
            subject: RecordId::Revision(rebased.id().into()),
            kind,
            verdict: None,
            issuer: principal(issuer),
            inputs: None,
            payload: None,
            supersedes: None,
            promoted_from: None,
        };
        let authorizes = |statement: &Statement| store.authorizes(statement).unwrap();
        assert!(authorizes(&reserved(Kind::ABANDON, "user:alice")));
        assert!(
            !authorizes(&reserved(Kind::ABANDON, "node:queue")),
            "it only rebased the change"
        );
        assert!(!authorizes(&reserved(Kind::ABANDON, "user:eve")));
        assert!(authorizes(&reserved(Kind::CONFLICT, "node:queue")));
        assert!(
            !authorizes(&reserved(Kind::CONFLICT, "user:alice")),
            "not her revision"
        );
        assert!(authorizes(&reserved(Kind::DIVERGENT, "node:laptop")));
        assert!(!authorizes(&reserved(Kind::DIVERGENT, "user:alice")));
        let other = reserved(Kind::new("core.other").unwrap(), "node:queue");
        assert!(!authorizes(&other));

        let annotations = Annotations::open(&store).unwrap();
        let foreign = reserved(Kind::ABANDON, "user:eve");
        assert!(matches!(
            annotations.issue(&foreign, None),
            Err(AnnotateError::Reserved { .. })
        ));
        let mut promotable = reserved(Kind::ABANDON, "user:alice");
        promotable.inputs = check(
            first,
            "x",
            Verdict::Pass,
            "node:ci",
            Some((&["src"], false)),
        )
        .inputs;
        assert!(!promotable.is_promotable(), "reserved kinds never promote");
    }

    #[test]
    fn withdrawals_have_nothing_to_promote() {
        let (_dir, store) = store();
        let first = revision(&store, 1, &[], FILES);
        let inputs = Some((&["src"][..], false));
        let result = check(first, "ci.test", Verdict::Pass, "node:ci", inputs);
        assert!(result.is_promotable() && !result.withdraws());
        let withdrawal = Statement {
            verdict: None,
            supersedes: Some(result.encode().id().into()),
            ..result
        };
        assert!(withdrawal.withdraws());
        assert!(!withdrawal.is_promotable());
    }

    #[test]
    fn only_the_creator_of_a_change_abandons_it() {
        let (_dir, store) = store();
        let first = revision(&store, 1, &[], FILES);
        // Eve takes the change over, then another root of it of hers
        // merges in: alice still created the change, and eve never did.
        let write = |predecessors: Vec<RevisionId>, author: &str| {
            let encoded = Revision {
                predecessors,
                author: principal(author),
                ..store.get(first).unwrap()
            }
            .encode();
            store.write_all(std::slice::from_ref(&encoded)).unwrap();
            RevisionId::from(encoded.id())
        };
        let taken = write(vec![first], "user:eve");
        let root = write(Vec::new(), "user:eve");
        let merged = write(vec![taken, root], "user:eve");
        let abandons = |subject, issuer: &str| {
            let statement = Statement {
                subject: RecordId::Revision(subject),
                kind: Kind::ABANDON,
                verdict: None,
                issuer: principal(issuer),
                inputs: None,
                payload: None,
                supersedes: None,
                promoted_from: None,
            };
            store.authorizes(&statement).unwrap()
        };
        assert!(abandons(taken, "user:alice"));
        assert!(!abandons(taken, "user:eve"), "she took it over");
        assert!(!abandons(merged, "user:eve"), "alice created it");
        assert!(!abandons(merged, "user:alice"), "eve created it too");
    }
}
