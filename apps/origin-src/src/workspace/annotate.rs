//! The statement commands: `annotate`, `statements`, `promote` and `gc`,
//! and flagging divergence when a revision is saved.

use std::fs::File;
use std::path::Path;
use std::time::SystemTime;

use origin_shared::{
    BlobId, EncodedObject, Inputs, Kind, Principal, RecordId, Revision, RevisionId, Statement,
    Verdict,
};

use super::{WorkingCopy, Workspace, WorkspaceError};
use crate::annotations::Annotations;
use crate::index::FindError;
use crate::policy::Policy;
use crate::tree::Snapshots;

impl WorkingCopy {
    /// The revision `rev` names: `@` for the working copy, a change for its
    /// head, or else a revision, each by a unique prefix of its id.
    fn find_revision(&self, rev: &str) -> Result<RevisionId, WorkspaceError> {
        match self.find_change(rev) {
            Ok(change) => Workspace::single_head(&self.index, change),
            Err(FindError::NotFound(_)) => Ok(self.index.find_revision(rev)?),
            Err(error) => Err(error.into()),
        }
    }
}

impl Workspace {
    /// This node, which issues the statements core makes.
    pub(super) fn node_principal(&self) -> Principal {
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

    /// `origin annotate <rev> <kind> --pass|--fail|--neutral`: issues a
    /// statement about a revision as the user, superseding the user's
    /// latest one of the same kind about it.
    ///
    /// # Errors
    ///
    /// Fails if the kind is reserved, the revision cannot be found, the
    /// payload cannot be read, or the records cannot be written.
    pub fn annotate(
        &mut self,
        rev: &str,
        kind: Kind,
        verdict: Verdict,
        inputs: Option<Inputs>,
        payload: Option<&Path>,
    ) -> Result<(), WorkspaceError> {
        if kind.is_reserved() {
            return Err(WorkspaceError::ReservedKind(kind));
        }
        let working_copy = self.refresh()?;
        let revision = working_copy.find_revision(rev)?;
        let annotations = Annotations::new(&self.store, working_copy.index);
        let payload = payload
            .map(|path| {
                File::open(path)
                    .and_then(|file| {
                        let len = file.metadata()?.len();
                        EncodedObject::read_blob(file, usize::try_from(len).unwrap_or(0))
                    })
                    .map_err(|source| WorkspaceError::io(path, source))
            })
            .transpose()?;
        let subject = RecordId::Revision(revision);
        let issuer = self.config.user();
        let statement = Statement {
            subject,
            supersedes: annotations.current(subject, &kind, &issuer)?,
            kind,
            verdict: Some(verdict),
            issuer,
            inputs,
            payload: payload.as_ref().map(|payload| payload.id().into()),
            promoted_from: None,
        };
        let id = annotations.issue(&statement, payload)?;
        println!(
            "Issued statement {}: {} {} about revision {}",
            id.short(),
            statement.kind,
            verdict,
            revision.short()
        );
        if let Some(superseded) = statement.supersedes {
            println!("Supersedes statement {}", superseded.short());
        }
        if let Some(inputs) = &statement.inputs {
            let snapshot = self.store.get(revision)?.snapshot;
            let digest = Snapshots::new(&self.store).input_digest(snapshot, inputs)?;
            println!("Input digest: {}", digest.short());
        }
        Ok(())
    }

    /// `origin statements [<rev>]`: prints the statements about a revision,
    /// the working copy by default, in the order they arrived.
    ///
    /// # Errors
    ///
    /// Fails if the revision cannot be found or a record cannot be read.
    pub fn statements(&mut self, rev: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let revision = working_copy.find_revision(rev)?;
        let annotations = Annotations::new(&self.store, working_copy.index);
        let found = annotations.about(RecordId::Revision(revision))?;
        if found.is_empty() {
            println!("No statements about revision {}", revision.short());
        }
        for annotation in found {
            let statement = &annotation.statement;
            let verdict = statement
                .verdict
                .map_or_else(String::new, |verdict| format!(" {verdict}"));
            let superseded = annotation
                .superseded_by
                .map_or_else(String::new, |by| format!(" (superseded by {})", by.short()));
            println!(
                "{} {}{verdict} by {}{superseded}",
                annotation.id.short(),
                statement.kind,
                statement.issuer
            );
            if let Some(original) = statement.promoted_from {
                let issuer = self.store.get(original)?.issuer;
                println!(
                    "    promoted from statement {} by {issuer}",
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
                println!("    inputs: {}{external}", paths.join(", "));
            }
            if let Some(payload) = statement.payload {
                println!("    payload: {}", self.payload_summary(payload)?);
            }
        }
        Ok(())
    }

    /// A payload's first line if it's text, its size otherwise, or that it
    /// was dropped.
    fn payload_summary(&self, payload: BlobId) -> Result<String, WorkspaceError> {
        let Some(content) = self.store.try_blob(payload.into())? else {
            return Ok("dropped, past its retention".to_owned());
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
        let revision = working_copy.find_revision(rev)?;
        let mut annotations = Annotations::new(&self.store, working_copy.index);
        let promoted = annotations.promote(RecordId::Revision(revision), &self.node_principal())?;
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

    /// `origin gc`: drops the payloads past the retention `policy.toml`
    /// sets, packs the small loose objects, and removes the temporary files
    /// crashed writes left behind. Statements are never dropped.
    ///
    /// # Errors
    ///
    /// Fails if the policy is invalid, or the store cannot be read,
    /// compacted or cleaned.
    pub fn gc(&self) -> Result<(), WorkspaceError> {
        let policy = Policy::load(&self.store)?;
        let dropped = Annotations::open(&self.store)?
            .drop_expired_payloads(&policy.retention, SystemTime::now())?;
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
