//! The statement commands: `annotate`, `statements` and `promote`,
//! and flagging divergence when a revision is saved.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use origin_shared::{
    BlobId, EncodedObject, Inputs, Kind, Principal, RecordId, Revision, RevisionId, Statement,
    StatementId, Verdict,
};

use super::{WorkingCopy, Workspace, WorkspaceError};
use crate::annotations::{AnnotateError, Annotations};
use crate::index::{FindError, Index};
use crate::store::StoreError;
use crate::tree::Snapshots;

impl WorkingCopy {
    /// The revision `rev` names: `@` for the working copy's own, or by a
    /// unique prefix of its id, a change for its head, or a revision. A
    /// prefix naming a change and another revision than its head is
    /// ambiguous.
    fn find_revision(&self, index: &Index, rev: &str) -> Result<RevisionId, WorkspaceError> {
        if rev == "@" {
            return Ok(self.id);
        }
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
    fn find_record(
        &self,
        index: &Index,
        annotated: Annotated<'_>,
    ) -> Result<RecordId, WorkspaceError> {
        Ok(match annotated {
            Annotated::Revision(rev) => RecordId::Revision(self.find_revision(index, rev)?),
            Annotated::Statement(prefix) => RecordId::Statement(index.find_statement(prefix)?),
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
    /// The blob holding the details.
    fn blob(&self) -> Result<EncodedObject, WorkspaceError> {
        match self {
            Self::Text(text) => Ok(EncodedObject::blob(text.as_bytes())),
            Self::File(path) if path == Path::new("-") => {
                EncodedObject::read_blob(io::stdin().lock(), 0)
                    .map_err(|source| WorkspaceError::io(path, source))
            }
            Self::File(path) => File::open(path)
                .and_then(|file| {
                    let len = file.metadata()?.len();
                    EncodedObject::read_blob(file, usize::try_from(len).unwrap_or(0))
                })
                .map_err(|source| WorkspaceError::io(path, source)),
        }
    }
}

impl Workspace {
    /// This node, which issues the statements core makes.
    fn node_principal(&self) -> Principal {
        Principal::Node(self.config.node().clone())
    }

    /// Issues `core.divergent` if `revision`, just saved, made its change
    /// diverge. The statement is the same for the same heads, so checking
    /// again writes nothing.
    pub(super) fn flag_divergence(&self, revision: RevisionId) -> Result<(), WorkspaceError> {
        let revision: Revision = self.store.get(revision)?;
        // A new change has a single revision.
        if revision.predecessors.is_empty() {
            return Ok(());
        }
        Annotations::open(&self.store)?.flag_divergence(revision.change, self.node_principal())?;
        Ok(())
    }

    /// `origin annotate <rev> <kind> --pass|--fail|--neutral|--withdraw`:
    /// issues a statement about a revision as the user, superseding the
    /// user's latest one of the same kind about it, as
    /// [`Annotations::supersede`] does. Without a `verdict`, it withdraws
    /// that one: see [`Statement::withdraws`].
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
        let mut annotations = Annotations::open(&self.store)?;
        let subject = working_copy.find_record(annotations.index(), annotated)?;
        let payload = payload.map(Payload::blob).transpose()?;
        let statement = Statement {
            subject,
            supersedes: None,
            kind,
            verdict,
            issuer: self.config.user(),
            inputs,
            payload: payload.as_ref().map(|payload| payload.id().into()),
            promoted_from: None,
        };
        let (id, statement) = match annotations.supersede(statement, payload) {
            Err(AnnotateError::NothingToWithdraw { kind, subject }) => {
                return Err(self.nothing_to_withdraw(&annotations, kind, subject)?);
            }
            issued => issued?,
        };
        match (statement.verdict, statement.supersedes) {
            (Some(verdict), superseded) => {
                println!(
                    "Issued statement {}: {} {verdict} about {}",
                    id.short(),
                    statement.kind,
                    subject.describe()
                );
                if let Some(superseded) = superseded {
                    println!("Supersedes statement {}", superseded.short());
                }
            }
            (None, Some(withdrawn)) => println!(
                "Withdrew statement {} with statement {}: {} about {}",
                withdrawn.short(),
                id.short(),
                statement.kind,
                subject.describe()
            ),
            (None, None) => {}
        }
        if let (Some(inputs), RecordId::Revision(revision)) = (&statement.inputs, subject) {
            let snapshot = self.store.get(revision)?.snapshot;
            let digest = Snapshots::new(&self.store).input_digest(snapshot, inputs)?;
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
            return Ok(AnnotateError::NothingToWithdraw { kind, subject }.into());
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
            None => AnnotateError::NothingToWithdraw { kind, subject }.into(),
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
        let annotations = Annotations::open(&self.store)?;
        let revision = working_copy.find_revision(annotations.index(), rev)?;
        let subject = RecordId::Revision(revision);
        if annotations.index().statements(subject).is_empty() {
            println!("No statements about revision {}", revision.short());
        }
        self.print_statements(&annotations, subject, "", &mut HashSet::new())
    }

    /// Prints the statements about `subject`, each one's lines after
    /// `indent`, then the statements about it, indented further. `shown`
    /// keeps a statement from printing twice, so a malformed store's cycle
    /// ends.
    fn print_statements(
        &self,
        annotations: &Annotations,
        subject: RecordId,
        indent: &str,
        shown: &mut HashSet<StatementId>,
    ) -> Result<(), WorkspaceError> {
        for annotation in annotations.about(subject)? {
            if !shown.insert(annotation.id) {
                continue;
            }
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
                let paths: Vec<_> = inputs.paths.iter().map(ToString::to_string).collect();
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
            self.print_statements(annotations, about, &deeper, shown)?;
        }
        Ok(())
    }

    /// A payload's first line if it's text, its size otherwise, or that
    /// this repository doesn't have it.
    fn payload_summary(&self, payload: BlobId) -> Result<String, WorkspaceError> {
        let Some(content) = self.store.try_blob(payload.into())? else {
            return Ok("not in this repository".to_owned());
        };
        Ok(
            match str::from_utf8(&content).map(|text| text.lines().next()) {
                Ok(Some(line)) => line.to_owned(),
                _ => format!("{} bytes", content.len()),
            },
        )
    }

    /// `origin promote [<rev>]`: carries promotable results over to a
    /// revision, the working copy by default, from the records with the
    /// same input digest, as statements this node issues.
    ///
    /// # Errors
    ///
    /// Fails if the revision cannot be found, or a record cannot be read or
    /// written.
    pub fn promote(&mut self, rev: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let mut annotations = Annotations::open(&self.store)?;
        let revision = working_copy.find_revision(annotations.index(), rev)?;
        let promoted = annotations.promote(revision, &self.node_principal())?;
        if promoted.is_empty() {
            println!("Nothing to promote to revision {}", revision.short());
        }
        for (id, statement) in promoted {
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
}
