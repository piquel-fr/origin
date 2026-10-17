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
//! objects, then the revision, then the `workspace` file naming it. Only
//! the statements a new revision needs, such as its unresolved conflicts,
//! come before it, so it's never found without them.
//!
//! A command that moves the working copy to a new revision, such as a
//! merge, checks its files out before it writes the revision. A file edited
//! in the meantime refuses the checkout, which then takes back the files it
//! changed, leaving those edited as they are: the working copy and its
//! change stay where they were, and the command can run again.
//!
//! The change can move on without the working copy, from another workspace:
//! it can land, or the landing queue can rebase it. The working copy is
//! then behind it, and only `origin pull` follows it there, merging the
//! edits made since (see [`land`]); commands never move the files on their
//! own. Until then, edits stay unrecorded rather than fork the change, the
//! commands that only read the working copy say where the change went, and
//! the others refuse to build on it.
//!
//! Every move of the working copy is logged in the op log, in an entry made
//! durable with the `workspace` file naming the new revision. A move that
//! changes the files first publishes `moving`, durably, naming the files it
//! starts from, those it goes to and their revision. It then checks the
//! files out and makes them durable, writes the revision if it's new,
//! points `workspace` at it, and removes `moving`, durably too. A crash, or
//! a power loss, can leave the files half checked out, with `moving`
//! behind, and the user may edit them before the next command to open the
//! workspace finishes the move. It only touches the files the move changes
//! that are still on one side of it, by content: it takes them to the
//! revision and points `workspace` at it, or back where they were if the
//! revision wasn't written. Every other file, edited or created since,
//! stays as it is, and the command's snapshot records it. So does every
//! file with a `moving` it cannot read, which no move writes, or with one
//! it cannot finish, such as where a file would replace a directory that
//! holds what no snapshot tracks: an error that lasts would fail every
//! command otherwise. Taking back a refused checkout is a move too, back to
//! the working copy, which publishes `moving` again first: a crash then
//! finishes taking the files back, not the move refused.

mod annotate;
mod land;
mod ops;
mod registry;

use std::fmt;
use std::fs;
use std::io;
use std::mem;
use std::path::{self, Path, PathBuf};
use std::sync::Arc;

use origin_platform::{LockedFile, Native};
use origin_shared::api::{RevisionEntry, Revisions};
use origin_shared::{
    AcceptanceId, Base, BlobId, ChangeId, EncodedObject, Kind, Principal, Record, RecordId,
    RepoPath, Revision, RevisionId, Source, Statement, TargetId, TargetName, Tree, TreeId,
};

use crate::ancestry::Ancestry;
use crate::annotations::{AnnotateError, Annotations};
use crate::config::{ConfigError, UserConfig};
use crate::conflict::Conflicts;
use crate::index::{FindChangeError, FindError, Index};
use crate::oplog::{Actor, Effect, Move, Why};
use crate::output::FileDiff;
use crate::policy::{Policy, PolicyError};
use crate::queue::{LandError, Queue};
use crate::service::{Caller, Node, OriginService, ServiceError};
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
    /// The move a snapshot made, saved with the command's own records,
    /// which is logged with the command's own move.
    unlogged: Option<Move>,
}

/// The working copy as of a snapshot.
struct WorkingCopy {
    id: RevisionId,
    revision: Revision,
    /// The revision's record, if the snapshot made a new revision, which is
    /// not written yet.
    unsaved: Option<EncodedObject>,
    /// The revision's unresolved conflicts.
    conflicts: Conflicts,
    /// The record index, as of the snapshot: once saved, it has the
    /// revision.
    index: Index,
    /// The files as the snapshot found them: the revision's, unless the
    /// working copy is behind and they changed.
    files: TreeId,
    /// The stat cache of the snapshot, which proves files unedited since.
    cache: StatCache,
    /// Where the change went, if it moved on without the working copy.
    behind: Option<Behind>,
    /// Keeps garbage collection away from the objects the snapshot found
    /// stored, until the command that saves it is done.
    _held: LockedFile,
}

/// Where the working copy's change went from elsewhere, without the working
/// copy, which `origin pull` follows.
#[derive(Clone, Debug)]
enum Behind {
    /// Its only head landed, at this position of the target.
    Landed { target: TargetName, seq: u64 },
    /// The landing queue rebased the working copy into this revision, which
    /// waits to land.
    Rebased(RevisionId),
}

/// A move of the working copy in progress, as `moving` records it, so the
/// next command can finish it if a crash cuts it short.
#[derive(Debug, PartialEq, Eq)]
struct Moving {
    /// The files the move found.
    from: TreeId,
    /// The files of `revision`.
    to: TreeId,
    /// Where the move goes, which is only written once its files are, if
    /// it's new.
    revision: RevisionId,
    why: Why,
}

impl Moving {
    /// The move that takes this one back, to `revision`, the working copy.
    fn back(&self, revision: RevisionId) -> Self {
        Self {
            from: self.to,
            to: self.from,
            revision,
            why: self.why,
        }
    }

    /// Parses `<from> <to> <revision> <why>`, as written.
    fn parse(bytes: &[u8]) -> Option<Self> {
        let mut fields = str::from_utf8(bytes).ok()?.trim_end().splitn(4, ' ');
        Some(Self {
            from: fields.next()?.parse().ok()?,
            to: fields.next()?.parse().ok()?,
            revision: fields.next()?.parse().ok()?,
            why: Why::parse(fields.next()?)?,
        })
    }
}

impl fmt::Display for Moving {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            from,
            to,
            revision,
            why,
        } = self;
        writeln!(f, "{from} {to} {revision} {why}")
    }
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

    /// The file in [`Workspace::META_DIR`] recording the move of a checkout
    /// in progress: see [`Moving`].
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

        let policy = Policy::initial(config.node(), &key.public());
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
            unlogged: None,
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
            unlogged: None,
        };
        workspace.finish_move()?;
        Ok(workspace)
    }

    /// Finishes the move `moving` records, if a crash cut it short, as the
    /// module explains: takes the files still as the move found them to
    /// where it went, and points the working copy there, if the move's
    /// revision was written, or else takes the files it changed back. Files
    /// on neither side, edited since, stay as they are. So does every file,
    /// with a `moving` it cannot read, or that it fails to finish.
    fn finish_move(&mut self) -> Result<(), WorkspaceError> {
        let path = self.meta(Self::MOVING);
        let moving = match fs::read(&path) {
            Ok(moving) => moving,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(WorkspaceError::io(&path, source)),
        };
        let Some(moving) = Moving::parse(&moving) else {
            self.moved()?;
            println!(
                "Found a move of the working copy a crash cut short: its files stay as they are"
            );
            return Ok(());
        };
        let written = self.store.contains(moving.revision.into())?;
        let (from, to) = if written {
            (moving.from, moving.to)
        } else {
            (moving.to, moving.from)
        };
        if let Err(error) = self.restore(from, to) {
            self.moved()?;
            println!(
                "Cannot finish a move of the working copy a crash cut short, so its files stay as they are: {:#}",
                anyhow::Error::from(error)
            );
            return Ok(());
        }
        if !written {
            self.moved()?;
            println!(
                "Took back a move of the working copy a crash cut short, before its revision was written"
            );
            return Ok(());
        }
        if self.revision != moving.revision {
            self.point_at(moving.revision, moving.why)?;
        }
        self.moved()?;
        let revision = self.store.get(moving.revision)?;
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

    /// The service of this repository, run by this node, which the commands
    /// that the API also offers go through. It finds records through
    /// `index`, if the command just read it, or else reads it once needed.
    fn service(&self, index: Option<Index>) -> Result<OriginService, WorkspaceError> {
        let node = Node {
            name: self.config.node().clone(),
            key: Arc::new(self.config.node_key()?),
        };
        let service = OriginService::new(self.store.clone(), node);
        Ok(match index {
            Some(index) => service.with_index(index),
            None => service,
        })
    }

    /// Snapshots the working copy. The revision is new if the files
    /// changed, and then still has to be saved, unless the working copy is
    /// behind its change: then the edits stay unrecorded.
    fn snapshot(&mut self) -> Result<WorkingCopy, WorkspaceError> {
        let held = self.store.hold_objects()?;
        let revision: Revision = self.store.get(self.revision)?;
        let path = self.meta(Self::STATE);
        let loaded = StatCache::load(&path);
        let (tree, cache) = Worktree::new(&self.root, &self.store).snapshot(&loaded)?;
        let cache = match cache {
            Some(cache) => {
                cache
                    .save(&path)
                    .map_err(|source| WorkspaceError::io(&path, source))?;
                cache
            }
            None => loaded,
        };
        let mut index = Index::open(&self.store)?;
        let behind = self.behind(&index, self.revision, &revision)?;
        let conflicts = self.conflicts(&mut index, self.revision, &revision)?;
        let (id, revision, unsaved, conflicts) = if tree == revision.snapshot || behind.is_some() {
            (self.revision, revision, None, conflicts)
        } else {
            let revision = Revision {
                predecessors: vec![self.revision],
                snapshot: tree,
                author: self.config.user(),
                ..revision
            };
            let encoded = revision.encode();
            let conflicts = conflicts.remaining(&self.store, tree)?;
            if !conflicts.is_empty() {
                // Now, so the revision never exists without them.
                self.write(&self.revision_layers(Vec::new(), &encoded, &conflicts))?;
            }
            (encoded.id().into(), revision, Some(encoded), conflicts)
        };
        Ok(WorkingCopy {
            id,
            revision,
            unsaved,
            conflicts,
            index,
            files: tree,
            cache,
            behind,
            _held: held,
        })
    }

    /// Where the working copy's change went without the working copy, `id`,
    /// if it did: its only head landed, or it's a rebase the landing queue
    /// made of the working copy. A queue's rebases evolve from the working
    /// copy, each from the one before, and nodes make them.
    fn behind(
        &self,
        index: &Index,
        id: RevisionId,
        revision: &Revision,
    ) -> Result<Option<Behind>, WorkspaceError> {
        let [head] = index.heads(revision.change)[..] else {
            return Ok(None);
        };
        let mut newest: Revision = self.store.get(head)?;
        if let Some(landing) = self.store.landing(head, &newest)? {
            let body = landing.body();
            return Ok(Some(Behind::Landed {
                target: self.target_name(body.target)?,
                seq: body.seq,
            }));
        }
        let mut at = head;
        while at != id && matches!(newest.author, Principal::Node(_)) {
            let [predecessor] = newest.predecessors[..] else {
                break;
            };
            at = predecessor;
            newest = self.store.get(at)?;
        }
        Ok((at == id && head != id).then_some(Behind::Rebased(head)))
    }

    /// Where the working copy's change went, in words.
    fn describe_behind(&self, change: ChangeId, behind: &Behind) -> Result<String, WorkspaceError> {
        Ok(match behind {
            Behind::Landed { target, seq } => {
                format!("change {} landed on {target} #{seq}", change.short())
            }
            Behind::Rebased(rebased) => format!(
                "the landing queue rebased change {} onto {}, as revision {}",
                change.short(),
                self.describe_base(self.store.get(*rebased)?.base)?,
                rebased.short()
            ),
        })
    }

    /// Prints where the change went, if the working copy is behind it, and
    /// how to follow it.
    fn print_behind(&self, working_copy: &WorkingCopy) -> Result<(), WorkspaceError> {
        if let Some(behind) = &working_copy.behind {
            let edits = if working_copy.files == working_copy.revision.snapshot {
                ""
            } else {
                ", with the edits made since"
            };
            println!(
                "Behind: {}. Run `origin pull` to follow it{edits}.",
                self.describe_behind(working_copy.revision.change, behind)?
            );
        }
        Ok(())
    }

    /// The unresolved conflicts of `revision`, `id`, found through `index`.
    fn conflicts(
        &self,
        index: &mut Index,
        id: RevisionId,
        revision: &Revision,
    ) -> Result<Conflicts, WorkspaceError> {
        let annotations = Annotations::new(&self.store, mem::take(index));
        let claims = annotations.claims(RecordId::Revision(id));
        *index = annotations.into_index();
        Ok(Conflicts::of(&self.store, &claims?, revision)?)
    }

    /// The unresolved conflicts a new change on `base` starts with, from
    /// the files of the revision it's on, if it's on one.
    fn base_conflicts(&self, index: &mut Index, base: Base) -> Result<Conflicts, WorkspaceError> {
        match base {
            Base::Revision(id) => self.conflicts(index, id, &self.store.get(id)?),
            Base::Acceptance(_) => Ok(Conflicts::default()),
        }
    }

    /// The layers that write `revision`, a new one: `first`, objects it
    /// refers to, then the statements published with it, then the revision
    /// itself. The statements are a `core.conflict` about it listing
    /// `conflicts`, its unresolved ones, if it has any (see
    /// [`conflict`](crate::conflict)), and any a caller adds to the second
    /// layer. Whoever finds the revision finds them: an error or a crash
    /// never publishes it without them, as one batch could, publishing some
    /// of its files.
    fn revision_layers(
        &self,
        mut first: Vec<EncodedObject>,
        revision: &EncodedObject,
        conflicts: &Conflicts,
    ) -> Vec<Vec<EncodedObject>> {
        let mut statements = Vec::new();
        if !conflicts.is_empty() {
            let subject = RecordId::Revision(revision.id().into());
            let (payload, statement) = conflicts.statement(subject, self.config.user(), None);
            first.push(payload);
            statements.push(statement.encode());
        }
        vec![first, statements, vec![revision.clone()]]
    }

    /// Snapshots the working copy, saving a new revision if the files
    /// changed and it isn't behind its change, for the commands that only
    /// read it.
    fn inspect(&mut self) -> Result<WorkingCopy, WorkspaceError> {
        let mut working_copy = self.snapshot()?;
        if let Some(unsaved) = working_copy.unsaved.take() {
            self.save(
                &[vec![unsaved]],
                working_copy.id,
                Why::Snapshot,
                &mut working_copy.index,
            )?;
        }
        Ok(working_copy)
    }

    /// Snapshots the working copy like [`Workspace::inspect`], for the
    /// commands that build on it: they refuse a working copy behind its
    /// change.
    fn refresh(&mut self) -> Result<WorkingCopy, WorkspaceError> {
        let working_copy = self.inspect()?;
        self.following(working_copy)
    }

    /// The working copy, unless it's behind its change.
    fn following(&self, working_copy: WorkingCopy) -> Result<WorkingCopy, WorkspaceError> {
        match &working_copy.behind {
            None => Ok(working_copy),
            Some(behind) => Err(WorkspaceError::Behind(
                self.describe_behind(working_copy.revision.change, behind)?,
            )),
        }
    }

    /// Records the snapshot of a command that saves it with its own
    /// records as a move of its own, so `undo` takes the command back to the
    /// edits it recorded. It's logged with the command's move, which starts
    /// from it.
    fn log_snapshot(&mut self, working_copy: &WorkingCopy) {
        if working_copy.unsaved.is_some() {
            self.unlogged = Some(Move {
                from: Some(self.revision),
                to: working_copy.id,
                why: Why::Snapshot,
            });
            self.revision = working_copy.id;
        }
    }

    /// Writes `layers` of objects, adding them to `index`, then makes
    /// `revision`, which has the working copy's files, the working copy,
    /// flagging its change if that made it diverge.
    fn save(
        &mut self,
        layers: &[Vec<EncodedObject>],
        revision: RevisionId,
        why: Why,
        index: &mut Index,
    ) -> Result<(), WorkspaceError> {
        self.write(layers)?;
        index.add(layers.iter().flatten())?;
        self.point_at(revision, why)?;
        self.flag_divergence(index, revision)
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
    /// created, to `to`, with the op log entry saying so, after the move of
    /// a snapshot the command saved: the entries are durable with
    /// `workspace`.
    fn publish_pointer(
        &mut self,
        from: Option<RevisionId>,
        to: RevisionId,
        why: Why,
    ) -> Result<(), WorkspaceError> {
        let mut moves: Vec<_> = self.unlogged.take().into_iter().collect();
        moves.push(Move { from, to, why });
        let write = self.store.durable_write();
        write
            .stage(self.meta(Self::POINTER), format!("{to}\n").as_bytes())
            .map_err(StoreError::from)?;
        let moves: Vec<_> = moves.into_iter().map(Effect::Moved).collect();
        self.store.commit_logged(write, &moves)?;
        self.revision = to;
        Ok(())
    }

    /// Moves the working copy, whose files `working_copy` found, to
    /// `revision`, `to`, as the module explains: checks it out over them,
    /// then writes `unwritten`, the layers that write it if it's new, and
    /// makes it the working copy. A crash before it's done leaves `moving`
    /// behind, so the next command finishes the move, or takes it back if
    /// the revision wasn't written. A checkout refused by a file edited
    /// since the snapshot writes nothing, and takes back the files it
    /// changed, as a move of its own, so the working copy and its change
    /// stay where they were.
    fn switch_to(
        &mut self,
        working_copy: &WorkingCopy,
        to: RevisionId,
        revision: &Revision,
        unwritten: &[Vec<EncodedObject>],
        why: Why,
    ) -> Result<(), WorkspaceError> {
        // The files stay as they are: there's nothing to finish.
        if revision.snapshot == working_copy.files {
            self.write(unwritten)?;
            return self.point_at(to, why);
        }
        let moving = Moving {
            from: working_copy.files,
            to: revision.snapshot,
            revision: to,
            why,
        };
        self.publish_moving(&moving)?;
        let checkout = self.checkout(
            working_copy.files,
            revision.snapshot,
            Some(&working_copy.cache),
        );
        if let Err(WorkspaceError::Snapshot(SnapshotError::Edited(_))) = checkout {
            // So a crash finishes taking the files back, not the move, whose
            // revision may exist.
            let back = moving.back(working_copy.id);
            self.publish_moving(&back)?;
            self.restore(back.from, back.to)?;
            self.moved()?;
        }
        checkout?;
        self.write(unwritten)?;
        self.point_at(to, why)?;
        self.moved()
    }

    /// Publishes `moving`, durably, before its move changes any file.
    fn publish_moving(&self, moving: &Moving) -> Result<(), WorkspaceError> {
        self.write_durably(self.meta(Self::MOVING), moving.to_string().as_bytes())
    }

    /// Removes `moving`, durably, once its move is done.
    fn moved(&self) -> Result<(), WorkspaceError> {
        let path = self.meta(Self::MOVING);
        fs::remove_file(&path).map_err(|source| WorkspaceError::io(&path, source))?;
        Ok(self.store.sync_dir(&self.root.join(Self::META_DIR))?)
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

    /// Makes the files still as `from` has them match `to`, durably, and
    /// leaves those edited since as they are.
    fn restore(&self, from: TreeId, to: TreeId) -> Result<(), WorkspaceError> {
        let changes = self.store.diff(from, to)?;
        Ok(Worktree::new(&self.root, &self.store).restore(&changes)?)
    }

    /// Makes the files match `to`, given that they match `from`, durably,
    /// cloning those the other workspaces have. With `unchanged`, the stat
    /// cache of the snapshot that found `from`, it changes nothing if a
    /// file it changes was edited since.
    fn checkout(
        &self,
        from: TreeId,
        to: TreeId,
        unchanged: Option<&StatCache>,
    ) -> Result<(), WorkspaceError> {
        let changes = self.store.diff(from, to)?;
        if changes.is_empty() {
            return Ok(());
        }
        let sources: Vec<_> = self
            .workspaces()?
            .into_iter()
            .filter(|root| *root != self.root)
            .map(|root| {
                let cache = root.join(Self::META_DIR).join(Self::STATE);
                CloneSource::saved(root, cache)
            })
            .collect();
        Ok(Worktree::new(&self.root, &self.store).checkout(&changes, &sources, unchanged)?)
    }

    /// `origin status`: prints the working copy's change, what it is on
    /// top of, whether it diverged, and the files it changes.
    ///
    /// # Errors
    ///
    /// Fails if the working copy cannot be snapshotted, or the records it
    /// refers to cannot be read.
    pub fn status(&mut self) -> Result<(), WorkspaceError> {
        let working_copy = self.inspect()?;
        let revision = &working_copy.revision;
        let index = &working_copy.index;
        println!(
            "Working copy: {}{}",
            self.headline(revision)?,
            self.flags(index, revision.change)?
        );
        println!("On top of: {}", self.describe_base(revision.base)?);
        self.print_behind(&working_copy)?;
        Self::print_divergence(index, revision.change);
        if !working_copy.conflicts.is_empty() {
            Self::print_conflicts(&working_copy.conflicts);
        }
        let changes = self
            .store
            .diff(self.base_tree(revision.base)?, working_copy.files)?;
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
        let working_copy = self.inspect()?;
        self.print_diff(working_copy.revision.base, working_copy.files)
    }

    /// `origin show [<change>]`: prints a change's head, or each of them if
    /// it diverged, and its diff. Shows the working copy by default.
    ///
    /// # Errors
    ///
    /// Fails if the change cannot be found, or like [`Workspace::diff`].
    pub fn show(&mut self, change: Option<&str>) -> Result<(), WorkspaceError> {
        let working_copy = self.inspect()?;
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
            self.print_diff(revision.base, revision.snapshot)?;
        }
        Ok(())
    }

    /// Prints the changes `snapshot` makes to `base`, line by line.
    fn print_diff(&self, base: Base, snapshot: TreeId) -> Result<(), WorkspaceError> {
        let base = self.base_tree(base)?;
        for change in self.store.diff(base, snapshot)? {
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
        let mut working_copy = self.following(working_copy)?;
        self.log_snapshot(&working_copy);
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
        let first = working_copy.unsaved.take().into_iter().chain([description]);
        // It starts with the files of the one below, and their conflicts.
        let layers = self.revision_layers(first.collect(), &encoded, &working_copy.conflicts);
        self.save(&layers, id, Why::Command, &mut working_copy.index)?;
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
        let mut working_copy = self.following(working_copy)?;
        self.log_snapshot(&working_copy);
        let description = EncodedObject::blob(description.as_bytes());
        let revision = Revision {
            predecessors: vec![working_copy.id],
            description: description.id().into(),
            author: self.config.user(),
            ..working_copy.revision.clone()
        };
        let encoded = revision.encode();
        let id = encoded.id().into();
        let first = working_copy.unsaved.take().into_iter().chain([description]);
        let layers = self.revision_layers(first.collect(), &encoded, &working_copy.conflicts);
        self.save(&layers, id, Why::Command, &mut working_copy.index)?;
        println!("Working copy: {}", self.headline(&revision)?);
        Ok(())
    }

    /// `origin resolve <path>...`: accepts the files at `paths` as they
    /// are, resolving their conflicts, with a new revision of the change
    /// that keeps the others. Paths without one are skipped.
    ///
    /// # Errors
    ///
    /// Fails if the working copy cannot be snapshotted or the revision
    /// cannot be written.
    pub fn resolve(&mut self, paths: &[RepoPath]) -> Result<(), WorkspaceError> {
        let mut working_copy = self.refresh()?;
        let resolved = working_copy.conflicts.resolve(paths);
        if resolved.is_empty() {
            println!("No conflict to resolve there.");
            return Ok(());
        }
        let revision = Revision {
            predecessors: vec![working_copy.id],
            author: self.config.user(),
            ..working_copy.revision
        };
        let encoded = revision.encode();
        let id = encoded.id().into();
        let layers = self.revision_layers(Vec::new(), &encoded, &working_copy.conflicts);
        self.save(&layers, id, Why::Command, &mut working_copy.index)?;
        println!("Resolved {}", Self::list_paths(&resolved));
        if !working_copy.conflicts.is_empty() {
            Self::print_conflicts(&working_copy.conflicts);
        }
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
            self.switch_to(&working_copy, head, &revision, &[], Why::Command)?;
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
        let mut working_copy = self.refresh()?;
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
        let conflicts = self.base_conflicts(&mut working_copy.index, revision.base)?;
        let mut layers = self.revision_layers(vec![reason, empty], &encoded, &conflicts);
        // Published before the revision that replaces the change.
        layers[1].push(statement);
        let id = encoded.id().into();
        self.switch_to(&working_copy, id, &revision, &layers, Why::Command)?;
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
        let mut working_copy = self.refresh()?;
        let change = working_copy.find_change(change)?;
        let heads = working_copy.index.heads(change);
        if heads.len() < 2 {
            return Err(WorkspaceError::NotDivergent(change));
        }
        let kept = if heads.contains(&working_copy.id) {
            working_copy.id
        } else {
            heads[heads.len() - 1]
        };
        let kept_revision: Revision = self.store.get(kept)?;
        let base = self.merge_base(&heads)?;
        let mut snapshot = kept_revision.snapshot;
        let mut merged = Conflicts::default();
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
            merged = merged.with(merge.conflicts);
        }
        // The heads' own, where the merge kept their side.
        let mut conflicts = Conflicts::default();
        for &head in &heads {
            let revision = self.store.get(head)?;
            let unresolved = self.conflicts(&mut working_copy.index, head, &revision)?;
            conflicts = conflicts.with(unresolved.remaining(&self.store, snapshot)?);
        }
        let conflicts = conflicts.with(merged);
        let revision = Revision {
            predecessors: heads.clone(),
            snapshot,
            author: self.config.user(),
            ..kept_revision
        };
        let encoded = revision.encode();
        let id = encoded.id().into();
        let layers = self.revision_layers(Vec::new(), &encoded, &conflicts);
        if change == working_copy.revision.change {
            self.switch_to(&working_copy, id, &revision, &layers, Why::Command)?;
        } else {
            self.write(&layers)?;
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

    /// Lists a revision's unresolved conflicts, and how to resolve them.
    fn print_conflicts(conflicts: &Conflicts) {
        println!(
            "Conflicts: {} hold both versions between conflict markers, or one side for a \
             binary file. Edit them, or run `origin resolve <path>...` to accept them as they \
             are.",
            Self::list_paths(&conflicts.paths())
        );
    }

    /// `paths`, separated by commas.
    fn list_paths(paths: &[RepoPath]) -> String {
        let paths: Vec<_> = paths.iter().map(ToString::to_string).collect();
        paths.join(", ")
    }

    /// The snapshot to merge `heads`, revisions of one change, from: that of
    /// the newest revision they all evolve from, or the
    /// [merge](Ancestry::merge_base) of several, if each lacks another.
    fn merge_base(&self, heads: &[RevisionId]) -> Result<Option<TreeId>, WorkspaceError> {
        let ancestry = Ancestry::walk(heads, |id| {
            let revision: Revision = self.store.get(id)?;
            Ok::<_, StoreError>((revision.predecessors, revision.snapshot))
        })?;
        Ok(ancestry.merge_base(&self.store, heads)?)
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
        let working_copy = self.inspect()?;
        let index = &working_copy.index;
        let mut next = working_copy.revision.base;
        let mut stack = vec![(working_copy.id, working_copy.revision.clone())];
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
        // Only the last `limit` acceptances are read.
        let since = match (limit, self.store.head(&target)?) {
            (Some(limit), Some(head)) => (head.seq + 1).saturating_sub(limit as u64),
            _ => 0,
        };
        let log = self.service(None)?.log(&Caller::Owner, &target, since)?;
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
        for entry in log.iter().rev() {
            let body = entry.acceptance.body();
            println!("◆ {target} #{} {}", body.seq, entry.id.short());
            for source in &body.landed {
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
        self.print_behind(&working_copy)
    }

    /// `origin evolog <change>`: prints every revision of a change, newest
    /// first, marking its heads.
    ///
    /// # Errors
    ///
    /// Fails if the change cannot be found, or a record cannot be read.
    pub fn evolog(&mut self, change: &str) -> Result<(), WorkspaceError> {
        let mut working_copy = self.inspect()?;
        let change = working_copy.find_change(change)?;
        let flags = self.flags(&working_copy.index, change)?;
        let Revisions { revisions, heads } = self
            .service(Some(mem::take(&mut working_copy.index)))?
            .revisions(&Caller::Owner, change)?;
        println!("Change {}{flags}", change.short());
        for RevisionEntry { id, revision } in revisions.iter().rev() {
            let id = *id;
            let marker = if id == working_copy.id { '@' } else { '○' };
            let head = if heads.contains(&id) { " (head)" } else { "" };
            println!("{marker} {} {}{head}", id.short(), self.summary(revision)?);
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
    /// The working copy's change moved on without it, and the command would
    /// build on it.
    #[error("{0}: run `origin pull` to follow it first")]
    Behind(String),
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
    /// The repository's service refused a call.
    #[error(transparent)]
    Service(#[from] ServiceError),
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
