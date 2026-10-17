//! Promotion: carrying results over to the records with the same input
//! digest.
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

use origin_platform::Platform;
use origin_shared::api::Annotation;
use origin_shared::{Principal, Record, RecordId, Statement, StatementId, TreeId};

use super::Annotations;
use crate::store::{Store, StoreError};
use crate::tree::Snapshots;

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

impl<P: Platform> Annotations<'_, P> {
    /// The claims about `subject` that hold, in arrival order: those of
    /// the statements nothing superseded, that [`Store::claim`] accepts with
    /// the records this node has, and for a promotion, whose original
    /// nothing superseded either, and that no statement about `subject`
    /// of the kind from the original's issuer replaces, whatever came
    /// first: a result that ran there wins, and so does one taken back
    /// there (see [`Statement::drop_replaced_promotions`]).
    ///
    /// # Errors
    ///
    /// Fails if a record or a tree cannot be read.
    pub fn claims(&self, subject: RecordId) -> Result<Vec<Claim>, StoreError> {
        self.claims_of(&self.listed(subject)?)
    }

    /// The claims that hold of `listed`, the statements about one record,
    /// as [`Annotations::claims`] finds them.
    ///
    /// # Errors
    ///
    /// Fails if a record or a tree cannot be read.
    pub(super) fn claims_of(&self, listed: &[Annotation]) -> Result<Vec<Claim>, StoreError> {
        let mut claims = Vec::new();
        for annotation in listed {
            if annotation.superseded_by.is_some() {
                continue;
            }
            let claim = match self
                .store
                .claim(annotation.id, annotation.statement.clone())
            {
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
        Statement::drop_replaced_promotions(&mut claims, |claim| {
            (&claim.statement, claim.claimed())
        });
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
    /// `issuer`, returning them. A promotion refers to its original's
    /// payload without writing it, so it [holds the objects](Store::hold_objects)
    /// from finding the originals to writing the promotions.
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
        let _held = self.store.hold_objects()?;
        let promotions = self.promotions(subject, issuer)?;
        let encoded: Vec<_> = promotions.iter().map(Statement::encode).collect();
        self.store.write_all(&encoded)?;
        Ok(encoded
            .iter()
            .map(|encoded| encoded.id().into())
            .zip(promotions)
            .collect())
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

#[cfg(test)]
mod tests {
    use origin_shared::{EncodedObject, Kind, Verdict};

    use super::*;
    use crate::annotations::AnnotateError;
    use crate::annotations::tests::{FILES, check, issue, principal, promote, revision, store};
    use crate::index::Index;

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
    fn results_there_replace_promoted_ones_whatever_came_first() {
        let (_dir, store) = store();
        let inputs = Some((&["src"][..], false));
        let first = revision(&store, 1, &[], FILES);
        issue(
            &store,
            &check(first, "lint", Verdict::Pass, "node:ci", inputs),
        );
        let verdicts = |revision| -> Vec<_> {
            let annotations = Annotations::open(&store).unwrap();
            let claims = annotations.claims(RecordId::Revision(revision)).unwrap();
            claims
                .iter()
                .map(|claim| (claim.original().is_some(), claim.claimed().verdict))
                .collect()
        };

        // A failure there, after a promoted pass.
        let second = revision(&store, 2, &[], FILES);
        assert_eq!(promote(&store, second).len(), 1);
        issue(
            &store,
            &check(second, "lint", Verdict::Fail, "node:ci", inputs),
        );
        assert_eq!(verdicts(second), [(false, Some(Verdict::Fail))]);

        // A pass there, then taken back, after a promoted pass.
        let third = revision(&store, 3, &[], FILES);
        assert_eq!(promote(&store, third).len(), 1);
        let pass = check(third, "lint", Verdict::Pass, "node:ci", inputs);
        let id = issue(&store, &pass);
        let withdrawal = Statement {
            verdict: None,
            inputs: None,
            supersedes: Some(id),
            ..pass
        };
        issue(&store, &withdrawal);
        assert_eq!(verdicts(third), [(false, None)]);
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
        let about = annotations.about(RecordId::Revision(second)).unwrap();
        assert!(
            about.iter().all(|annotation| !annotation.holds),
            "{about:?}"
        );
        assert_eq!(promote(&store, second), [("lint".to_owned(), Some(rerun))]);
    }
}
