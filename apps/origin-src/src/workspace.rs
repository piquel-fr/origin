//! A working copy and its repository in `.origin/`.
//!
//! ```text
//! .origin/
//!   objects/ targets/ index/   the store, see `store`, `targets` and `index`
//!   workspace                  the revision the working copy is
//!   state                      the stat cache of the last snapshot
//! ```
//!
//! The working copy is always a revision of the current change. Every
//! command that reads it snapshots it first: when the files changed, they
//! become a new revision of the change, which evolves from the previous one.
//! Records are written before anything refers to them: the snapshot's
//! objects, then the revision, then the `workspace` file naming it.

mod annotate;

use std::fs;
use std::io;
use std::path::{self, Path, PathBuf};

use origin_platform::{LockedFile, Native};
use origin_shared::{
    AcceptanceBody, AcceptanceId, Base, BlobId, ChangeId, EncodedObject, EntryKind, EntryName,
    Kind, NodePublicKey, Principal, Record, RecordId, Revision, RevisionId, Source, Statement,
    TargetId, TargetName, Tree, TreeEntry, TreeId,
};

use crate::annotations::AnnotateError;
use crate::config::{ConfigError, UserConfig};
use crate::index::{FindChangeError, FindError, Index};
use crate::output::FileDiff;
use crate::policy::PolicyError;
use crate::store::{Store, StoreError};
use crate::worktree::{SnapshotError, StatCache, Worktree};

/// A working copy and its repository.
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    store: Store,
    config: UserConfig,
    /// The revision the working copy was when last snapshotted.
    revision: RevisionId,
}

/// The working copy as of a snapshot.
struct WorkingCopy {
    id: RevisionId,
    revision: Revision,
    /// The revision's record, if the snapshot made a new revision, which is
    /// not written yet.
    unsaved: Option<EncodedObject>,
    /// Keeps garbage collection away from the objects the snapshot found
    /// stored, until the command that saves it is done.
    _held: LockedFile,
}

impl WorkingCopy {
    /// The change `prefix` names: `@` for the working copy's, or a unique
    /// prefix of its id.
    fn find_change(&self, index: &Index, prefix: &str) -> Result<ChangeId, FindChangeError> {
        if prefix == "@" {
            return Ok(self.revision.change);
        }
        index.find_change(prefix)
    }
}

impl Workspace {
    /// The metadata directory at the root of every working copy.
    pub const META_DIR: &str = ".origin";

    /// `origin init`: creates a repository in `root`, with the `policy`
    /// target naming this node the authority of `policy` and `main`, an
    /// empty `main`, and a first change on top of it.
    ///
    /// # Errors
    ///
    /// Fails if `root` already has a repository, the node key cannot be
    /// loaded, or the repository cannot be written.
    pub fn init(root: &Path, config: UserConfig) -> Result<Self, WorkspaceError> {
        let root = path::absolute(root).map_err(|source| WorkspaceError::io(root, source))?;
        let meta = root.join(Self::META_DIR);
        if meta.exists() {
            return Err(WorkspaceError::AlreadyInitialized(root));
        }
        let store = Store::new(meta, Native::default());
        store.init()?;
        let key = config.node_key()?;

        let (policy, policy_tree) = Self::policy(&config, &key.public());
        let empty_tree = Tree::default().encode();
        let empty_description = EncodedObject::blob(b"");
        let genesis = |target: &TargetName, state: &EncodedObject, policy| AcceptanceBody {
            target: target.id(),
            seq: 0,
            prev: None,
            state: state.id().into(),
            landed: Vec::new(),
            evidence: Vec::new(),
            policy,
        };
        store.write_all(&[
            policy,
            policy_tree.clone(),
            empty_tree.clone(),
            empty_description.clone(),
        ])?;
        let policy = store.append(
            &TargetName::POLICY,
            &genesis(&TargetName::POLICY, &policy_tree, None).sign(&key),
        )?;
        let main = store.append(
            &TargetName::MAIN,
            &genesis(&TargetName::MAIN, &empty_tree, Some(policy)).sign(&key),
        )?;

        let revision = Revision {
            change: ChangeId::random().map_err(WorkspaceError::Random)?,
            predecessors: Vec::new(),
            base: Base::Acceptance(main),
            deps: Vec::new(),
            snapshot: empty_tree.id().into(),
            description: empty_description.id().into(),
            author: config.user(),
        };
        let encoded = revision.encode();
        let mut workspace = Self {
            root,
            store,
            config,
            revision: encoded.id().into(),
        };
        workspace.save(&[vec![encoded]], workspace.revision)?;
        println!(
            "Initialized a repository in {}: node:{} is the authority of `policy` and `main`",
            workspace.root.display(),
            workspace.config.node()
        );
        println!("Working copy: {}", workspace.headline(&revision)?);
        Ok(workspace)
    }

    /// The first `policy.toml`, and the snapshot holding it: this node is
    /// the authority of `policy` and `main`, and neither requires anything,
    /// so a solo project lands instantly.
    fn policy(config: &UserConfig, key: &NodePublicKey) -> (EncodedObject, EncodedObject) {
        let node = config.node();
        let policy = format!(
            "# Who may land what. Changing it is a landing on `policy`.\n\
             \n\
             [nodes]\n\
             \"{node}\" = \"{key}\"\n\
             \n\
             [targets.policy]\n\
             authority = \"node:{node}\"\n\
             \n\
             [targets.main]\n\
             authority = \"node:{node}\"\n"
        );
        let policy = EncodedObject::blob(policy.as_bytes());
        let tree: Tree = [(
            EntryName::new("policy.toml").expect("a valid name"),
            TreeEntry {
                kind: EntryKind::File,
                id: policy.id(),
            },
        )]
        .into_iter()
        .collect();
        (policy, tree.encode())
    }

    /// Opens the working copy containing `start`.
    ///
    /// # Errors
    ///
    /// Fails if neither `start` nor any parent has a repository, it is on a
    /// network file system, or its working copy file is unreadable.
    pub fn discover(start: &Path, config: UserConfig) -> Result<Self, WorkspaceError> {
        let root = start
            .ancestors()
            .find(|dir| dir.join(Self::META_DIR).is_dir())
            .ok_or_else(|| WorkspaceError::NotFound(start.to_owned()))?;
        let meta = root.join(Self::META_DIR);
        let path = meta.join("workspace");
        let revision = fs::read_to_string(&path)
            .map_err(|source| WorkspaceError::io(&path, source))?
            .trim_end()
            .parse()
            .map_err(|_| WorkspaceError::Malformed(path))?;
        Ok(Self {
            root: root.to_owned(),
            store: Store::open_writable(meta, Native::default())?,
            config,
            revision,
        })
    }

    fn meta(&self, name: &str) -> PathBuf {
        self.root.join(Self::META_DIR).join(name)
    }

    /// The root of the working copy.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Snapshots the working copy. The revision is new if the files
    /// changed, and then still has to be saved.
    fn snapshot(&self) -> Result<WorkingCopy, WorkspaceError> {
        let held = self.store.hold_objects()?;
        let revision: Revision = self.store.get(self.revision)?;
        let path = self.meta("state");
        let (tree, cache) =
            Worktree::new(&self.root, &self.store).snapshot(&StatCache::load(&path))?;
        if let Some(cache) = cache {
            cache
                .save(&path)
                .map_err(|source| WorkspaceError::io(&path, source))?;
        }
        if tree == revision.snapshot {
            return Ok(WorkingCopy {
                id: self.revision,
                revision,
                unsaved: None,
                _held: held,
            });
        }
        let revision = Revision {
            predecessors: vec![self.revision],
            snapshot: tree,
            author: self.config.user(),
            ..revision
        };
        let encoded = revision.encode();
        Ok(WorkingCopy {
            id: encoded.id().into(),
            revision,
            unsaved: Some(encoded),
            _held: held,
        })
    }

    /// Snapshots the working copy, saving a new revision if the files
    /// changed.
    fn refresh(&mut self) -> Result<WorkingCopy, WorkspaceError> {
        let mut working_copy = self.snapshot()?;
        if let Some(unsaved) = working_copy.unsaved.take() {
            self.save(&[vec![unsaved]], working_copy.id)?;
        }
        Ok(working_copy)
    }

    /// Writes `layers` of objects, then makes `revision` the working copy,
    /// flagging its change if that made it diverge.
    fn save(
        &mut self,
        layers: &[Vec<EncodedObject>],
        revision: RevisionId,
    ) -> Result<(), WorkspaceError> {
        self.write(layers)?;
        self.point_at(revision)?;
        self.flag_divergence(revision)
    }

    /// Writes `layers` of objects in order, each durable before the next,
    /// so objects may refer to those of earlier layers.
    fn write(&self, layers: &[Vec<EncodedObject>]) -> Result<(), WorkspaceError> {
        for layer in layers.iter().filter(|layer| !layer.is_empty()) {
            self.store.write_all(layer)?;
        }
        Ok(())
    }

    /// Durably records that the working copy is `revision`.
    fn point_at(&mut self, revision: RevisionId) -> Result<(), WorkspaceError> {
        let write = self.store.durable_write();
        write
            .stage(self.meta("workspace"), format!("{revision}\n").as_bytes())
            .and_then(|()| write.commit())
            .map_err(StoreError::from)?;
        self.revision = revision;
        Ok(())
    }

    /// Makes the files match `to`, given that they match `from`.
    fn checkout(&self, from: TreeId, to: TreeId) -> Result<(), WorkspaceError> {
        let changes = self.store.diff(from, to)?;
        Ok(Worktree::new(&self.root, &self.store).checkout(&changes)?)
    }

    /// `origin status`: prints the working copy's change, what it is on
    /// top of, whether it diverged, and the files it changes.
    ///
    /// # Errors
    ///
    /// Fails if the working copy cannot be snapshotted, or the records it
    /// refers to cannot be read.
    pub fn status(&mut self) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let revision = &working_copy.revision;
        let index = Index::open(&self.store)?;
        println!(
            "Working copy: {}{}",
            self.headline(revision)?,
            self.flags(&index, revision.change)?
        );
        println!("On top of: {}", self.describe_base(revision.base)?);
        Self::print_divergence(&index, revision.change);
        let changes = self
            .store
            .diff(self.base_tree(revision.base)?, revision.snapshot)?;
        if changes.is_empty() {
            println!("No changes.");
        }
        for change in changes {
            let marker = match (change.old, change.new) {
                (None, _) => 'A',
                (_, None) => 'D',
                _ => 'M',
            };
            println!("{marker} {}", change.path);
        }
        Ok(())
    }

    /// `origin diff`: prints the working copy's changes, line by line.
    ///
    /// # Errors
    ///
    /// Fails like [`Workspace::status`], or if a changed file's content
    /// cannot be read.
    pub fn diff(&mut self) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        self.print_diff(&working_copy.revision)
    }

    /// `origin show [<change>]`: prints a change's head, or each of them if
    /// it diverged, and its diff. Shows the working copy by default.
    ///
    /// # Errors
    ///
    /// Fails if the change cannot be found, or like [`Workspace::diff`].
    pub fn show(&mut self, change: Option<&str>) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let index = Index::open(&self.store)?;
        let revisions = match change {
            None => vec![working_copy.id],
            Some(change) => index.heads(working_copy.find_change(&index, change)?),
        };
        for (position, id) in revisions.into_iter().enumerate() {
            if position > 0 {
                println!();
            }
            let revision: Revision = self.store.get(id)?;
            println!(
                "Change {}{}",
                revision.change.short(),
                self.flags(&index, revision.change)?
            );
            println!("Revision {} by {}", id.short(), revision.author);
            println!("On top of: {}", self.describe_base(revision.base)?);
            for (issuer, reason) in self.abandonments(&index, id)? {
                println!("Abandoned by {issuer}: {reason}");
            }
            println!();
            let description = self.description(&revision)?;
            if description.trim().is_empty() {
                println!("    (no description)");
            }
            for line in description.lines() {
                println!("    {line}");
            }
            println!();
            self.print_diff(&revision)?;
        }
        Ok(())
    }

    fn print_diff(&self, revision: &Revision) -> Result<(), WorkspaceError> {
        let base = self.base_tree(revision.base)?;
        for change in self.store.diff(base, revision.snapshot)? {
            print!("{}", FileDiff::new(&self.store, &change)?);
        }
        Ok(())
    }

    /// `origin new [description]`: finishes the current change, and starts
    /// a new one on top of it.
    ///
    /// # Errors
    ///
    /// Fails if the working copy cannot be snapshotted or the new change
    /// cannot be written.
    pub fn new_change(&mut self, description: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.snapshot()?;
        let description = EncodedObject::blob(description.as_bytes());
        let revision = Revision {
            change: ChangeId::random().map_err(WorkspaceError::Random)?,
            predecessors: Vec::new(),
            base: Base::Revision(working_copy.id),
            deps: Vec::new(),
            snapshot: working_copy.revision.snapshot,
            description: description.id().into(),
            author: self.config.user(),
        };
        let encoded = revision.encode();
        let id = encoded.id().into();
        let first = working_copy.unsaved.into_iter().chain([description]);
        self.save(&[first.collect(), vec![encoded]], id)?;
        println!("Working copy: {}", self.headline(&revision)?);
        println!("On top of: {}", self.headline(&working_copy.revision)?);
        Ok(())
    }

    /// `origin describe <description>`: replaces the current change's
    /// description, with a new revision.
    ///
    /// # Errors
    ///
    /// Fails if the working copy cannot be snapshotted or the revision
    /// cannot be written.
    pub fn describe(&mut self, description: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.snapshot()?;
        let description = EncodedObject::blob(description.as_bytes());
        let revision = Revision {
            predecessors: vec![working_copy.id],
            description: description.id().into(),
            author: self.config.user(),
            ..working_copy.revision
        };
        let encoded = revision.encode();
        let id = encoded.id().into();
        let first = working_copy.unsaved.into_iter().chain([description]);
        self.save(&[first.collect(), vec![encoded]], id)?;
        println!("Working copy: {}", self.headline(&revision)?);
        Ok(())
    }

    /// `origin edit <change>`: switches the working copy to the change's
    /// head.
    ///
    /// # Errors
    ///
    /// Fails if the change cannot be found or diverged, or the working copy
    /// cannot be snapshotted or checked out.
    pub fn edit(&mut self, change: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let index = Index::open(&self.store)?;
        let change = working_copy.find_change(&index, change)?;
        let head = Self::single_head(&index, change)?;
        let revision: Revision = self.store.get(head)?;
        if head != working_copy.id {
            self.checkout(working_copy.revision.snapshot, revision.snapshot)?;
            self.point_at(head)?;
        }
        println!("Working copy: {}", self.headline(&revision)?);
        Ok(())
    }

    /// `origin abandon [<change>] --reason <reason>`: records that the
    /// change was given up, and why, with a `core.abandon` statement on its
    /// head. Abandoning the current change starts a new one where it
    /// started, and restores those files.
    ///
    /// # Errors
    ///
    /// Fails if the change cannot be found or diverged, or the records
    /// cannot be written.
    pub fn abandon(&mut self, change: Option<&str>, reason: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let index = Index::open(&self.store)?;
        let change = match change {
            Some(change) => working_copy.find_change(&index, change)?,
            None => working_copy.revision.change,
        };
        let head = Self::single_head(&index, change)?;
        let reason = EncodedObject::blob(reason.as_bytes());
        let statement = Statement {
            subject: RecordId::Revision(head),
            kind: Kind::ABANDON,
            verdict: None,
            issuer: self.config.user(),
            inputs: None,
            payload: Some(reason.id().into()),
            supersedes: None,
            promoted_from: None,
        }
        .encode();
        if change != working_copy.revision.change {
            self.write(&[vec![reason], vec![statement]])?;
            println!("Abandoned change {}", change.short());
            return Ok(());
        }

        let abandoned: Revision = self.store.get(head)?;
        let empty = EncodedObject::blob(b"");
        let revision = Revision {
            change: ChangeId::random().map_err(WorkspaceError::Random)?,
            predecessors: Vec::new(),
            base: abandoned.base,
            deps: abandoned.deps,
            snapshot: self.base_tree(abandoned.base)?,
            description: empty.id().into(),
            author: self.config.user(),
        };
        let encoded = revision.encode();
        self.write(&[vec![reason, empty], vec![statement, encoded.clone()]])?;
        self.checkout(working_copy.revision.snapshot, revision.snapshot)?;
        self.point_at(encoded.id().into())?;
        println!("Abandoned change {}", change.short());
        println!("Working copy: {}", self.headline(&revision)?);
        println!("On top of: {}", self.describe_base(revision.base)?);
        Ok(())
    }

    /// `origin converge <change>`: ends a change's divergence with a
    /// revision evolving from all its heads. It keeps the files and
    /// description of the working copy, if it is one of the heads, or else
    /// of the newest head.
    ///
    /// # Errors
    ///
    /// Fails if the change cannot be found or didn't diverge, or the
    /// revision cannot be written.
    pub fn converge(&mut self, change: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let index = Index::open(&self.store)?;
        let change = working_copy.find_change(&index, change)?;
        let heads = index.heads(change);
        if heads.len() < 2 {
            return Err(WorkspaceError::NotDivergent(change));
        }
        let kept = if heads.contains(&working_copy.id) {
            working_copy.id
        } else {
            heads[heads.len() - 1]
        };
        let revision = Revision {
            predecessors: heads.clone(),
            author: self.config.user(),
            ..self.store.get(kept)?
        };
        let encoded = revision.encode();
        let id = encoded.id().into();
        self.write(&[vec![encoded]])?;
        if change == working_copy.revision.change {
            self.checkout(working_copy.revision.snapshot, revision.snapshot)?;
            self.point_at(id)?;
        }
        println!(
            "Converged change {}: revision {} evolves from {}, with the files of {}",
            change.short(),
            id.short(),
            Self::list(&heads),
            kept.short()
        );
        Ok(())
    }

    /// `origin log [<target>]`: prints the working copy's stack of changes
    /// down to the target it's on, then the target's acceptances, newest
    /// first. With a target, prints that target's acceptances instead,
    /// under the stack if it's on that target.
    ///
    /// # Errors
    ///
    /// Fails if the target doesn't exist, or a record cannot be read.
    pub fn log(
        &mut self,
        target: Option<TargetName>,
        limit: Option<usize>,
    ) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let index = Index::open(&self.store)?;
        let mut next = working_copy.revision.base;
        let mut stack = vec![(working_copy.id, working_copy.revision)];
        let acceptance = loop {
            match next {
                Base::Revision(id) => {
                    let revision: Revision = self.store.get(id)?;
                    next = revision.base;
                    stack.push((id, revision));
                }
                Base::Acceptance(id) => break id,
            }
        };
        let on = self.store.get(acceptance)?.body().target;
        let target = match target {
            Some(target) => target,
            None => self.target_name(on)?,
        };
        let log = self.store.log(&target)?;
        if log.is_empty() {
            return Err(WorkspaceError::UnknownTarget(target));
        }
        if target.id() == on {
            for (id, revision) in &stack {
                let marker = if *id == working_copy.id { '@' } else { '○' };
                println!(
                    "{marker} {}{}",
                    self.headline(revision)?,
                    self.flags(&index, revision.change)?
                );
            }
        }
        let shown = log
            .iter()
            .enumerate()
            .rev()
            .take(limit.unwrap_or(usize::MAX));
        for (seq, &id) in shown {
            println!("◆ {target} #{seq} {}", id.short());
            for source in &self.store.get(id)?.body().landed {
                match *source {
                    Source::Revision(id) => {
                        let revision = self.store.get(id)?;
                        println!(
                            "    {}{}",
                            self.headline(&revision)?,
                            self.flags(&index, revision.change)?
                        );
                    }
                    Source::Target(id) => {
                        println!("    {}", self.describe_acceptance(id)?);
                    }
                }
            }
        }
        Ok(())
    }

    /// `origin evolog <change>`: prints every revision of a change, newest
    /// first, marking its heads.
    ///
    /// # Errors
    ///
    /// Fails if the change cannot be found, or a record cannot be read.
    pub fn evolog(&mut self, change: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let index = Index::open(&self.store)?;
        let change = working_copy.find_change(&index, change)?;
        let heads = index.heads(change);
        println!("Change {}{}", change.short(), self.flags(&index, change)?);
        for &id in index.revisions(change).iter().rev() {
            let revision: Revision = self.store.get(id)?;
            let marker = if id == working_copy.id { '@' } else { '○' };
            let head = if heads.contains(&id) { " (head)" } else { "" };
            println!("{marker} {} {}{head}", id.short(), self.summary(&revision)?);
            if revision.predecessors.len() > 1 {
                println!("    converges {}", Self::list(&revision.predecessors));
            }
        }
        Ok(())
    }

    /// `origin index rebuild`: reconstructs the index from the store.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read or the index written.
    pub fn rebuild_index(&self) -> Result<(), WorkspaceError> {
        let index = Index::rebuild(&self.store)?;
        println!("Rebuilt the index of {} records", index.arrivals().len());
        Ok(())
    }

    /// The head of a change that didn't diverge.
    fn single_head(index: &Index, change: ChangeId) -> Result<RevisionId, WorkspaceError> {
        match index.heads(change)[..] {
            [head] => Ok(head),
            ref heads => Err(WorkspaceError::Divergent {
                change,
                heads: heads.to_vec(),
            }),
        }
    }

    /// The description of a revision.
    fn description(&self, revision: &Revision) -> Result<String, WorkspaceError> {
        self.text(revision.description)
    }

    /// A blob's content, as text.
    fn text(&self, blob: BlobId) -> Result<String, WorkspaceError> {
        let id = blob.into();
        let blob = self.store.read(id)?;
        let content = blob
            .blob_content()
            .map_err(|source| StoreError::Decode { id, source })?;
        Ok(String::from_utf8_lossy(content).into_owned())
    }

    /// The first line of a revision's description.
    fn summary(&self, revision: &Revision) -> Result<String, WorkspaceError> {
        let description = self.description(revision)?;
        Ok(match description.lines().next() {
            Some(line) if !line.trim().is_empty() => line.to_owned(),
            _ => "(no description)".to_owned(),
        })
    }

    /// A revision's change and summary.
    fn headline(&self, revision: &Revision) -> Result<String, WorkspaceError> {
        Ok(format!(
            "{} {}",
            revision.change.short(),
            self.summary(revision)?
        ))
    }

    /// Marks a divergent or abandoned change.
    fn flags(&self, index: &Index, change: ChangeId) -> Result<String, WorkspaceError> {
        let heads = index.heads(change);
        let mut flags = String::new();
        if heads.len() > 1 {
            flags.push_str(" (divergent)");
        }
        for head in heads {
            if !self.abandonments(index, head)?.is_empty() {
                flags.push_str(" (abandoned)");
                break;
            }
        }
        Ok(flags)
    }

    /// Who abandoned a revision, and why: only those who may, as
    /// [`Store::authorizes`] says.
    fn abandonments(
        &self,
        index: &Index,
        revision: RevisionId,
    ) -> Result<Vec<(Principal, String)>, WorkspaceError> {
        let mut abandonments = Vec::new();
        for &id in index.statements(RecordId::Revision(revision)) {
            let statement = self.store.get(id)?;
            if statement.kind == Kind::ABANDON && self.store.authorizes(&statement)? {
                let reason = match statement.payload {
                    Some(payload) => self.text(payload)?,
                    None => String::new(),
                };
                abandonments.push((statement.issuer, reason));
            }
        }
        Ok(abandonments)
    }

    /// Explains a divergent change's heads, and how to converge them.
    fn print_divergence(index: &Index, change: ChangeId) {
        let heads = index.heads(change);
        if heads.len() > 1 {
            println!(
                "Divergent: change {} has {} heads, {}. Run `origin converge {}`.",
                change.short(),
                heads.len(),
                Self::list(&heads),
                change.short()
            );
        }
    }

    fn list(revisions: &[RevisionId]) -> String {
        let shorts: Vec<_> = revisions.iter().map(RevisionId::short).collect();
        shorts.join(", ")
    }

    /// The snapshot a revision was made on top of.
    fn base_tree(&self, base: Base) -> Result<TreeId, WorkspaceError> {
        Ok(match base {
            Base::Acceptance(id) => self.store.get(id)?.body().state,
            Base::Revision(id) => self.store.get(id)?.snapshot,
        })
    }

    fn describe_base(&self, base: Base) -> Result<String, WorkspaceError> {
        match base {
            Base::Acceptance(id) => self.describe_acceptance(id),
            Base::Revision(id) => self.headline(&self.store.get(id)?),
        }
    }

    /// A target and position, such as `main #3`.
    fn describe_acceptance(&self, id: AcceptanceId) -> Result<String, WorkspaceError> {
        let acceptance = self.store.get(id)?;
        let body = acceptance.body();
        Ok(format!("{} #{}", self.target_name(body.target)?, body.seq))
    }

    fn target_name(&self, id: TargetId) -> Result<TargetName, WorkspaceError> {
        self.store
            .targets()?
            .into_iter()
            .find(|target| target.id() == id)
            .ok_or(WorkspaceError::UnknownTargetId(id))
    }
}

/// A failure to operate on a working copy.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// No repository was found.
    #[error("not in an origin repository: {} or its parents have no `.origin` directory", .0.display())]
    NotFound(PathBuf),
    /// `init` found an existing repository.
    #[error("{} already has a repository", .0.display())]
    AlreadyInitialized(PathBuf),
    /// The change has several heads, and the command needs one.
    #[error(
        "change {} is divergent: its heads are {}. Run `origin converge {}` first",
        change.short(),
        Workspace::list(heads),
        change.short()
    )]
    Divergent {
        /// The change.
        change: ChangeId,
        /// Its heads.
        heads: Vec<RevisionId>,
    },
    /// `converge` was given a change with a single head.
    #[error("change {} is not divergent", .0.short())]
    NotDivergent(ChangeId),
    /// No target has this name.
    #[error("no target is named `{0}`")]
    UnknownTarget(TargetName),
    /// An acceptance belongs to a target this repository doesn't have.
    #[error("no target has the id {0}")]
    UnknownTargetId(TargetId),
    /// A change id prefix doesn't name one change.
    #[error(transparent)]
    FindChange(#[from] FindChangeError),
    /// The working copy file isn't a revision id.
    #[error("{} is not a revision id", .0.display())]
    Malformed(PathBuf),
    /// A metadata file cannot be accessed.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The operating system has no randomness for a change id.
    #[error("cannot generate a change id")]
    Random(#[source] getrandom::Error),
    /// The user's configuration is unusable.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Objects cannot be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The working copy cannot be snapshotted or checked out.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// A revision id prefix doesn't name one revision.
    #[error(transparent)]
    FindRevision(#[from] FindError<RevisionId>),
    /// Only origin itself issues statements of `core.` kinds.
    #[error("`{0}` is a reserved kind: only origin itself issues `core.` statements")]
    ReservedKind(Kind),
    /// A statement cannot be issued.
    #[error(transparent)]
    Annotate(#[from] AnnotateError),
    /// The policy cannot be read.
    #[error(transparent)]
    Policy(#[from] PolicyError),
}

impl WorkspaceError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}
