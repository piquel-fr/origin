//! The op log commands: `op log`, `undo` and `watch`.
//!
//! `undo` takes back the latest operation of the workspace it runs in that
//! moved its working copy, other than a snapshot: it moves the working copy
//! back to the revision it was before, and checks it out. That is all it
//! does. Records stay, since others may have them already: an undone
//! `abandon` still abandoned the change, and an undone `land` still landed.
//! Operations from other workspaces are never undone, and the other
//! workspaces' working copies never move: each undoes its own. Undoing
//! again takes back the operation before, and so on; undos themselves are
//! never undone.
//!
//! Snapshots aren't undone on their own: the edits they recorded stay
//! revisions of their change. If the revision moved back to is no longer a
//! head of its change, because the operation made a newer revision of it,
//! as `describe` does, the working copy becomes a new revision with its
//! contents, evolving from the change's heads, so the change doesn't
//! diverge.

use std::collections::{BTreeMap, HashSet};

use origin_shared::api::{EventsQuery, List};
use origin_shared::landing::Proposal;
use origin_shared::policy::KindPattern;
use origin_shared::{Period, Record, RecordId, Revision, RevisionId};

use super::{Workspace, WorkspaceError};
use crate::index;
use crate::oplog::{Effect, Entry, Move, Why};
use crate::service::{Caller, OriginService};

impl Workspace {
    /// `origin op log [-n <limit>]`: prints the latest operations of every
    /// workspace, newest first, each with what its entries record, marking
    /// this workspace's.
    ///
    /// # Errors
    ///
    /// Fails if the op log or a record cannot be read.
    pub fn op_log(&self, limit: Option<usize>) -> Result<(), WorkspaceError> {
        let limit = limit.unwrap_or(usize::MAX);
        let mut ops: BTreeMap<u64, Vec<Entry>> = BTreeMap::new();
        for entry in self.store.oplog().newest_first()? {
            let entry = entry?;
            let full = ops.len() >= limit;
            // An operation's entries all come after its first.
            if full && ops.keys().next().is_some_and(|&oldest| entry.seq < oldest) {
                break;
            }
            if full && !ops.contains_key(&entry.op) {
                continue;
            }
            ops.entry(entry.op).or_default().push(entry);
        }
        if ops.is_empty() {
            println!("The op log is empty.");
        }
        for (op, entries) in ops.iter().rev() {
            let first = &entries[entries.len() - 1];
            let (marker, workspace) = match &first.workspace {
                Some(root) if *root == self.root => ('@', root.display().to_string()),
                Some(root) => ('○', root.display().to_string()),
                None => ('○', "(no workspace)".to_owned()),
            };
            println!("{marker} #{op} {workspace}: origin {}", first.command);
            for entry in entries.iter().rev() {
                println!("    {}", self.describe_entry(entry)?);
            }
        }
        Ok(())
    }

    /// What an entry records, in a line.
    fn describe_entry(&self, entry: &Entry) -> Result<String, WorkspaceError> {
        Ok(match &entry.effect {
            Effect::Wrote(records) => {
                let mut described = Vec::new();
                for record in records.iter().map(index::Entry::record) {
                    let mut description = Self::describe_record(record);
                    if let RecordId::Acceptance(id) = record
                        && let Ok(landing) = self.describe_acceptance(id)
                    {
                        description = format!("{description}, {landing}");
                    }
                    described.push(description);
                }
                format!("wrote {}", described.join(", "))
            }
            Effect::Waiting { onto, proposal } => match proposal {
                Proposal::Change(change) => {
                    format!("change {} waits to land on `{onto}`", change.short())
                }
                Proposal::Target(target) => format!("`{target}` waits to land on `{onto}`"),
            },
            Effect::Moved(Move { from, to, why }) => {
                let to = self.describe_revision(*to)?;
                let from = match from {
                    Some(from) => self.describe_revision(*from)?,
                    None => return Ok(format!("created the working copy at {to}")),
                };
                match why {
                    Why::Snapshot => format!("snapshotted the working copy: {from} -> {to}"),
                    Why::Command => format!("moved the working copy: {from} -> {to}"),
                    Why::Undo(op) => format!("undid #{op}: {from} -> {to}"),
                }
            }
            Effect::Advanced {
                target,
                seq,
                acceptance,
            } => format!(
                "advanced `{target}` to #{seq}, acceptance {}",
                acceptance.short()
            ),
        })
    }

    /// A revision and its change, such as `revision 1a2b of change 3c4d`.
    fn describe_revision(&self, id: RevisionId) -> Result<String, WorkspaceError> {
        let revision = self.store.get(id)?;
        Ok(format!(
            "revision {} of change {}",
            id.short(),
            revision.change.short()
        ))
    }

    /// `origin undo`: moves the working copy back to where it was before
    /// this workspace's latest operation, as the module explains.
    ///
    /// # Errors
    ///
    /// Fails if there is nothing to undo, the latest operation created the
    /// workspace, or the working copy cannot be snapshotted or moved.
    pub fn undo(&mut self) -> Result<(), WorkspaceError> {
        let mut working_copy = self.refresh()?;
        let (op, command, from) = self.last_undoable(working_copy.id)?;
        let revision: Revision = self.store.get(from)?;
        let heads = working_copy.index.heads(revision.change);
        let (to, restored, layers) = if heads.contains(&from) {
            (from, revision, Vec::new())
        } else {
            // A newer revision of the change exists: restore this one's
            // contents, and its unresolved conflicts, in a revision after it.
            let conflicts = self.conflicts(&mut working_copy.index, from, &revision)?;
            let restored = Revision {
                predecessors: heads,
                author: self.config.user(),
                ..revision
            };
            let encoded = restored.encode();
            let layers = self.revision_layers(Vec::new(), &encoded, &conflicts);
            (encoded.id().into(), restored, layers)
        };
        self.switch_to(&working_copy, to, &restored, &layers, Why::Undo(op))?;
        println!("Undid #{op}: origin {command}");
        println!("Working copy: {}", self.headline(&restored)?);
        println!("On top of: {}", self.describe_base(restored.base)?);
        Ok(())
    }

    /// The latest operation of this workspace to undo, its command, and the
    /// revision the working copy was before it. An operation that left the
    /// working copy where it was, `current`, moved nothing, so it's skipped.
    fn last_undoable(
        &self,
        current: RevisionId,
    ) -> Result<(u64, String, RevisionId), WorkspaceError> {
        // This workspace's moves, newest first.
        let mut moves = Vec::new();
        for entry in self.store.oplog().newest_first()? {
            let entry = entry?;
            if let Effect::Moved(moved) = entry.effect
                && entry.workspace.as_ref() == Some(&self.root)
            {
                moves.push((entry.op, entry.command, moved));
            }
        }
        let mut skipped = HashSet::new();
        for (op, _, moved) in &moves {
            if let Why::Undo(undone) = moved.why {
                skipped.extend([*op, undone]);
            }
        }
        let mut seen = HashSet::new();
        for (op, command, _) in &moves {
            if skipped.contains(op) || !seen.insert(*op) {
                continue;
            }
            // Where the operation's first move, other than a snapshot,
            // started.
            let from = moves
                .iter()
                .rfind(|(other, _, moved)| other == op && moved.why == Why::Command)
                .map(|(_, _, moved)| moved.from);
            match from {
                // Only snapshots, or it left the working copy where it is.
                None => {}
                Some(Some(from)) if from == current => {}
                // Logged before a crash kept its revision from being written.
                Some(Some(from)) if !self.store.contains(from.into())? => {}
                Some(None) => return Err(WorkspaceError::UndoCreation(self.root.clone())),
                Some(Some(from)) => return Ok((*op, command.clone(), from)),
            }
        }
        Err(WorkspaceError::NothingToUndo)
    }

    /// `origin watch [--kind <kind>]... [--subject <subject>]... [--since
    /// <cursor>] [--local]`: prints the events of those kinds about those
    /// subjects, every event without them, as they happen, one a line, from
    /// `since`, or from now. They are the remote's, long-polled, if the
    /// repository has one, or with `local`, this node's. Runs until
    /// interrupted.
    ///
    /// # Errors
    ///
    /// Fails if the cursor expired, the remote fails, or the op log or a
    /// record cannot be read.
    pub fn watch(
        &self,
        kinds: Vec<KindPattern>,
        subjects: Vec<String>,
        since: Option<u64>,
        local: bool,
    ) -> Result<(), WorkspaceError> {
        let remote = if local { None } else { self.remote()? };
        let service = self.service(None)?;
        let mut query = EventsQuery {
            cursor: since,
            generation: None,
            wait: Some(Period(OriginService::MAX_WAIT)),
            limit: Some(256),
            kind: List(kinds),
            subject: List(subjects),
        };
        loop {
            let page = match remote {
                Some(remote) => remote.events(&query)?,
                None => service.events(&Caller::Owner, &query)?,
            };
            for event in &page.events {
                println!("{event}");
            }
            query.cursor = Some(page.next);
            query.generation = Some(page.generation);
        }
    }
}
