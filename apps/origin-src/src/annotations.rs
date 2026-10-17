//! Statements: issuing them, reading them, promoting them by input digest,
//! and dropping their payloads once retention ends.
//!
//! Core interprets only the reserved kinds: [`Kind::ABANDON`],
//! [`Kind::CONFLICT`] and [`Kind::DIVERGENT`], and only from the issuers
//! [`Store::authorizes`] names. Every other kind belongs to whoever issues
//! it. Supersession counts only within one issuer: a statement claiming to
//! supersede another issuer's is issued like any other, and the one it
//! names still stands.
//!
//! A promotion claims what its original does, so it counts only once
//! checked against it: [`Store::claim`] checks it, and only that makes a
//! [`Claim`]. Whatever reads results, such as the landing queue, reads
//! claims, through [`Annotations::claims`].
//!
//! Promotion runs on demand, never as a side effect of reading: `origin
//! promote`, and the landing queue when it checks evidence, call
//! [`Annotations::promote`]. The index groups promotable statements by
//! kind, issuer and declared inputs, so the search costs one lookup per
//! input path for each group, then a comparison per statement against
//! digests the index caches. A statement's digest is computed from its
//! subject's trees the first time a search meets it.

use std::collections::HashSet;
use std::time::SystemTime;

use origin_platform::{Native, Platform};
use origin_shared::api::Annotation;
use origin_shared::{
    ChangeId, EncodedObject, Kind, ObjectId, Principal, Record, RecordId, RevisionId, Statement,
    StatementId, TreeId,
};

use crate::index::Index;
use crate::policy::Retention;
use crate::store::{Store, StoreError};
use crate::tree::Snapshots;

/// The statements of a store, through its index.
#[derive(Debug)]
pub struct Annotations<'a, P = Native> {
    store: &'a Store<P>,
    index: Index,
}

/// The claim a statement makes, checked: only [`Store::claim`] makes one.
/// A statement claims for itself, unless it promotes another. Then it makes
/// its original's claim, kind, verdict and issuer included, which holds
/// because their subjects' input digests are equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    id: StatementId,
    statement: Statement,
    /// The original a promotion carries, checked.
    original: Option<(StatementId, Statement)>,
}

impl Claim {
    /// The statement making the claim, which evidence cites.
    #[must_use]
    pub fn id(&self) -> StatementId {
        self.id
    }

    /// The statement making the claim.
    #[must_use]
    pub fn statement(&self) -> &Statement {
        &self.statement
    }

    /// What is claimed: the statement, or for a promotion its original,
    /// whose kind, verdict and issuer count.
    #[must_use]
    pub fn claimed(&self) -> &Statement {
        self.original
            .as_ref()
            .map_or(&self.statement, |(_, original)| original)
    }

    /// The original the statement promotes, if it's a promotion.
    #[must_use]
    pub fn original(&self) -> Option<StatementId> {
        self.original.as_ref().map(|&(id, _)| id)
    }
}

/// What garbage collection dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// The payloads deleted.
    pub payloads: usize,
    /// The bytes that freed on disk.
    pub bytes: u64,
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

    /// Gives the index back, for the next to use it.
    #[must_use]
    pub fn into_index(self) -> Index {
        self.index
    }

    /// Catches the index up with the records written since it was read,
    /// see [`Index::refresh`].
    ///
    /// # Errors
    ///
    /// Fails like [`Index::refresh`].
    pub fn refresh(&mut self) -> Result<(), StoreError> {
        self.index.refresh(self.store)
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

    /// Writes a statement after its payload, which is durable before
    /// anything refers to it, and stays until then.
    fn write(
        &self,
        statement: &Statement,
        payload: Option<EncodedObject>,
    ) -> Result<StatementId, StoreError> {
        let _held = self.store.hold_objects()?;
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

    /// The claims about `subject` that hold, in arrival order: those of
    /// the statements nothing superseded, that [`Store::claim`] accepts with
    /// the records this node has, and for a promotion, whose original
    /// nothing superseded either.
    ///
    /// # Errors
    ///
    /// Fails if a record or a tree cannot be read.
    pub fn claims(&self, subject: RecordId) -> Result<Vec<Claim>, StoreError> {
        let mut claims = Vec::new();
        for annotation in self.about(subject)? {
            if annotation.superseded_by.is_some() {
                continue;
            }
            let claim = match self.store.claim(annotation.id, annotation.statement) {
                Ok(Ok(claim)) => claim,
                // A clone may lack the trees a promotion is checked with.
                Ok(Err(_)) | Err(StoreError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            if let Some((id, original)) = &claim.original
                && self.superseded_by(*id, original)?.is_some()
            {
                continue;
            }
            claims.push(claim);
        }
        Ok(claims)
    }

    /// Computes and remembers the input digest of every promotable
    /// statement the index has none for, from every core. Those whose
    /// subject or trees aren't in the store yet are left for a later search.
    fn compute_digests(&mut self) -> Result<(), StoreError> {
        let missing: Vec<StatementId> = self
            .index
            .promotable()
            .flat_map(|(_, statements)| statements)
            .filter(|&&id| self.index.digest(id).is_none())
            .copied()
            .collect();
        let chunks = Store::<P>::on_every_core(&missing, |ids| {
            let mut snapshots = Snapshots::new(self.store);
            let mut digests = Vec::new();
            for &id in ids {
                let statement = self.store.get(id)?;
                let Some(inputs) = &statement.inputs else {
                    continue;
                };
                let digest = self
                    .store
                    .snapshot_of(statement.subject)
                    .and_then(|snapshot| {
                        snapshot
                            .map(|snapshot| snapshots.input_digest(snapshot, inputs))
                            .transpose()
                    });
                match digest {
                    Ok(Some(digest)) => digests.push((id, digest)),
                    Ok(None) | Err(StoreError::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(digests)
        });
        let digests = chunks.into_iter().collect::<Result<Vec<_>, _>>()?;
        self.index.remember_digests(self.store, &digests.concat())
    }

    /// The statements promoting results to `subject`, as `issuer` would
    /// issue them, without writing them, sorted by kind: for each
    /// [promotion group](Statement::promotion_group), the latest statement
    /// about another record with the same input digest as the subject,
    /// unless its issuer superseded it. Groups whose kind and issuer the
    /// subject already has a result of, directly or promoted, are skipped:
    /// a result that ran there wins. A statement has no snapshot, so
    /// nothing is promoted to one.
    ///
    /// # Errors
    ///
    /// Fails if a record or tree cannot be read, or computed digests cannot
    /// be remembered.
    pub fn promotions(
        &mut self,
        subject: RecordId,
        issuer: &Principal,
    ) -> Result<Vec<Statement>, StoreError> {
        let Some(snapshot) = self.store.snapshot_of(subject)? else {
            return Ok(Vec::new());
        };
        self.compute_digests()?;

        // The results the revision has that hold, by kind and original
        // issuer.
        let held: HashSet<_> = self
            .claims(subject)?
            .iter()
            .map(|claim| (claim.claimed().kind.clone(), claim.claimed().issuer.clone()))
            .collect();

        let mut snapshots = Snapshots::new(self.store);
        let mut promotions = Vec::new();
        for (_, statements) in self.index.promotable() {
            // A group shares its kind, issuer and inputs.
            let first = self.store.get(statements[0])?;
            let Some(inputs) = &first.inputs else {
                continue;
            };
            if held.contains(&(first.kind.clone(), first.issuer.clone())) {
                continue;
            }
            let digest = snapshots.input_digest(snapshot, inputs)?;
            for &id in statements.iter().rev() {
                if self.index.digest(id) != Some(digest) {
                    continue;
                }
                let statement = self.store.get(id)?;
                if self.superseded_by(id, &statement)?.is_none() {
                    promotions.push(Statement {
                        subject,
                        issuer: issuer.clone(),
                        supersedes: None,
                        promoted_from: Some(id),
                        ..statement
                    });
                    break;
                }
            }
        }
        promotions
            .sort_unstable_by(|a, b| (&a.kind, a.promoted_from).cmp(&(&b.kind, b.promoted_from)));
        Ok(promotions)
    }

    /// Issues the [`promotions`](Self::promotions) to `subject` as
    /// `issuer`, returning them.
    ///
    /// # Errors
    ///
    /// Fails like [`Annotations::promotions`], or if the statements cannot
    /// be written.
    pub fn promote(
        &mut self,
        subject: RecordId,
        issuer: &Principal,
    ) -> Result<Vec<(StatementId, Statement)>, StoreError> {
        let promotions = self.promotions(subject, issuer)?;
        let encoded: Vec<_> = promotions.iter().map(Statement::encode).collect();
        self.store.write_all(&encoded)?;
        Ok(encoded
            .iter()
            .map(|encoded| encoded.id().into())
            .zip(promotions)
            .collect())
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

    /// Deletes the payloads whose retention ended by `now`, counted from
    /// when this node stored their statement. A payload stays while any
    /// statement still keeps it, or a revision or a tree refers to the same
    /// blob, such as an empty file and an empty payload. Statements stay.
    /// It waits for the writers [holding objects](Store::hold_objects), and
    /// reads the index again once they're done, so a writer never refers
    /// to a payload deleted under it.
    ///
    /// # Errors
    ///
    /// Fails if a record cannot be read or a payload deleted.
    pub fn drop_expired_payloads(
        &mut self,
        retention: &Retention,
        now: SystemTime,
    ) -> Result<Dropped, StoreError> {
        if retention.is_empty() {
            return Ok(Dropped::default());
        }
        let _collecting = self.store.lock_for_collection()?;
        self.index = Index::open(self.store)?;
        let mut expired = HashSet::new();
        let mut live: HashSet<ObjectId> = HashSet::new();
        let mut roots = Vec::new();
        for &record in self.index.arrivals() {
            match record {
                RecordId::Revision(id) => {
                    let revision = self.store.get(id)?;
                    roots.push(revision.snapshot);
                    live.insert(revision.description.into());
                }
                RecordId::Acceptance(id) => roots.push(self.store.get(id)?.body().state),
                RecordId::Statement(id) => {
                    let statement = self.store.get(id)?;
                    if let RecordId::Snapshot(tree) = statement.subject {
                        roots.push(tree);
                    }
                    let Some(payload) = statement.payload else {
                        continue;
                    };
                    let ended = match retention.period(&statement.kind) {
                        Some(period) => {
                            let stored = self.store.stored_at(id.into())?;
                            now.duration_since(stored).is_ok_and(|age| age >= period)
                        }
                        None => false,
                    };
                    if ended {
                        expired.insert(payload.into());
                    } else {
                        live.insert(payload.into());
                    }
                }
                RecordId::Snapshot(_) => {}
            }
        }
        if expired.is_empty() {
            return Ok(Dropped::default());
        }
        self.store.reach(roots, &mut live)?;
        let mut doomed = Vec::new();
        for &id in expired.difference(&live) {
            if self.store.contains(id)? {
                doomed.push(id);
            }
        }
        Ok(Dropped {
            payloads: doomed.len(),
            bytes: self.store.remove(&doomed)?,
        })
    }
}

impl<P: Platform> Store<P> {
    /// The snapshot of a record a statement is about, which its input
    /// digest is taken from. Statements have none.
    ///
    /// # Errors
    ///
    /// Fails if the record cannot be read.
    pub fn snapshot_of(&self, subject: RecordId) -> Result<Option<TreeId>, StoreError> {
        Ok(match subject {
            RecordId::Snapshot(id) => Some(id),
            RecordId::Revision(id) => Some(self.get(id)?.snapshot),
            RecordId::Acceptance(id) => Some(self.get(id)?.body().state),
            RecordId::Statement(_) => None,
        })
    }

    /// Checks the claim `statement`, `id`, makes. A statement that promotes
    /// none makes its own. A promotion must name an original the store has
    /// that is [promotable](Statement::is_promotable), match its kind,
    /// verdict, inputs and payload, and be about a record whose input
    /// digest equals the original's subject's: then it makes the
    /// original's claim, or else the claim is invalid, which is the inner
    /// error. Whether anyone superseded either is left to the reader, see
    /// [`Annotations::claims`].
    ///
    /// # Errors
    ///
    /// Fails if a record or a tree cannot be read.
    pub fn claim(
        &self,
        id: StatementId,
        statement: Statement,
    ) -> Result<Result<Claim, InvalidPromotion>, StoreError> {
        let Some(original_id) = statement.promoted_from else {
            return Ok(Ok(Claim {
                id,
                statement,
                original: None,
            }));
        };
        let original: Statement = match self.get(original_id) {
            Ok(original) => original,
            Err(StoreError::NotFound(_)) => {
                return Ok(Err(InvalidPromotion::UnknownOriginal(original_id)));
            }
            Err(error) => return Err(error),
        };
        if !original.is_promotable() {
            return Ok(Err(InvalidPromotion::NotPromotable(original_id)));
        }
        let differs = [
            ("kind", statement.kind != original.kind),
            ("verdict", statement.verdict != original.verdict),
            ("inputs", statement.inputs != original.inputs),
            ("payload", statement.payload != original.payload),
        ];
        if let Some(&(field, _)) = differs.iter().find(|(_, differs)| *differs) {
            return Ok(Err(InvalidPromotion::Differs(field)));
        }
        let (Some(here), Some(there), Some(inputs)) = (
            self.snapshot_of(statement.subject)?,
            self.snapshot_of(original.subject)?,
            &original.inputs,
        ) else {
            return Ok(Err(InvalidPromotion::NoSnapshot));
        };
        let mut snapshots = Snapshots::new(self);
        if snapshots.input_digest(here, inputs)? != snapshots.input_digest(there, inputs)? {
            return Ok(Err(InvalidPromotion::InputsDiffer));
        }
        Ok(Ok(Claim {
            id,
            statement,
            original: Some((original_id, original)),
        }))
    }

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

/// Why a promotion makes no claim.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPromotion {
    /// The store doesn't have the original.
    #[error("it promotes statement {}, which this repository doesn't have", .0.short())]
    UnknownOriginal(StatementId),
    /// The original declares no inputs, depends on external ones, is
    /// reserved, about a statement, or a promotion itself.
    #[error("it promotes statement {}, which isn't promotable", .0.short())]
    NotPromotable(StatementId),
    /// A field differs from the original's.
    #[error("its {0} differs from its original's")]
    Differs(&'static str),
    /// Its subject is a statement, which has no snapshot.
    #[error("it is about a statement, which has no inputs")]
    NoSnapshot,
    /// Its subject's input digest differs from the original's subject's.
    #[error("its subject's inputs differ from its original's")]
    InputsDiffer,
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
    /// Records cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use origin_shared::{Base, BlobId, Digest, Inputs, RepoPath, Revision, RevisionId, Verdict};

    use super::*;
    use crate::tree::tests::build;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        (dir, store)
    }

    fn principal(name: &str) -> Principal {
        name.parse().unwrap()
    }

    /// Writes a revision of `change` with `files`, evolving from
    /// `predecessors`.
    fn revision(
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
    fn check(
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
    fn issue(store: &Store, statement: &Statement) -> StatementId {
        Annotations::open(store)
            .unwrap()
            .issue(statement, None)
            .unwrap()
    }

    /// The kinds and originals of what `promote` issues to `revision`.
    fn promote(store: &Store, revision: RevisionId) -> Vec<(String, Option<StatementId>)> {
        let promoter = principal("node:queue");
        Annotations::open(store)
            .unwrap()
            .promote(RecordId::Revision(revision), &promoter)
            .unwrap()
            .into_iter()
            .map(|(_, statement)| {
                assert_eq!(statement.subject, RecordId::Revision(revision));
                assert_eq!(statement.issuer, promoter);
                (statement.kind.to_string(), statement.promoted_from)
            })
            .collect()
    }

    const FILES: &[(&str, &str)] = &[("src/lib.rs", "v1"), ("docs/guide.md", "v1")];

    #[test]
    fn promotes_results_whose_inputs_are_unchanged() {
        let (_dir, store) = store();
        let first = revision(&store, 1, &[], FILES);
        let lint = issue(
            &store,
            &check(
                first,
                "lint",
                Verdict::Pass,
                "node:ci",
                Some((&["src"], false)),
            ),
        );
        let docs = check(
            first,
            "docs",
            Verdict::Pass,
            "node:ci",
            Some((&["src", "docs"], false)),
        );
        issue(&store, &docs);
        let links = check(
            first,
            "links",
            Verdict::Pass,
            "node:ci",
            Some((&["docs"], true)),
        );
        issue(&store, &links);
        issue(
            &store,
            &check(first, "review", Verdict::Pass, "user:bob", None),
        );

        // A docs-only change keeps lint, and nothing that reads docs.
        let docs_only = revision(
            &store,
            2,
            &[],
            &[("src/lib.rs", "v1"), ("docs/guide.md", "v2")],
        );
        assert_eq!(
            promote(&store, docs_only),
            [("lint".to_owned(), Some(lint))]
        );
        assert_eq!(promote(&store, docs_only), [], "promoted once");

        // An unchanged snapshot keeps every promotable result, never the
        // external one or the review.
        let same = revision(&store, 3, &[], FILES);
        let kinds: Vec<_> = promote(&store, same)
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(kinds, ["docs", "lint"]);
        assert_eq!(promote(&store, first), [], "it has its own results");

        let source = revision(
            &store,
            4,
            &[],
            &[("src/lib.rs", "v2"), ("docs/guide.md", "v1")],
        );
        assert_eq!(promote(&store, source), []);

        // Promoted results aren't promoted again: their originals are.
        let again = revision(&store, 5, &[], FILES);
        let originals: Vec<_> = promote(&store, again).into_iter().map(|(_, o)| o).collect();
        assert!(originals.contains(&Some(lint)), "{originals:?}");
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
    fn direct_results_win_over_promotion() {
        let (_dir, store) = store();
        let inputs = Some((&["src"][..], false));
        let first = revision(&store, 1, &[], FILES);
        issue(
            &store,
            &check(first, "lint", Verdict::Pass, "node:ci", inputs),
        );
        let second = revision(&store, 2, &[], FILES);
        issue(
            &store,
            &check(second, "lint", Verdict::Fail, "node:ci", inputs),
        );
        assert_eq!(promote(&store, second), [], "it ran there, and failed");

        // Of several matching results, the latest is promoted.
        let third = revision(&store, 3, &[], FILES);
        let promoted = promote(&store, third);
        let second_result = Annotations::open(&store)
            .unwrap()
            .current(
                RecordId::Revision(second),
                &Kind::new("lint").unwrap(),
                &principal("node:ci"),
            )
            .unwrap();
        assert_eq!(promoted, [("lint".to_owned(), second_result)]);
    }

    #[test]
    fn digests_are_remembered_across_rebuilds() {
        let (_dir, store) = store();
        let first = revision(&store, 1, &[], FILES);
        let lint = issue(
            &store,
            &check(
                first,
                "lint",
                Verdict::Pass,
                "node:ci",
                Some((&["src"], false)),
            ),
        );
        promote(&store, revision(&store, 2, &[], FILES));

        let index = Index::open(&store).unwrap();
        assert!(index.digest(lint).is_some());
        assert_eq!(Index::rebuild(&store).unwrap().index, index);
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
    fn retention_drops_payloads_and_keeps_statements() {
        let (_dir, store) = store();
        let first = revision(&store, 1, &[], &[("empty", "")]);
        let with_payload = |kind: &str, payload: &[u8]| {
            let payload = EncodedObject::blob(payload);
            let mut statement = check(first, kind, Verdict::Pass, "node:ci", None);
            statement.payload = Some(payload.id().into());
            let annotations = Annotations::open(&store).unwrap();
            let id = annotations
                .issue(&statement, Some(payload.clone()))
                .unwrap();
            (id, payload.id())
        };
        let (log_statement, log) = with_payload("ci.test", b"a long log");
        // The same blob as the empty file, which must stay.
        let (_, empty) = with_payload("ci.lint", b"");
        let (_, review) = with_payload("review", b"looks good");
        let (_, shared) = with_payload("ci.bench", b"shared");
        // Also the payload of a statement whose retention hasn't ended.
        let mut kept = check(first, "keep", Verdict::Pass, "node:ci", None);
        kept.payload = Some(shared.into());
        issue(&store, &kept);

        let retention: Retention = toml::from_str::<crate::policy::Policy>(
            "[retention]\n\"ci.*\" = \"1d\"\n\"keep\" = \"1000d\"\n",
        )
        .unwrap()
        .retention;
        let mut annotations = Annotations::open(&store).unwrap();
        let now = SystemTime::now();
        assert_eq!(
            annotations.drop_expired_payloads(&retention, now).unwrap(),
            Dropped::default(),
            "not a day old yet"
        );
        let later = now + Duration::from_hours(48);
        let dropped = annotations
            .drop_expired_payloads(&retention, later)
            .unwrap();
        assert_eq!(dropped.payloads, 1);
        assert!(!store.contains(log).unwrap());
        for stays in [empty, review, shared] {
            assert!(store.contains(stays).unwrap(), "{stays}");
        }
        assert!(store.get(log_statement).is_ok(), "statements are permanent");
    }

    #[test]
    fn promotions_must_match_their_original() {
        let (_dir, store) = store();
        let inputs = Some((&["src"][..], false));
        let first = revision(&store, 1, &[], FILES);
        let mut failed = check(first, "lint", Verdict::Fail, "node:ci", inputs);
        let failure = issue(&store, &failed);
        let second = revision(&store, 2, &[], FILES);
        let promotion = |original: StatementId, statement: &Statement| Statement {
            subject: RecordId::Revision(second),
            issuer: principal("user:eve"),
            promoted_from: Some(original),
            ..statement.clone()
        };
        let annotations = Annotations::open(&store).unwrap();
        let refused = |statement: &Statement| match annotations.issue(statement, None) {
            Err(AnnotateError::InvalidPromotion(why)) => why,
            other => panic!("{other:?}"),
        };

        // Turning a failure into a pass.
        let forged = Statement {
            verdict: Some(Verdict::Pass),
            ..promotion(failure, &failed)
        };
        assert_eq!(refused(&forged), InvalidPromotion::Differs("verdict"));
        // Carrying it where the inputs differ.
        let changed = revision(&store, 3, &[], &[("src/lib.rs", "v2")]);
        let moved = Statement {
            subject: RecordId::Revision(changed),
            ..promotion(failure, &failed)
        };
        assert_eq!(refused(&moved), InvalidPromotion::InputsDiffer);
        // Carrying a result that read the network.
        failed.inputs.as_mut().unwrap().external = true;
        let external = issue(&store, &failed);
        assert_eq!(
            refused(&promotion(external, &failed)),
            InvalidPromotion::NotPromotable(external)
        );
        let unknown = EncodedObject::blob(b"nothing").id().into();
        assert_eq!(
            refused(&promotion(unknown, &failed)),
            InvalidPromotion::UnknownOriginal(unknown)
        );

        // Written anyway, as a fetch could, none of them claims anything,
        // and a valid one claims as the original's issuer.
        let valid = promotion(
            failure,
            &check(first, "lint", Verdict::Fail, "node:ci", inputs),
        );
        let records: Vec<_> = [forged, moved, valid]
            .iter()
            .map(Statement::encode)
            .collect();
        store.write_all(&records).unwrap();
        let claims = |subject| Annotations::open(&store).unwrap().claims(subject).unwrap();
        assert_eq!(claims(RecordId::Revision(changed)), []);
        let [claim] = &claims(RecordId::Revision(second))[..] else {
            panic!("only the valid promotion claims")
        };
        assert_eq!(claim.original(), Some(failure));
        assert_eq!(claim.claimed().issuer, principal("node:ci"));
        assert_eq!(claim.claimed().verdict, Some(Verdict::Fail));
    }

    #[test]
    fn superseded_promotions_hold_nothing() {
        let (_dir, store) = store();
        let inputs = Some((&["src"][..], false));
        let first = revision(&store, 1, &[], FILES);
        let original = check(first, "lint", Verdict::Pass, "node:ci", inputs);
        let id = issue(&store, &original);
        let second = revision(&store, 2, &[], FILES);
        assert_eq!(promote(&store, second), [("lint".to_owned(), Some(id))]);

        // The rerun replaces the original, so the promotion of it no longer
        // holds, and the rerun is promoted in its place.
        let rerun = issue(
            &store,
            &Statement {
                supersedes: Some(id),
                ..original
            },
        );
        let annotations = Annotations::open(&store).unwrap();
        assert_eq!(annotations.claims(RecordId::Revision(second)).unwrap(), []);
        assert_eq!(promote(&store, second), [("lint".to_owned(), Some(rerun))]);
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

    #[test]
    fn collection_waits_for_writers_holding_objects() {
        let (_dir, store) = store();
        let first = revision(&store, 1, &[], FILES);
        let payload = EncodedObject::blob(b"a log, and a file");
        let mut statement = check(first, "ci.test", Verdict::Pass, "node:ci", None);
        statement.payload = Some(payload.id().into());
        Annotations::open(&store)
            .unwrap()
            .issue(&statement, Some(payload.clone()))
            .unwrap();
        let retention = toml::from_str::<crate::policy::Policy>("[retention]\n\"ci.*\" = \"1s\"\n")
            .unwrap()
            .retention;

        // A writer finds the expired payload stored as a file it snapshots,
        // so it doesn't write it, then writes the revision referring to it.
        let held = store.hold_objects().unwrap();
        assert_eq!(store.write_all(std::slice::from_ref(&payload)).unwrap(), 0);
        let (done, collected) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let later = SystemTime::now() + Duration::from_hours(1);
                let dropped = Annotations::open(&store)
                    .unwrap()
                    .drop_expired_payloads(&retention, later)
                    .unwrap();
                done.send(dropped).unwrap();
            });
            thread::sleep(Duration::from_millis(100));
            assert!(collected.try_recv().is_err(), "collection waits");
            revision(&store, 2, &[], &[("log", "a log, and a file")]);
            drop(held);
            assert_eq!(collected.recv().unwrap(), Dropped::default());
        });
        assert!(store.contains(payload.id()).unwrap());
    }

    #[test]
    fn writers_never_trust_a_pack_collection_rewrote() {
        let (dir, store) = store();
        let first = revision(&store, 1, &[], FILES);
        let payload = EncodedObject::blob(b"expired, then needed again");
        let mut statement = check(first, "ci.test", Verdict::Pass, "node:ci", None);
        statement.payload = Some(payload.id().into());
        Annotations::open(&store)
            .unwrap()
            .issue(&statement, Some(payload.clone()))
            .unwrap();
        store.compact().unwrap();
        // Another process, which loaded the pack holding the payload.
        let writer = Store::new(dir.path().join(".origin"), Native::default());
        assert!(writer.contains(payload.id()).unwrap());

        let retention = toml::from_str::<crate::policy::Policy>("[retention]\n\"ci.*\" = \"1s\"\n")
            .unwrap()
            .retention;
        let later = SystemTime::now() + Duration::from_hours(1);
        let dropped = Annotations::open(&store)
            .unwrap()
            .drop_expired_payloads(&retention, later)
            .unwrap();
        assert_eq!(dropped.payloads, 1, "its pack is rewritten without it");
        let _held = writer.hold_objects().unwrap();
        assert_eq!(
            writer.write_all(std::slice::from_ref(&payload)).unwrap(),
            1,
            "so the writer writes it again"
        );
        assert!(store.contains(payload.id()).unwrap());
    }
}
