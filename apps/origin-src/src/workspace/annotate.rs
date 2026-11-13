//! The statement commands: `annotate`, `statements`, `promote` and `gc`,
//! and flagging divergence when a revision is saved.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use origin_shared::api::Annotation;
use origin_shared::pack::Pack;
use origin_shared::{
    AttachmentId, EncodedObject, Inputs, Kind, Principal, Record, RecordId, Revision, RevisionId,
    Statement, StatementId, Verdict,
};

use super::{WorkingCopy, Workspace, WorkspaceError};
use crate::annotations::Annotations;
use crate::index::{FindError, Index};
use crate::policy::Policy;
use crate::service::{Caller, OriginService};
use crate::store::StoreError;
use crate::tree::Snapshots;

impl WorkingCopy {
    /// The revision `rev` names: `@` for the working copy's own, or by a
    /// unique prefix of its id, a change for its head, or a revision. A
    /// prefix naming a change and another revision than its head is
    /// ambiguous.
    fn find_revision(&self, rev: &str) -> Result<RevisionId, WorkspaceError> {
        if rev == "@" {
            return Ok(self.id);
        }
        let index = &self.index;
        match (index.find_change(rev), index.find_revision(rev)) {
            (Err(FindError::NotFound(_)), revision) => Ok(revision?),
            (Ok(change), Err(FindError::NotFound(_))) => Workspace::single_head(index, change),
            (Ok(change), Ok(revision)) if index.heads(change) == [revision] => Ok(revision),
            (Ok(change), Ok(revision)) => Err(WorkspaceError::AmbiguousRevision {
                prefix: rev.to_owned(),
                change,
                revision,
            }),
            (Err(error), _) => Err(error.into()),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }

    /// The record `annotated` names, as [`WorkingCopy::find_revision`]
    /// finds a revision, or a statement by a unique prefix of its id.
    fn find_record(&self, annotated: Annotated<'_>) -> Result<RecordId, WorkspaceError> {
        Ok(match annotated {
            Annotated::Revision(rev) => RecordId::Revision(self.find_revision(rev)?),
            Annotated::Statement(prefix) => RecordId::Statement(self.index.find_statement(prefix)?),
        })
    }
}

/// What a statement is about, as the command line names it.
#[derive(Clone, Copy, Debug)]
pub enum Annotated<'a> {
    /// A revision: `@`, a change for its head, or a revision.
    Revision(&'a str),
    /// A statement, by a unique prefix of its id.
    Statement(&'a str),
}

/// Where a statement's details come from.
#[derive(Debug)]
pub enum Payload {
    /// Text given inline, such as a review's message.
    Text(String),
    /// A file's content, such as a log: standard input's for `-`.
    File(PathBuf),
}

impl Payload {
    /// The attachment holding the details.
    fn attachment(&self) -> Result<EncodedObject, WorkspaceError> {
        match self {
            Self::Text(text) => Ok(EncodedObject::attachment(text.as_bytes())),
            Self::File(path) if path == Path::new("-") => {
                EncodedObject::read_attachment(io::stdin().lock(), 0)
                    .map_err(|source| WorkspaceError::io(path, source))
            }
            Self::File(path) => File::open(path)
                .and_then(|file| {
                    let len = file.metadata()?.len();
                    EncodedObject::read_attachment(file, usize::try_from(len).unwrap_or(0))
                })
                .map_err(|source| WorkspaceError::io(path, source)),
        }
    }
}

impl Workspace {
    /// This node, which issues the statements core makes.
    pub(super) fn node_principal(&self) -> Principal {
        Principal::Node(self.config.node().clone())
    }

    /// Issues `core.divergent` if `revision`, just saved, made its change
    /// diverge, as `index`, which has it, tells. The statement is the same
    /// for the same heads, so checking again writes nothing.
    pub(super) fn flag_divergence(
        &self,
        index: &Index,
        revision: RevisionId,
    ) -> Result<(), WorkspaceError> {
        let revision: Revision = self.store.get(revision)?;
        // A new change has a single revision.
        if revision.predecessors.is_empty() || !index.is_divergent(revision.change) {
            return Ok(());
        }
        Annotations::new(&self.store, index.clone())
            .flag_divergence(revision.change, self.node_principal())?;
        Ok(())
    }

    /// `origin annotate <rev> <kind> --pass|--fail|--neutral|--withdraw`:
    /// issues a statement about a revision as the user, superseding the
    /// user's latest one of the same kind about it, and resumes the
    /// landings waiting for evidence about it. Without a `verdict`, it
    /// withdraws that one: see [`Statement::withdraws`].
    ///
    /// # Errors
    ///
    /// Fails if the kind is reserved, the revision cannot be found, there
    /// is nothing to withdraw, the payload cannot be read, or the records
    /// cannot be written.
    pub fn annotate(
        &mut self,
        annotated: Annotated<'_>,
        kind: Kind,
        verdict: Option<Verdict>,
        inputs: Option<Inputs>,
        payload: Option<&Payload>,
    ) -> Result<(), WorkspaceError> {
        if kind.is_reserved() {
            return Err(WorkspaceError::ReservedKind(kind));
        }
        let working_copy = self.refresh()?;
        let subject = working_copy.find_record(annotated)?;
        let annotations = Annotations::new(&self.store, working_copy.index);
        let issuer = self.config.user();
        let supersedes = annotations.current(subject, &kind, &issuer)?;
        // What a withdrawal takes back: a statement with a verdict.
        let withdrawn = match (verdict, supersedes) {
            (Some(_), _) => None,
            (None, Some(id)) if !self.store.get(id)?.withdraws() => Some(id),
            (None, _) => return Err(self.nothing_to_withdraw(&annotations, kind, subject)?),
        };
        let payload = payload.map(Payload::attachment).transpose()?;
        let statement = Statement {
            subject,
            supersedes,
            kind,
            verdict,
            issuer,
            inputs,
            payload: payload.as_ref().map(|payload| payload.id().into()),
            promoted_from: None,
        };
        // Through the service, which resumes the landings waiting for it.
        let encoded = statement.encode();
        let id: StatementId = encoded.id().into();
        let objects = payload.into_iter().chain([encoded]).collect();
        self.service(Some(annotations.into_index()))?
            .put_records(&Caller::Owner, Pack { objects })?;
        if let Some(verdict) = verdict {
            println!(
                "Issued statement {}: {} {verdict} about {}",
                id.short(),
                statement.kind,
                subject.describe()
            );
            if let Some(superseded) = statement.supersedes {
                println!("Supersedes statement {}", superseded.short());
            }
        }
        if let Some(withdrawn) = withdrawn {
            println!(
                "Withdrew statement {} with statement {}: {} about {}",
                withdrawn.short(),
                id.short(),
                statement.kind,
                subject.describe()
            );
        }
        if let (Some(inputs), Some(mut trees)) =
            (&statement.inputs, self.store.input_trees(subject)?)
            && let Some(digest) = Snapshots::new(&self.store).input_digest(&mut trees, inputs)?
        {
            println!("Input digest: {}", digest.short());
        }
        Ok(())
    }

    /// Why the user has no `kind` statement about `subject` to withdraw:
    /// none holds there, or about a revision, only a promotion of one of
    /// theirs about another revision, which is where they withdraw it.
    ///
    /// # Errors
    ///
    /// Fails if the claims about the revision cannot be read.
    fn nothing_to_withdraw(
        &self,
        annotations: &Annotations,
        kind: Kind,
        subject: RecordId,
    ) -> Result<WorkspaceError, WorkspaceError> {
        let RecordId::Revision(revision) = subject else {
            return Ok(WorkspaceError::NothingToWithdraw { kind, subject });
        };
        let user = self.config.user();
        let claims = annotations.claims(RecordId::Revision(revision))?;
        let promoted = claims.iter().find_map(|claim| {
            let claimed = claim.claimed();
            match (claim.original(), claimed.subject) {
                (Some(original), RecordId::Revision(from))
                    if claimed.kind == kind && claimed.issuer == user =>
                {
                    Some((original, from))
                }
                _ => None,
            }
        });
        Ok(match promoted {
            Some((original, from)) => WorkspaceError::WithdrawPromoted {
                kind,
                revision,
                original,
                from,
            },
            None => WorkspaceError::NothingToWithdraw { kind, subject },
        })
    }

    /// `origin statements [<rev>]`: prints the statements about a revision,
    /// the working copy by default, in the order they arrived, each with the
    /// statements about it indented under it.
    ///
    /// # Errors
    ///
    /// Fails if the revision cannot be found or a record cannot be read.
    pub fn statements(&mut self, rev: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.snapshot()?;
        let revision = working_copy.find_revision(rev)?;
        let service = self.service(Some(working_copy.index))?;
        let subject = RecordId::Revision(revision);
        if !self.print_statements(&service, subject, "", &mut HashSet::new())? {
            println!("No statements about revision {}", revision.short());
        }
        Ok(())
    }

    /// Prints the statements about `subject` the user sees, as `service`
    /// lists them, each one's lines after `indent`, then the statements
    /// about it, indented further. `shown` keeps a statement from printing
    /// twice, so a malformed store's cycle ends. Returns whether it printed
    /// any.
    fn print_statements(
        &self,
        service: &OriginService,
        subject: RecordId,
        indent: &str,
        shown: &mut HashSet<StatementId>,
    ) -> Result<bool, WorkspaceError> {
        let mut printed = false;
        for annotation in service.statements(&Caller::Owner, subject)? {
            if !shown.insert(annotation.id) {
                continue;
            }
            printed = true;
            let statement = &annotation.statement;
            let verdict = match statement.verdict {
                Some(verdict) => format!(" {verdict}"),
                None if statement.withdraws() => " withdrawn".to_owned(),
                None => String::new(),
            };
            let superseded = annotation
                .superseded_by
                .map_or_else(String::new, |by| format!(" (superseded by {})", by.short()));
            println!(
                "{indent}{} {}{verdict} by {}{superseded}",
                annotation.id.short(),
                statement.kind,
                statement.issuer
            );
            if let Some(original) = statement.promoted_from {
                let issuer = match self.store.get(original) {
                    Ok(original) => original.issuer.to_string(),
                    Err(StoreError::NotFound(_)) => "an issuer not in this repository".to_owned(),
                    Err(error) => return Err(error.into()),
                };
                println!(
                    "{indent}    promoted from statement {} by {issuer}",
                    original.short()
                );
            }
            if let Some(inputs) = &statement.inputs {
                let on_target = inputs
                    .target_paths
                    .iter()
                    .map(|path| format!("{path} on the target"));
                let paths: Vec<_> = inputs
                    .paths
                    .iter()
                    .map(ToString::to_string)
                    .chain(on_target)
                    .collect();
                let external = if inputs.external {
                    ", and external"
                } else {
                    ""
                };
                println!("{indent}    inputs: {}{external}", paths.join(", "));
            }
            if let Some(payload) = statement.payload {
                println!("{indent}    payload: {}", self.payload_summary(payload)?);
            }
            let deeper = format!("{indent}    ");
            let about = RecordId::Statement(annotation.id);
            self.print_statements(service, about, &deeper, shown)?;
        }
        Ok(printed)
    }

    /// A payload's first line if it's text, its size otherwise, or that this
    /// repository doesn't have it: retention may have dropped it, or it
    /// never came here.
    fn payload_summary(&self, payload: AttachmentId) -> Result<String, WorkspaceError> {
        let Some(content) = self.store.try_attachment(payload)? else {
            return Ok("unavailable here".to_owned());
        };
        Ok(
            match str::from_utf8(&content).map(|text| text.lines().next()) {
                Ok(Some(line)) => line.to_owned(),
                _ => format!("{} bytes", content.len()),
            },
        )
    }

    /// `origin promote [<rev>]`: carries promotable results over to a
    /// revision, the working copy by default, from the records on the same
    /// target with the same input digest, as statements this node issues.
    /// Landings waiting for them continue.
    ///
    /// # Errors
    ///
    /// Fails if the revision cannot be found, or a record cannot be read or
    /// written.
    pub fn promote(&mut self, rev: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let revision = working_copy.find_revision(rev)?;
        let promoted = self
            .service(Some(working_copy.index))?
            .promote(&Caller::Owner, RecordId::Revision(revision))?;
        if promoted.is_empty() {
            println!("Nothing to promote to revision {}", revision.short());
        }
        for Annotation { id, statement, .. } in promoted {
            let Some(original) = statement.promoted_from else {
                continue;
            };
            let verdict = statement
                .verdict
                .map_or_else(String::new, |verdict| format!(" {verdict}"));
            println!(
                "Promoted {}{verdict} by {} as statement {}, from statement {}",
                statement.kind,
                self.store.get(original)?.issuer,
                id.short(),
                original.short()
            );
        }
        Ok(())
    }

    /// `origin gc`: drops the payloads past the retention `policy.toml`
    /// sets, packs the small loose objects, and removes the temporary files
    /// crashed writes left behind. Statements are never dropped.
    ///
    /// # Errors
    ///
    /// Fails if the policy is invalid, or the store cannot be read,
    /// compacted or cleaned.
    pub fn gc(&self) -> Result<(), WorkspaceError> {
        // The policy in force once collection may start, not before.
        let retention = || Ok::<_, WorkspaceError>(Policy::load(&self.store)?.retention);
        let dropped =
            Annotations::open(&self.store)?.drop_expired_payloads(retention, SystemTime::now())?;
        println!(
            "Dropped {} payloads past their retention, freeing {} bytes",
            dropped.payloads, dropped.bytes
        );
        match self.store.gc()? {
            Some(compaction) => println!(
                "Packed {} loose objects, merging {} packs",
                compaction.packed, compaction.merged
            ),
            None => println!("Another process is compacting the store"),
        }
        Ok(())
    }
}
