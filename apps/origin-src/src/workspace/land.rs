//! The target commands: `land`, `target new` and `targets`, and starting a
//! change on a target with `new --on`.
//!
//! The queue runs in this process, as this node. After a landing, the
//! working copy of the workspace that ran it follows its change: when it
//! landed, the working copy starts a new change on the target's tip; when
//! the queue rebased it, the working copy becomes the rebased revision; and
//! when it sits on a change that landed, it is rebased onto the tip.
//!
//! Other workspaces are left alone, behind their change, until `origin
//! pull` follows it the same way, merging the edits made since. Where edits
//! conflict with what they're merged onto, the files hold both versions
//! between conflict markers, and a `core.conflict` statement lists them, so
//! the working copy doesn't land until they change or are accepted with
//! `origin resolve` (see [`conflict`](crate::conflict)).

use origin_shared::{
    Base, ChangeId, EncodedObject, Record, RecordId, Revision, RevisionId, Source, TargetName,
    TreeId,
};

use super::{Behind, WorkingCopy, Workspace, WorkspaceError};
use crate::conflict::Conflicts;
use crate::index::Index;
use crate::oplog::Why;
use crate::policy::Policy;
use crate::queue::{Blocked, Landing, Pending, Proposal, Queue};
use crate::tree::Merge;

impl Workspace {
    /// `origin land [<change>] [--onto <target>]`: lands a change, the
    /// working copy's by default, with the changes below it the target
    /// doesn't have yet, on the target its stack is on by default.
    ///
    /// # Errors
    ///
    /// Fails if the change cannot be found or cannot land, this node isn't
    /// the target's authority, or the records cannot be read or written.
    pub fn land(
        &mut self,
        change: Option<&str>,
        onto: Option<TargetName>,
    ) -> Result<(), WorkspaceError> {
        let mut working_copy = self.save_draft()?;
        let change = working_copy.find_change(change.unwrap_or("@"))?;
        let onto = match onto {
            Some(onto) => onto,
            None => self.target_of(Self::single_head(&working_copy.index, change)?)?,
        };
        let key = self.config.node_key()?;
        let landings =
            Queue::new(&self.store, &key, self.config.node().clone()).land_change(change, &onto)?;
        for landing in &landings {
            self.print_landing(landing, &onto)?;
        }
        working_copy.index = Index::open(&self.store)?;
        working_copy.behind =
            self.behind(&working_copy.index, working_copy.id, &working_copy.revision)?;
        self.follow(&working_copy)?;
        Ok(())
    }

    /// `origin land --from <target> --onto <target>`: lands the tip of
    /// another target.
    ///
    /// # Errors
    ///
    /// Fails if a target doesn't exist, this node isn't the authority of
    /// `onto`, or the records cannot be read or written.
    pub fn land_target(&self, from: &TargetName, onto: &TargetName) -> Result<(), WorkspaceError> {
        let key = self.config.node_key()?;
        let landing =
            Queue::new(&self.store, &key, self.config.node().clone()).land_target(from, onto)?;
        self.print_landing(&landing, onto)
    }

    /// `origin land --status`: lists the landings that wait, and why.
    ///
    /// # Errors
    ///
    /// Fails if the queues cannot be read.
    pub fn land_status(&self) -> Result<(), WorkspaceError> {
        let pending = Queue::pending(&self.store)?;
        if pending.is_empty() {
            println!("No landing waits.");
        }
        for pending in &pending {
            Self::print_pending(pending);
        }
        Ok(())
    }

    /// `origin target new <name> --from <target>`: creates a target at the
    /// tip of another.
    ///
    /// # Errors
    ///
    /// Fails if the name is taken, `from` doesn't exist, this node wouldn't
    /// be the target's authority, or the records cannot be written.
    pub fn target_new(&self, name: &TargetName, from: &TargetName) -> Result<(), WorkspaceError> {
        let key = self.config.node_key()?;
        Queue::new(&self.store, &key, self.config.node().clone()).create_target(name, from)?;
        let tip = self
            .store
            .head(from)?
            .ok_or_else(|| WorkspaceError::UnknownTarget(from.clone()))?;
        println!(
            "Created target `{name}` at {}",
            self.describe_acceptance(tip.acceptance)?
        );
        Ok(())
    }

    /// `origin targets`: lists every target with its tip, its authority and
    /// what landing on it requires.
    ///
    /// # Errors
    ///
    /// Fails if the targets or the policy cannot be read.
    pub fn targets(&self) -> Result<(), WorkspaceError> {
        let policy = Policy::load(&self.store)?;
        let mut targets = self.store.targets()?;
        targets.sort();
        for target in targets {
            let Some(tip) = self.store.head(&target)? else {
                continue;
            };
            let authority = policy
                .authority(&target)
                .map_or_else(|| "no authority".to_owned(), |node| format!("node:{node}"));
            println!(
                "{target} #{} {} ({authority})",
                tip.seq,
                tip.acceptance.short()
            );
            for requirement in policy.requirements(&target) {
                println!("    requires {requirement}");
            }
        }
        Ok(())
    }

    /// `origin new [description] --on <target>`: starts a change on the tip
    /// of a target.
    ///
    /// # Errors
    ///
    /// Fails if the target doesn't exist, the working copy cannot be
    /// snapshotted or is behind its change, or the change cannot be written
    /// or checked out.
    pub fn new_on(&mut self, target: &TargetName, description: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        self.start_on(&working_copy, target, description, working_copy.files)
    }

    /// `origin pull`: moves the working copy where its change went without
    /// it, merging the edits made since: onto the tip of the target it
    /// landed on, as a new change, or onto the landing queue's rebase of it.
    /// Otherwise rebases it onto the tip of the target its stack is on:
    /// once the change below it landed, or once the target it's on moved.
    ///
    /// # Errors
    ///
    /// Fails if the working copy cannot be snapshotted, a file was edited
    /// while it moved, or the records cannot be read or written.
    pub fn pull(&mut self) -> Result<(), WorkspaceError> {
        let working_copy = self.save_draft()?;
        if self.follow(&working_copy)? {
            return Ok(());
        }
        let Base::Acceptance(base) = working_copy.revision.base else {
            println!("The working copy is on a stack of changes, which stays where it is.");
            return Ok(());
        };
        let target = self.target_name(self.store.get(base)?.body().target)?;
        let tip = self
            .store
            .head(&target)?
            .ok_or_else(|| WorkspaceError::UnknownTarget(target.clone()))?;
        if tip.acceptance == base {
            println!("The working copy is on the tip of `{target}`.");
            return Ok(());
        }
        let files = self.base_tree(working_copy.revision.base)?;
        self.rebase_onto_tip(&working_copy, files, &target)
    }

    /// Starts a change described by `description` on the tip of `target`,
    /// with the edits that made the working copy's files from `base`, and
    /// checks it out over them, as the module explains.
    fn start_on(
        &mut self,
        working_copy: &WorkingCopy,
        target: &TargetName,
        description: &str,
        base: TreeId,
    ) -> Result<(), WorkspaceError> {
        let tip = self
            .store
            .head(target)?
            .ok_or_else(|| WorkspaceError::UnknownTarget(target.clone()))?;
        let state = self.store.get(tip.acceptance)?.body().state;
        let tip_label = format!("{target} #{}", tip.seq);
        let merge = self.store.merge_marked(
            Some(base),
            working_copy.files,
            state,
            [Self::OURS, &tip_label],
        )?;
        let description = EncodedObject::blob(description.as_bytes());
        let revision = Revision {
            change: ChangeId::random().map_err(WorkspaceError::Random)?,
            predecessors: Vec::new(),
            base: Base::Acceptance(tip.acceptance),
            deps: Vec::new(),
            snapshot: merge.tree,
            description: description.id().into(),
            author: self.config.user(),
        };
        let encoded = revision.encode();
        let (conflicts, layers) =
            self.write_merge(working_copy, merge, vec![description], &encoded)?;
        self.move_to(
            working_copy,
            encoded.id().into(),
            &revision,
            &layers,
            &conflicts,
        )
    }

    /// How conflict markers name the working copy's side.
    const OURS: &str = "working copy";

    /// Follows the working copy's change where it went without it, merging
    /// the edits made since, or rebases the working copy onto the tip once
    /// the change below it landed, as the module explains. Returns whether
    /// it moved.
    fn follow(&mut self, working_copy: &WorkingCopy) -> Result<bool, WorkspaceError> {
        match &working_copy.behind {
            Some(Behind::Landed { target, .. }) => {
                let base = working_copy.revision.snapshot;
                self.start_on(working_copy, &target.clone(), "", base)?;
                return Ok(true);
            }
            Some(Behind::Rebased(rebased)) => {
                self.move_onto(working_copy, *rebased)?;
                return Ok(true);
            }
            None => {}
        }
        let Base::Revision(below) = working_copy.revision.base else {
            return Ok(false);
        };
        let below: Revision = self.store.get(below)?;
        let [head] = working_copy.index.heads(below.change)[..] else {
            return Ok(false);
        };
        let Some(landing) = self.store.landing(head, &self.store.get(head)?)? else {
            return Ok(false);
        };
        let target = self.target_name(landing.body().target)?;
        self.rebase_onto_tip(working_copy, below.snapshot, &target)?;
        Ok(true)
    }

    /// The target a revision's stack is on: that of the acceptance at its
    /// bottom.
    fn target_of(&self, revision: RevisionId) -> Result<TargetName, WorkspaceError> {
        let mut base = self.store.get(revision)?.base;
        loop {
            match base {
                Base::Revision(id) => base = self.store.get(id)?.base,
                Base::Acceptance(id) => {
                    return self.target_name(self.store.get(id)?.body().target);
                }
            }
        }
    }

    /// Moves the working copy onto `rebased`, the landing queue's rebase of
    /// it, with the edits made since in a revision after it.
    fn move_onto(
        &mut self,
        working_copy: &WorkingCopy,
        rebased: RevisionId,
    ) -> Result<(), WorkspaceError> {
        let theirs: Revision = self.store.get(rebased)?;
        if working_copy.files == working_copy.revision.snapshot {
            return self.move_to(working_copy, rebased, &theirs, &[], &Conflicts::default());
        }
        let merge = self.store.merge_marked(
            Some(working_copy.revision.snapshot),
            working_copy.files,
            theirs.snapshot,
            [Self::OURS, &rebased.short()],
        )?;
        let revision = Revision {
            predecessors: vec![rebased],
            snapshot: merge.tree,
            author: self.config.user(),
            ..theirs
        };
        let encoded = revision.encode();
        let (conflicts, layers) = self.write_merge(working_copy, merge, Vec::new(), &encoded)?;
        self.move_to(
            working_copy,
            encoded.id().into(),
            &revision,
            &layers,
            &conflicts,
        )
    }

    /// Rebases the working copy, made on `base`, onto the tip of `onto`, as
    /// the module explains.
    fn rebase_onto_tip(
        &mut self,
        working_copy: &WorkingCopy,
        base: TreeId,
        onto: &TargetName,
    ) -> Result<(), WorkspaceError> {
        let tip = self
            .store
            .head(onto)?
            .ok_or_else(|| WorkspaceError::UnknownTarget(onto.clone()))?;
        let state = self.store.get(tip.acceptance)?.body().state;
        let current = &working_copy.revision;
        let tip_label = format!("{onto} #{}", tip.seq);
        let merge = self.store.merge_marked(
            Some(base),
            current.snapshot,
            state,
            [Self::OURS, &tip_label],
        )?;
        let revision = Revision {
            predecessors: vec![working_copy.id],
            base: Base::Acceptance(tip.acceptance),
            snapshot: merge.tree,
            author: self.config.user(),
            ..current.clone()
        };
        let encoded = revision.encode();
        let (conflicts, layers) = self.write_merge(working_copy, merge, Vec::new(), &encoded)?;
        self.move_to(
            working_copy,
            encoded.id().into(),
            &revision,
            &layers,
            &conflicts,
        )
    }

    /// Writes the objects `merge` of the working copy's files made, for
    /// `revision`, which it made. Returns the revision's unresolved
    /// conflicts, those the merge left and those of the working copy it
    /// still has, and the layers that write it, with `with`, and with a
    /// `core.conflict` statement about it listing them: written once it's
    /// checked out.
    fn write_merge(
        &self,
        working_copy: &WorkingCopy,
        merge: Merge,
        with: Vec<EncodedObject>,
        revision: &EncodedObject,
    ) -> Result<(Conflicts, Vec<Vec<EncodedObject>>), WorkspaceError> {
        // Finding the working copy's conflicts that remain reads its trees.
        self.write(&[merge.files.into_iter().chain(merge.trees).collect()])?;
        let conflicts = working_copy.conflicts.clone();
        let conflicts = conflicts
            .remaining(&self.store, merge.tree)?
            .with(merge.conflicts);
        let layers = self.revision_layers(with, revision, &conflicts);
        Ok((conflicts, layers))
    }

    /// Checks `revision`, `id`, out over the working copy, writes
    /// `unwritten`, the layers that write it if it's new, makes it the
    /// working copy, and says so, listing its unresolved `conflicts`.
    fn move_to(
        &mut self,
        working_copy: &WorkingCopy,
        id: RevisionId,
        revision: &Revision,
        unwritten: &[Vec<EncodedObject>],
        conflicts: &Conflicts,
    ) -> Result<(), WorkspaceError> {
        self.switch_to(working_copy, id, revision, unwritten, Why::Command)?;
        println!("Working copy: {}", self.headline(revision)?);
        println!("On top of: {}", self.describe_base(revision.base)?);
        if !conflicts.is_empty() {
            Self::print_conflicts(conflicts);
        }
        Ok(())
    }

    fn print_landing(&self, landing: &Landing, onto: &TargetName) -> Result<(), WorkspaceError> {
        match landing {
            Landing::Landed {
                proposal,
                source,
                acceptance,
            } => {
                let landed = match source {
                    Source::Revision(revision) => format!(", revision {},", revision.short()),
                    Source::Target(_) => String::new(),
                };
                println!(
                    "Landed {proposal}{landed} on {}",
                    self.describe_acceptance(acceptance.acceptance)?
                );
            }
            Landing::Unchanged(proposal) => {
                println!("Landing {proposal} would change nothing on `{onto}`");
            }
            Landing::Pending(pending) => Self::print_pending(pending),
        }
        Ok(())
    }

    fn print_pending(pending: &Pending) {
        let Pending {
            onto,
            proposal,
            subject,
            blocked,
        } = pending;
        let retry = match proposal {
            Proposal::Change(change) => format!("origin land {} --onto {onto}", change.short()),
            Proposal::Target(target) => format!("origin land --from {target} --onto {onto}"),
        };
        match blocked {
            Blocked::Evidence(missing) => {
                let missing: Vec<_> = missing.iter().map(ToString::to_string).collect();
                println!(
                    "The landing of {proposal} on `{onto}` waits for {}",
                    missing.join(", ")
                );
                println!(
                    "    about {}: once they're issued, run `{retry}`",
                    Self::describe_record(*subject)
                );
            }
            Blocked::Conflict { paths, .. } => {
                println!(
                    "The landing of {proposal} on `{onto}` conflicts in {}",
                    Self::list_paths(paths)
                );
                println!("    resolve it in a new revision, then run `{retry}`");
            }
        }
    }

    pub(super) fn describe_record(record: RecordId) -> String {
        match record {
            RecordId::Revision(id) => format!("revision {}", id.short()),
            RecordId::Snapshot(id) => format!("snapshot {}", id.short()),
            RecordId::Statement(id) => format!("statement {}", id.short()),
            RecordId::Acceptance(id) => format!("acceptance {}", id.short()),
        }
    }
}
