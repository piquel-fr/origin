//! The target commands: `land`, `target new` and `targets`, and starting a
//! change on a target with `new --on`.
//!
//! The queue runs in this process, as this node. After a landing, the
//! working copy of the workspace that ran it follows its change: when it
//! landed, the working copy starts a new change on the target's tip; when
//! the queue rebased it, the working copy becomes the rebased revision; and
//! when it sits on a change that landed, it is rebased onto the tip, if
//! that merges cleanly.
//!
//! Other workspaces are left alone until their next command, whose snapshot
//! catches up: if the working copy's change landed, it starts a new change
//! on the target's tip, keeping the edits made since.

use origin_shared::{
    Base, ChangeId, EncodedObject, Record, RecordId, RepoPath, Revision, RevisionId, Source,
    TargetName, TreeId,
};

use super::{WorkingCopy, Workspace, WorkspaceError};
use crate::index::Index;
use crate::oplog::Why;
use crate::policy::Policy;
use crate::queue::{Blocked, Landing, Pending, Proposal, Queue};

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
        let working_copy = self.refresh()?;
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
        self.follow(&working_copy, &onto, &landings)
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
    /// snapshotted, or the change cannot be written or checked out.
    pub fn new_on(&mut self, target: &TargetName, description: &str) -> Result<(), WorkspaceError> {
        let files = self.refresh()?.revision.snapshot;
        self.start_on(target, description, files, files)?;
        Ok(())
    }

    /// Starts a change described by `description` on the tip of `target`,
    /// with the edits that made `files` from `base`, and checks it out over
    /// `files`. Returns the paths where the edits conflict with the tip,
    /// and then starts nothing.
    fn start_on(
        &mut self,
        target: &TargetName,
        description: &str,
        base: TreeId,
        files: TreeId,
    ) -> Result<Vec<RepoPath>, WorkspaceError> {
        let tip = self
            .store
            .head(target)?
            .ok_or_else(|| WorkspaceError::UnknownTarget(target.clone()))?;
        let state = self.store.get(tip.acceptance)?.body().state;
        let merge = self.store.merge(Some(base), files, state)?;
        if !merge.conflicts.is_empty() {
            return Ok(merge.conflicts);
        }
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
        let mut first = merge.trees;
        first.push(description);
        self.write(&[first, vec![encoded.clone()]])?;
        self.switch_to(files, encoded.id().into(), revision.snapshot, Why::Command)?;
        println!("Working copy: {}", self.headline(&revision)?);
        println!("On top of: {}", self.describe_base(revision.base)?);
        Ok(Vec::new())
    }

    /// Follows the working copy's change once it landed from elsewhere,
    /// such as another workspace: when the change's only head landed, the
    /// working copy, `revision` with its files now `files`, starts a new
    /// change on the tip of the target it landed on, keeping the edits made
    /// since. Returns whether it moved: it stays if the edits conflict with
    /// the tip, and they then become a revision of the landed change.
    pub(super) fn catch_up(
        &mut self,
        index: &Index,
        revision: &Revision,
        files: TreeId,
    ) -> Result<bool, WorkspaceError> {
        let [head] = index.heads(revision.change)[..] else {
            return Ok(false);
        };
        let Some(landing) = self.store.landing(head, &self.store.get(head)?)? else {
            return Ok(false);
        };
        let target = self.target_name(landing.body().target)?;
        println!(
            "Change {} landed on {target} #{}",
            revision.change.short(),
            landing.body().seq
        );
        let conflicts = self.start_on(&target, "", revision.snapshot, files)?;
        if conflicts.is_empty() {
            return Ok(true);
        }
        println!(
            "The working copy stays on it: its edits conflict with `{target}` in {}",
            Self::paths(&conflicts)
        );
        Ok(false)
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

    /// Moves the working copy after `landings` on `onto`, as the module
    /// explains.
    fn follow(
        &mut self,
        working_copy: &WorkingCopy,
        onto: &TargetName,
        landings: &[Landing],
    ) -> Result<(), WorkspaceError> {
        let revision = &working_copy.revision;
        let files = revision.snapshot;
        let landed = |change: ChangeId| {
            landings.iter().any(|landing| {
                matches!(landing, Landing::Landed { proposal, .. }
                    if *proposal == Proposal::Change(change))
            })
        };
        if landed(revision.change) {
            self.start_on(onto, "", files, files)?;
            return Ok(());
        }
        for landing in landings {
            if let Landing::Pending(Pending {
                proposal: Proposal::Change(change),
                subject: RecordId::Revision(rebased),
                ..
            }) = landing
                && *change == revision.change
                && *rebased != working_copy.id
            {
                let rebased = *rebased;
                self.switch_to(
                    files,
                    rebased,
                    self.store.get(rebased)?.snapshot,
                    Why::Command,
                )?;
                println!(
                    "Working copy: {}",
                    self.headline(&self.store.get(rebased)?)?
                );
                return Ok(());
            }
        }
        let Base::Revision(below) = revision.base else {
            return Ok(());
        };
        let below = self.store.get(below)?;
        if !landed(below.change) {
            return Ok(());
        }
        self.rebase_onto_tip(working_copy, below.snapshot, onto)
    }

    /// Rebases the working copy, made on `base`, onto the tip of `onto`, if
    /// that merges cleanly.
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
        let merge = self.store.merge(Some(base), current.snapshot, state)?;
        if !merge.conflicts.is_empty() {
            println!(
                "The working copy stays where it is: it conflicts with `{onto}` in {}",
                Self::paths(&merge.conflicts)
            );
            return Ok(());
        }
        let revision = Revision {
            predecessors: vec![working_copy.id],
            base: Base::Acceptance(tip.acceptance),
            snapshot: merge.tree,
            author: self.config.user(),
            ..current.clone()
        };
        let encoded = revision.encode();
        self.write(&[merge.trees, vec![encoded.clone()]])?;
        self.switch_to(
            current.snapshot,
            encoded.id().into(),
            revision.snapshot,
            Why::Command,
        )?;
        println!("Working copy: {}", self.headline(&revision)?);
        println!("On top of: {}", self.describe_base(revision.base)?);
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
                    Self::paths(paths)
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

    fn paths(paths: &[RepoPath]) -> String {
        let paths: Vec<_> = paths.iter().map(ToString::to_string).collect();
        paths.join(", ")
    }
}
