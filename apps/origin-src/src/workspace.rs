//! A working copy and its repository in `.origin/`.
//!
//! ```text
//! .origin/
//!   objects/ targets/ index/   the store, see `store`, `targets` and `index`
//!   oplog/                     what this node applied, see `oplog`
//!   workspaces/                the other workspaces, see `registry`
//!   workspace                  the revision the working copy is
//!   state                      the stat cache of the last snapshot
//!   moving                     the move in progress, if a checkout is
//! ```
//!
//! A repository can have several working copies, its workspaces, which
//! share its store and op log. The others have only `workspace`, `state`
//! and `moving` in their `.origin`, and a `repository` file naming the main
//! one's.
//!
//! The working copy is always a revision of the current change. Every
//! command that reads it snapshots it first: when the files changed, they
//! become a new revision of the change, which evolves from the previous one.
//! Records are written before anything refers to them: the snapshot's
//! objects, then the revision, then the `workspace` file naming it.
//!
//! Every move of the working copy is logged in the op log before the
//! `workspace` file names the new revision. A move that changes the files
//! writes `moving` first, naming where it goes, and removes it once
//! `workspace` names the new revision. `moving` isn't synced, as the files a
//! checkout writes aren't: it only has to survive a process crash, which can
//! leave the files half checked out. The next command to open the workspace
//! then finishes the move: it snapshots the files as they are, so nothing on
//! disk is lost, checks the revision out over them, and points `workspace`
//! at it.

mod annotate;
mod land;
mod ops;
mod registry;

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{self, Path, PathBuf};

use origin_platform::{LockedFile, Native};
use origin_shared::{
    AcceptanceId, Base, BlobId, ChangeId, EncodedObject, Kind, NodePublicKey, Principal, Record,
    RecordId, RepoPath, Revision, RevisionId, Source, Statement, TargetId, TargetName, Tree,
    TreeId,
};

use crate::annotations::{AnnotateError, Annotations};
use crate::config::{ConfigError, UserConfig};
use crate::conflict::Conflict;
use crate::index::{FindChangeError, FindError, Index};
use crate::oplog::{Actor, Effect, Move, Why};
use crate::output::FileDiff;
use crate::policy::PolicyError;
use crate::queue::{LandError, Queue};
use crate::store::{Store, StoreError};
use crate::worktree::{CloneSource, SnapshotError, StatCache, Worktree};

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
    /// The record index, as of the snapshot: once saved, it has the
    /// revision.
    index: Index,
    /// Keeps garbage collection away from the objects the snapshot found
    /// stored, until the command that saves it is done.
    _held: LockedFile,
}

impl WorkingCopy {
    /// The change `prefix` names: `@` for the working copy's, or a unique
    /// prefix of its id.
    fn find_change(&self, prefix: &str) -> Result<ChangeId, FindChangeError> {
        if prefix == "@" {
            return Ok(self.revision.change);
        }
        self.index.find_change(prefix)
    }
}

impl Workspace {
    /// The metadata directory at the root of every working copy.
    pub const META_DIR: &str = ".origin";

    /// The file in [`Workspace::META_DIR`] naming the revision the working
    /// copy is.
    const POINTER: &str = "workspace";

    /// The file in [`Workspace::META_DIR`] holding the stat cache.
    const STATE: &str = "state";

    /// The file in a workspace's [`Workspace::META_DIR`] naming the main
    /// one's, which holds the store.
    const REPOSITORY: &str = "repository";

    /// The file in [`Workspace::META_DIR`] naming the revision a checkout in
    /// progress moves the working copy to, and why: `<revision> <why>`.
    const MOVING: &str = "moving";

    /// `origin init`: creates a repository in `root`, with the `policy`
    /// target naming this node the authority of `policy` and `main`, an
    /// empty `main`, and a first change on top of it.
    ///
    /// # Errors
    ///
    /// Fails if `root` already has a repository, the node key cannot be
    /// loaded, or the repository cannot be written.
    pub fn init(root: &Path, config: UserConfig, actor: &Actor) -> Result<Self, WorkspaceError> {
        let root = path::absolute(root).map_err(|source| WorkspaceError::io(root, source))?;
        let meta = root.join(Self::META_DIR);
        if meta.exists() {
            return Err(WorkspaceError::AlreadyInitialized(root));
        }
        let store = Store::new(meta, Native::default()).with_actor(actor.in_workspace(&root));
        store.init()?;
        let key = config.node_key()?;

        let policy = Self::policy(&config, &key.public());
        let main = Queue::new(&store, &key, config.node().clone()).init(&policy)?;
        let empty_tree = Tree::default().encode();
        let empty_description = EncodedObject::blob(b"");

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
        workspace.write(&[vec![empty_description], vec![encoded]])?;
        workspace.publish_pointer(None, workspace.revision, Why::Command)?;
        println!(
            "Initialized a repository in {}: node:{} is the authority of `policy` and `main`",
            workspace.root.display(),
            workspace.config.node()
        );
        println!("Working copy: {}", workspace.headline(&revision)?);
        Ok(workspace)
    }

    /// The first `policy.toml`: this node is the authority of `policy` and
    /// `main`, and neither requires anything, so a solo project lands
    /// instantly.
    fn policy(config: &UserConfig, key: &NodePublicKey) -> String {
        let node = config.node();
        format!(
            "# Who may land what. Changing it is a landing on `policy`.\n\
             \n\
             # Each node's public key.\n\
             [nodes]\n\
             \"{node}\" = \"{key}\"\n\
             \n\
             # Groups of users and nodes, for requirements:\n\
             # [groups]\n\
             # runners = [\"node:ci-1\"]\n\
             \n\
             # The node ordering each target, and the statements landing needs.\n\
             [targets.policy]\n\
             authority = \"node:{node}\"\n\
             \n\
             [targets.main]\n\
             authority = \"node:{node}\"\n\
             # require = [{{ kind = \"ci.test\", verdict = \"pass\", issuer = \"group:runners\" }}]\n"
        )
    }

    /// Opens the working copy containing `start`, for `actor` to write in.
    ///
    /// # Errors
    ///
    /// Fails if neither `start` nor any parent has a repository, or like
    /// [`Workspace::open`].
    pub fn discover(
        start: &Path,
        config: UserConfig,
        actor: &Actor,
    ) -> Result<Self, WorkspaceError> {
        let root = start
            .ancestors()
            .find(|dir| dir.join(Self::META_DIR).is_dir())
            .ok_or_else(|| WorkspaceError::NotFound(start.to_owned()))?;
        Self::open(root, config, actor)
    }

    /// Opens the working copy at `root`: the main one, whose `.origin` holds
    /// the store, or another workspace, whose `.origin/repository` names
    /// the main one's. Finishes a move a crash cut short first.
    fn open(root: &Path, config: UserConfig, actor: &Actor) -> Result<Self, WorkspaceError> {
        let meta = root.join(Self::META_DIR);
        let repository = meta.join(Self::REPOSITORY);
        let store = match Self::read_path(&repository) {
            Ok(store) if store.is_dir() => store,
            Ok(store) => return Err(WorkspaceError::MissingRepository(store)),
            Err(WorkspaceError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                meta
            }
            Err(error) => return Err(error),
        };
        let revision =
            Self::read_pointer(root)?.ok_or_else(|| WorkspaceError::Incomplete(root.to_owned()))?;
        let mut workspace = Self {
            root: root.to_owned(),
            store: Store::open_writable(store, Native::default())?
                .with_actor(actor.in_workspace(root)),
            config,
            revision,
        };
        workspace.finish_move()?;
        Ok(workspace)
    }

    /// Finishes the move `moving` names, if a crash cut it short: records
    /// the files as they are, checks the revision out over them, and points
    /// the working copy at it.
    fn finish_move(&mut self) -> Result<(), WorkspaceError> {
        let path = self.meta(Self::MOVING);
        let moving = match fs::read_to_string(&path) {
            Ok(moving) => moving,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(WorkspaceError::io(&path, source)),
        };
        let (to, why) = moving
            .trim_end()
            .split_once(' ')
            .and_then(|(to, why)| Some((to.parse().ok()?, Why::parse(why)?)))
            .ok_or_else(|| WorkspaceError::Malformed(path.clone()))?;
        let state = self.meta(Self::STATE);
        let (files, cache) =
            Worktree::new(&self.root, &self.store).snapshot(&StatCache::load(&state))?;
        if let Some(cache) = cache {
            cache
                .save(&state)
                .map_err(|source| WorkspaceError::io(&state, source))?;
        }
        let revision: Revision = self.store.get(to)?;
        self.checkout(files, revision.snapshot)?;
        if self.revision != to {
            self.point_at(to, why)?;
        }
        fs::remove_file(&path).map_err(|source| WorkspaceError::io(&path, source))?;
        println!(
            "Finished moving the working copy to {}, which a crash cut short",
            self.headline(&revision)?
        );
        Ok(())
    }

    /// The revision the working copy at `root` is, if it has one.
    fn read_pointer(root: &Path) -> Result<Option<RevisionId>, WorkspaceError> {
        let path = root.join(Self::META_DIR).join(Self::POINTER);
        let pointer = match fs::read_to_string(&path) {
            Ok(pointer) => pointer,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(WorkspaceError::io(&path, source)),
        };
        let revision = pointer.trim_end().parse();
        Ok(Some(revision.map_err(|_| WorkspaceError::Malformed(path))?))
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
    /// changed, and then still has to be saved. If the working copy's
    /// change landed from elsewhere, the working copy follows it first, see
    /// [`Workspace::catch_up`].
    fn snapshot(&mut self) -> Result<WorkingCopy, WorkspaceError> {
        let held = self.store.hold_objects()?;
        let revision: Revision = self.store.get(self.revision)?;
        let path = self.meta(Self::STATE);
        let (tree, cache) =
            Worktree::new(&self.root, &self.store).snapshot(&StatCache::load(&path))?;
        if let Some(cache) = cache {
            cache
                .save(&path)
                .map_err(|source| WorkspaceError::io(&path, source))?;
        }
        let index = Index::open(&self.store)?;
        if self.catch_up(&index, &revision, tree)? {
            return Ok(WorkingCopy {
                id: self.revision,
                revision: self.store.get(self.revision)?,
                unsaved: None,
                index: Index::open(&self.store)?,
                _held: held,
            });
        }
        if tree == revision.snapshot {
            return Ok(WorkingCopy {
                id: self.revision,
                revision,
                unsaved: None,
                index,
                _held: held,
            });
        }
        let conflict = self.conflict(self.revision, &revision)?;
        let revision = Revision {
            predecessors: vec![self.revision],
            snapshot: tree,
            author: self.config.user(),
            ..revision
        };
        let encoded = revision.encode();
        if let Some(conflict) = conflict {
            self.carry(&conflict, &encoded, tree)?;
        }
        Ok(WorkingCopy {
            id: encoded.id().into(),
            revision,
            unsaved: Some(encoded),
            index,
            _held: held,
        })
    }

    /// The unresolved conflict of `revision`, `id`, if it has one.
    fn conflict(
        &self,
        id: RevisionId,
        revision: &Revision,
    ) -> Result<Option<Conflict>, WorkspaceError> {
        let annotations = Annotations::open(&self.store)?;
        let claims = annotations.claims(RecordId::Revision(id))?;
        Ok(Conflict::of(&self.store, &claims, &revision.author)?)
    }

    /// Carries `conflict`, of the revision the working copy was, to the new
    /// one, `encoded`, whose files are `tree`: if markers remain in some of
    /// its files, the new revision is written now, with a `core.conflict`
    /// listing them, so it stays unresolved.
    fn carry(
        &self,
        conflict: &Conflict,
        encoded: &EncodedObject,
        tree: TreeId,
    ) -> Result<(), WorkspaceError> {
        let remaining = conflict.remaining(&self.store, tree)?;
        if remaining.is_empty() {
            return Ok(());
        }
        let subject = RecordId::Revision(encoded.id().into());
        let (payload, statement) =
            Conflict::statement(subject, self.config.user(), &remaining, None);
        self.write(&[vec![payload, encoded.clone()], vec![statement.encode()]])
    }

    /// Snapshots the working copy, saving a new revision if the files
    /// changed.
    fn refresh(&mut self) -> Result<WorkingCopy, WorkspaceError> {
        let mut working_copy = self.snapshot()?;
        if let Some(unsaved) = working_copy.unsaved.take() {
            self.save(&[vec![unsaved]], working_copy.id, Why::Snapshot)?;
            working_copy.index = Index::open(&self.store)?;
        }
        Ok(working_copy)
    }

    /// Logs the snapshot of a command that saves it with its own records,
    /// as a move of its own, so `undo` takes the command back to the edits
    /// it recorded. The working copy points at the command's revision next.
    fn log_snapshot(&mut self, working_copy: &WorkingCopy) -> Result<(), WorkspaceError> {
        if working_copy.unsaved.is_some() {
            self.store.log_unsynced(&Effect::Moved(Move {
                from: Some(self.revision),
                to: working_copy.id,
                why: Why::Snapshot,
            }))?;
            self.revision = working_copy.id;
        }
        Ok(())
    }

    /// Writes `layers` of objects, then makes `revision`, which has the
    /// working copy's files, the working copy, flagging its change if that
    /// made it diverge.
    fn save(
        &mut self,
        layers: &[Vec<EncodedObject>],
        revision: RevisionId,
        why: Why,
    ) -> Result<(), WorkspaceError> {
        self.write(layers)?;
        self.point_at(revision, why)?;
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

    /// Durably records that the working copy, which has the files of
    /// `revision`, is `revision`, and logs the move.
    fn point_at(&mut self, revision: RevisionId, why: Why) -> Result<(), WorkspaceError> {
        self.publish_pointer(Some(self.revision), revision, why)
    }

    /// Durably records that the working copy moved from `from`, or was
    /// created, to `to`, with the op log entry saying so.
    fn publish_pointer(
        &mut self,
        from: Option<RevisionId>,
        to: RevisionId,
        why: Why,
    ) -> Result<(), WorkspaceError> {
        // Not synced on its own: see `oplog`.
        self.store
            .log_unsynced(&Effect::Moved(Move { from, to, why }))?;
        self.write_durably(self.meta(Self::POINTER), format!("{to}\n").as_bytes())?;
        self.revision = to;
        Ok(())
    }

    /// Moves the working copy, whose files are `files`, to `to`, whose files
    /// are `tree`: checks them out, then points at it. A crash in between
    /// leaves `moving` behind, so the next command finishes the move.
    fn switch_to(
        &mut self,
        files: TreeId,
        to: RevisionId,
        tree: TreeId,
        why: Why,
    ) -> Result<(), WorkspaceError> {
        let moving = self.meta(Self::MOVING);
        fs::write(&moving, format!("{to} {why}\n"))
            .map_err(|source| WorkspaceError::io(&moving, source))?;
        self.checkout(files, tree)?;
        self.point_at(to, why)?;
        fs::remove_file(&moving).map_err(|source| WorkspaceError::io(&moving, source))
    }

    /// Durably replaces a metadata file.
    fn write_durably(&self, path: PathBuf, contents: &[u8]) -> Result<(), WorkspaceError> {
        let write = self.store.durable_write();
        write
            .stage(path, contents)
            .and_then(|()| write.commit())
            .map_err(StoreError::from)?;
        Ok(())
    }

    /// Makes the files match `to`, given that they match `from`, cloning
    /// those the other workspaces have.
    fn checkout(&self, from: TreeId, to: TreeId) -> Result<(), WorkspaceError> {
        let changes = self.store.diff(from, to)?;
        let sources: Vec<_> = self
            .workspaces()?
            .into_iter()
            .filter(|root| *root != self.root)
            .map(|root| {
                let cache = StatCache::load(&root.join(Self::META_DIR).join(Self::STATE));
                CloneSource::new(root, cache)
            })
            .collect();
        Ok(Worktree::new(&self.root, &self.store).checkout(&changes, &sources)?)
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
        let index = &working_copy.index;
        println!(
            "Working copy: {}{}",
            self.headline(revision)?,
            self.flags(index, revision.change)?
        );
        println!("On top of: {}", self.describe_base(revision.base)?);
        Self::print_divergence(index, revision.change);
        if let Some(conflict) = self.conflict(working_copy.id, revision)? {
            Self::print_conflicts(&conflict.paths);
        }
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
        let index = &working_copy.index;
        let revisions = match change {
            None => vec![working_copy.id],
            Some(change) => index.heads(working_copy.find_change(change)?),
        };
        for (position, id) in revisions.into_iter().enumerate() {
            if position > 0 {
                println!();
            }
            let revision: Revision = self.store.get(id)?;
            println!(
                "Change {}{}",
                revision.change.short(),
                self.flags(index, revision.change)?
            );
            println!("Revision {} by {}", id.short(), revision.author);
            println!("On top of: {}", self.describe_base(revision.base)?);
            for (issuer, reason) in self.abandonments(index, id)? {
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
        self.log_snapshot(&working_copy)?;
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
        self.save(&[first.collect(), vec![encoded]], id, Why::Command)?;
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
        self.log_snapshot(&working_copy)?;
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
        self.save(&[first.collect(), vec![encoded]], id, Why::Command)?;
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
        let index = &working_copy.index;
        let change = working_copy.find_change(change)?;
        let head = Self::single_head(index, change)?;
        let revision: Revision = self.store.get(head)?;
        if head != working_copy.id {
            self.switch_to(
                working_copy.revision.snapshot,
                head,
                revision.snapshot,
                Why::Command,
            )?;
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
        let index = &working_copy.index;
        let change = match change {
            Some(change) => working_copy.find_change(change)?,
            None => working_copy.revision.change,
        };
        let head = Self::single_head(index, change)?;
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
        self.switch_to(
            working_copy.revision.snapshot,
            encoded.id().into(),
            revision.snapshot,
            Why::Command,
        )?;
        println!("Abandoned change {}", change.short());
        println!("Working copy: {}", self.headline(&revision)?);
        println!("On top of: {}", self.describe_base(revision.base)?);
        Ok(())
    }

    /// `origin converge <change>`: ends a change's divergence with a
    /// revision evolving from all its heads. It merges their files, file by
    /// file, with the changes each made since the latest revision they all
    /// evolve from. It keeps the base and the description of the working
    /// copy, if it is one of the heads, or else of the newest head. Where
    /// heads changed the same text file, it holds each version between
    /// conflict markers, and a `core.conflict` statement lists the files,
    /// so the revision doesn't land until a snapshot without the markers:
    /// see [`conflict`](crate::conflict).
    ///
    /// # Errors
    ///
    /// Fails if the change cannot be found or didn't diverge, or the
    /// revision cannot be written.
    pub fn converge(&mut self, change: &str) -> Result<(), WorkspaceError> {
        let working_copy = self.refresh()?;
        let index = &working_copy.index;
        let change = working_copy.find_change(change)?;
        let heads = index.heads(change);
        if heads.len() < 2 {
            return Err(WorkspaceError::NotDivergent(change));
        }
        let kept = if heads.contains(&working_copy.id) {
            working_copy.id
        } else {
            heads[heads.len() - 1]
        };
        let kept_revision: Revision = self.store.get(kept)?;
        let base = match self.common_predecessor(&heads, kept)? {
            Some(base) => Some(self.store.get(base)?.snapshot),
            None => None,
        };
        let mut snapshot = kept_revision.snapshot;
        let mut conflicts = Vec::new();
        for &head in heads.iter().filter(|&&head| head != kept) {
            let merge = self.store.merge_marked(
                base,
                snapshot,
                self.store.get(head)?.snapshot,
                [&kept.short(), &head.short()],
            )?;
            // The next merge reads this one's trees.
            self.write(&[merge.files.into_iter().chain(merge.trees).collect()])?;
            snapshot = merge.tree;
            conflicts.extend(merge.conflicts);
        }
        conflicts.sort_unstable();
        conflicts.dedup();
        let revision = Revision {
            predecessors: heads.clone(),
            snapshot,
            author: self.config.user(),
            ..kept_revision
        };
        let encoded = revision.encode();
        let id = encoded.id().into();
        self.write(&[vec![encoded]])?;
        if !conflicts.is_empty() {
            let (payload, statement) =
                Conflict::statement(RecordId::Revision(id), self.config.user(), &conflicts, None);
            self.write(&[vec![payload], vec![statement.encode()]])?;
        }
        if change == working_copy.revision.change {
            self.switch_to(
                working_copy.revision.snapshot,
                id,
                revision.snapshot,
                Why::Command,
            )?;
        }
        println!(
            "Converged change {}: revision {} evolves from {}, merging their files",
            change.short(),
            id.short(),
            Self::list(&heads),
        );
        if !conflicts.is_empty() {
            Self::print_conflicts(&conflicts);
        }
        Ok(())
    }

    /// Lists the files a revision holds conflict markers in, and how to
    /// resolve them.
    fn print_conflicts(paths: &[RepoPath]) {
        let paths: Vec<_> = paths.iter().map(ToString::to_string).collect();
        println!(
            "Conflicts: {} hold both versions between conflict markers. Edit the markers \
             out to resolve them.",
            paths.join(", ")
        );
    }

    /// The nearest revision that `kept` and every other head of `heads`
    /// evolve from, if there's one.
    fn common_predecessor(
        &self,
        heads: &[RevisionId],
        kept: RevisionId,
    ) -> Result<Option<RevisionId>, WorkspaceError> {
        let mut others = Vec::new();
        for &head in heads.iter().filter(|&&head| head != kept) {
            others.push(self.predecessors(head)?);
        }
        // Breadth first from the kept head, so the nearest is found first.
        let mut seen = HashSet::new();
        let mut next: VecDeque<RevisionId> = self.store.get(kept)?.predecessors.into();
        while let Some(id) = next.pop_front() {
            if !seen.insert(id) {
                continue;
            }
            if others.iter().all(|predecessors| predecessors.contains(&id)) {
                return Ok(Some(id));
            }
            next.extend(self.store.get(id)?.predecessors);
        }
        Ok(None)
    }

    /// Every revision `revision` evolves from, however indirectly.
    fn predecessors(&self, revision: RevisionId) -> Result<HashSet<RevisionId>, WorkspaceError> {
        let mut found = HashSet::new();
        let mut next = self.store.get(revision)?.predecessors;
        while let Some(id) = next.pop() {
            if found.insert(id) {
                next.extend(self.store.get(id)?.predecessors);
            }
        }
        Ok(found)
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
        let index = &working_copy.index;
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
                    self.flags(index, revision.change)?
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
                            self.flags(index, revision.change)?
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
        let index = &working_copy.index;
        let change = working_copy.find_change(change)?;
        let heads = index.heads(change);
        println!("Change {}{}", change.short(), self.flags(index, change)?);
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

    /// Who abandoned a revision, and why.
    fn abandonments(
        &self,
        index: &Index,
        revision: RevisionId,
    ) -> Result<Vec<(Principal, String)>, WorkspaceError> {
        let mut abandonments = Vec::new();
        for &id in index.statements(RecordId::Revision(revision)) {
            let statement = self.store.get(id)?;
            if statement.kind == Kind::ABANDON {
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
            .target_name(id)?
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
    /// A workspace's creation never finished.
    #[error(
        "the workspace at {} was never finished: remove it with `origin workspace rm`",
        .0.display()
    )]
    Incomplete(PathBuf),
    /// A workspace's repository is gone.
    #[error("this workspace's repository, {}, is gone", .0.display())]
    MissingRepository(PathBuf),
    /// `workspace new` was given a path in a working copy.
    #[error("{} is inside a working copy", .0.display())]
    Nested(PathBuf),
    /// `workspace new` was given a directory with files in it.
    #[error("{} is not empty", .0.display())]
    NotEmpty(PathBuf),
    /// `workspace rm` was given a path that isn't another workspace.
    #[error("{} is not a workspace of this repository", .0.display())]
    UnknownWorkspace(PathBuf),
    /// `workspace rm` was given the main working copy, which holds the
    /// store.
    #[error("{} is the main working copy, which holds the repository", .0.display())]
    RemoveMain(PathBuf),
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
    /// A landing failed.
    #[error(transparent)]
    Land(#[from] LandError),
    /// `undo` found no operation of this workspace to take back.
    #[error("nothing to undo: no operation moved this workspace's working copy")]
    NothingToUndo,
    /// `undo` reached the operation that created the workspace.
    #[error(
        "nothing to undo but the creation of {}: remove it with `origin workspace rm`",
        .0.display()
    )]
    UndoCreation(PathBuf),
}

impl WorkspaceError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}
