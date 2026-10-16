//! The op log: what this node applied, in order. It is local, never synced
//! with other nodes, and shared by every workspace of the repository, so
//! each entry says which workspace, and which command, it came from. Its
//! entries record:
//!
//! - records written: revisions, statements and acceptances, with what the
//!   [index](crate::index) keeps of them, so also every landing on a target;
//! - landings left waiting in a queue;
//! - working copies moved, by a snapshot or a command.
//!
//! Each entry has a sequence number, one more than the entry before it,
//! which never changes: it is the cursor of the [event stream](crate::events).
//! The entries one command run appends form an operation, numbered by its
//! first entry's sequence number, which `origin op log` shows and `origin
//! undo` takes back.
//!
//! ```text
//! .origin/oplog/
//!   lock                       held exclusively while appending
//!   writers                    held shared by every write in progress
//!   <first sequence number>    segments of entries, 16 hex digits each
//! ```
//!
//! A segment is a file of [frames](crate::frames), an entry each. Entries are
//! appended to the last segment until it's full, then to a new one, and the
//! oldest segments are deleted beyond a cap: the log keeps the latest 16 MiB
//! of entries, and a cursor older than that has expired.
//!
//! **Writing.** Events and the index come from the entries of records
//! written and landings left waiting, so those must be durable whenever
//! their write is: each is appended just before its write is committed,
//! and synced with it ([`DurableWrite::sync_with`]), at no extra sync. The
//! writer holds `writers` shared from appending the entry until its write
//! is published; a landing holds it until the target's head names the
//! acceptance.
//!
//! Syncing one more file still costs about a third of a small batch on a
//! journaling file system. A batch of records syncs the op log in place of
//! the index, which catches up from it, so that costs nothing; a waiting
//! landing pays it. Moves of a working copy, which only `op log` and `undo`
//! read, aren't synced on their own: they are appended before the move is
//! published, and become durable with the next entry that is. A process
//! crash loses none, since the kernel still writes them; a power loss can
//! lose the last ones, which only shortens what `undo` can take back.
//!
//! **Crashes.** An entry can therefore be in the log before its write is
//! published, and a crash can leave it that way:
//!
//! - A crash in the middle of an append leaves part of a frame at the end of
//!   the last segment. It never was an entry: readers skip it, and the next
//!   append cuts it off.
//! - A crash after appending an entry, before its write was published,
//!   leaves an entry for something that didn't happen, or only partly: some
//!   of a batch's records were published, or none; an acceptance was
//!   written but the target's head didn't move to it (the next landing on
//!   the target publishes it, and logs the acceptance again); a waiting
//!   landing wasn't kept; a working copy didn't move (see
//!   [`workspace`](crate::workspace) for how a move cut short is finished).
//!   Readers tell such an entry from one whose write is in progress by
//!   taking `writers` exclusively, which no writer then holds: an entry's
//!   effects are what they find published.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::{fmt, iter};

use origin_platform::{DurableWrite, LockMode, LockedFile, Platform};
use origin_shared::{ChangeId, ObjectId, RevisionId, TargetName};

use crate::frames::{Frames, Reader, Writer};
use crate::index;
use crate::queue::Proposal;
use crate::store::{Store, StoreError};

/// Who a store writes for, as the op log records it: a command, and the
/// workspace running it, if any.
#[derive(Clone, Debug, Default)]
pub struct Actor {
    workspace: Option<PathBuf>,
    command: String,
    /// The operation: the sequence number of the first entry appended for
    /// it, once there is one. Clones share it.
    op: Arc<OnceLock<u64>>,
}

impl Actor {
    /// A new operation running `command`, such as `land 3fa2`, outside any
    /// workspace.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            ..Self::default()
        }
    }

    /// The same operation, in the workspace at `root`.
    #[must_use]
    pub fn in_workspace(&self, root: &Path) -> Self {
        Self {
            workspace: Some(root.to_owned()),
            ..self.clone()
        }
    }

    /// The workspace, if any.
    #[must_use]
    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }
}

/// An entry of the op log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its sequence number: the one before it, plus one.
    pub seq: u64,
    /// The operation it belongs to: its first entry's sequence number.
    pub op: u64,
    /// The root of the workspace it came from, if any.
    pub workspace: Option<PathBuf>,
    /// The command that appended it, such as `land 3fa2`.
    pub command: String,
    /// What happened.
    pub effect: Effect,
}

/// What an entry records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Records were written: revisions, statements and acceptances, with
    /// what the index keeps of them.
    Wrote(Vec<index::Entry>),
    /// A landing waits in a queue, for evidence or a conflict fix.
    Waiting {
        /// The target it lands on.
        onto: TargetName,
        /// What lands.
        proposal: Proposal,
    },
    /// The entry's workspace's working copy moved.
    Moved(Move),
}

/// A working copy moving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    /// The revision it was, or `None` for a new workspace.
    pub from: Option<RevisionId>,
    /// The revision it became.
    pub to: RevisionId,
    /// What moved it.
    pub why: Why,
}

/// What moved a working copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// A snapshot recorded the files as a new revision.
    Snapshot,
    /// A command, such as `new` or `edit`.
    Command,
    /// `origin undo`, taking back this operation.
    Undo(u64),
}

impl Why {
    /// Parses what [`Why`]'s `Display` writes.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "snapshot" => Self::Snapshot,
            "command" => Self::Command,
            _ => Self::Undo(text.strip_prefix("undo ")?.parse().ok()?),
        })
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot => f.write_str("snapshot"),
            Self::Command => f.write_str("command"),
            Self::Undo(op) => write!(f, "undo {op}"),
        }
    }
}

impl Entry {
    const WROTE: u8 = 1;
    const WAITING: u8 = 2;
    const MOVED: u8 = 3;

    /// Encodes the entry's frame: its sequence number, operation, workspace
    /// and command, then a tag and the effect's fields.
    fn encode(&self) -> Vec<u8> {
        Frames::frame(|out| {
            out.u64(self.seq);
            out.u64(self.op);
            match &self.workspace {
                None => out.u8(0),
                Some(root) => {
                    out.u8(1);
                    out.sized(root.as_os_str().as_bytes());
                }
            }
            out.sized(self.command.as_bytes());
            self.effect.write(out);
        })
    }

    fn decode(payload: &[u8]) -> Option<Self> {
        let mut reader = Reader(payload);
        let seq = reader.u64()?;
        let op = reader.u64()?;
        let workspace = match reader.u8()? {
            0 => None,
            1 => Some(PathBuf::from(OsStr::from_bytes(reader.sized()?))),
            _ => return None,
        };
        let command = String::from_utf8(reader.sized()?.to_vec()).ok()?;
        let effect = Effect::read(&mut reader)?;
        reader.is_empty().then_some(Self {
            seq,
            op,
            workspace,
            command,
            effect,
        })
    }
}

impl Effect {
    fn write(&self, out: &mut Writer) {
        match self {
            Self::Wrote(records) => {
                out.u8(Entry::WROTE);
                out.len(records.len());
                for record in records {
                    record.write(out);
                }
            }
            Self::Waiting { onto, proposal } => {
                out.u8(Entry::WAITING);
                out.sized(onto.as_str().as_bytes());
                match proposal {
                    Proposal::Change(change) => {
                        out.u8(0);
                        out.bytes(change.as_bytes());
                    }
                    Proposal::Target(target) => {
                        out.u8(1);
                        out.sized(target.as_str().as_bytes());
                    }
                }
            }
            Self::Moved(moved) => {
                out.u8(Entry::MOVED);
                out.option(moved.from.as_ref().map(RevisionId::as_bytes));
                out.bytes(moved.to.as_bytes());
                match moved.why {
                    Why::Snapshot => out.u8(0),
                    Why::Command => out.u8(1),
                    Why::Undo(op) => {
                        out.u8(2);
                        out.u64(op);
                    }
                }
            }
        }
    }

    fn read(reader: &mut Reader<'_>) -> Option<Self> {
        let target = |reader: &mut Reader<'_>| {
            TargetName::new(str::from_utf8(reader.sized()?).ok()?.to_owned()).ok()
        };
        let revision = |bytes| RevisionId::from(ObjectId::from_bytes(bytes));
        Some(match reader.u8()? {
            Entry::WROTE => {
                let count = reader.len()?;
                let records = iter::repeat_with(|| index::Entry::read(reader))
                    .take(count)
                    .collect::<Option<_>>()?;
                Self::Wrote(records)
            }
            Entry::WAITING => Self::Waiting {
                onto: target(reader)?,
                proposal: match reader.u8()? {
                    0 => Proposal::Change(ChangeId::from_bytes(reader.array()?)),
                    1 => Proposal::Target(target(reader)?),
                    _ => return None,
                },
            },
            Entry::MOVED => Self::Moved(Move {
                from: reader.option()?.map(revision),
                to: revision(reader.array()?),
                why: match reader.u8()? {
                    0 => Why::Snapshot,
                    1 => Why::Command,
                    2 => Why::Undo(reader.u64()?),
                    _ => return None,
                },
            }),
            _ => return None,
        })
    }
}

/// The op log of a store, for reading.
#[derive(Debug)]
pub struct OpLog<'a, P> {
    store: &'a Store<P>,
}

impl<'a, P: Platform> OpLog<'a, P> {
    /// The format of a segment, and its version.
    const FRAMES: Frames = Frames::new(b"origin oplog 1\n");

    /// A segment this large is full: the next entry starts a new one.
    const SEGMENT_BYTES: u64 = if cfg!(test) { 1024 } else { 256 << 10 };

    /// The most segments kept, 16 MiB of entries, about 100,000.
    const SEGMENTS: usize = if cfg!(test) { 4 } else { 64 };

    /// The smallest frame an entry makes: sequence number, operation, no
    /// workspace, an empty command, and a tag with an empty list.
    const SMALLEST_FRAME: u64 = (Frames::FRAMING + 8 + 8 + 1 + 4 + 1 + 4) as u64;

    /// The lock held while appending.
    const APPENDING: &str = "lock";

    /// The lock every write holds, shared, while it is in progress.
    const WRITERS: &str = "writers";

    /// The op log of `store`.
    pub fn new(store: &'a Store<P>) -> Self {
        Self { store }
    }

    fn dir(&self) -> PathBuf {
        self.store.root().join("oplog")
    }

    fn segment(&self, first: u64) -> PathBuf {
        self.dir().join(format!("{first:016x}"))
    }

    /// Creates the log's directory and first segment, durably, so the first
    /// entry costs no more than the next. Repositories made before the op
    /// log existed get them with their first entry.
    pub(crate) fn init(&self) -> Result<(), StoreError> {
        let dir = self.dir();
        fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
        self.start_segment(1, &[])?;
        Ok(())
    }

    /// Opens and locks one of the log's lock files, creating the log's
    /// directory, durably, if it's missing.
    fn lock(&self, name: &str, mode: LockMode) -> Result<LockedFile, StoreError> {
        let dir = self.dir();
        if !dir.is_dir() {
            fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
            self.store.sync_dir(self.store.root())?;
        }
        Ok(LockedFile::open(
            &self.store.platform,
            &dir.join(name),
            mode,
        )?)
    }

    /// The first sequence number of each segment, oldest first.
    fn segments(&self) -> Result<Vec<u64>, StoreError> {
        let dir = self.dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(StoreError::io(&dir, source)),
        };
        let mut segments = Vec::new();
        for entry in entries {
            let name = entry
                .map_err(|source| StoreError::io(&dir, source))?
                .file_name();
            // Anything else, such as the locks, isn't a segment.
            segments.extend(
                name.to_str()
                    .filter(|name| name.len() == 16)
                    .and_then(|name| u64::from_str_radix(name, 16).ok()),
            );
        }
        segments.sort_unstable();
        Ok(segments)
    }

    /// The entries of the segment starting at `first`, up to a partial or
    /// damaged frame, or `None` if retention deleted it. Those after `after`
    /// at least: when they're at its end, which is usual, only they are
    /// read, backwards from the end.
    fn read_segment(&self, first: u64, after: u64) -> Result<Option<Vec<Entry>>, StoreError> {
        let path = self.segment(first);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::io(&path, source)),
        };
        if let Some(entries) =
            Self::FRAMES.parse_back(&bytes, Entry::decode, |entry| entry.seq > after)
        {
            return Ok(Some(entries));
        }
        let (entries, _) = Self::FRAMES.parse(&bytes, Entry::decode);
        Ok(Some(entries.into_iter().map(|(_, entry)| entry).collect()))
    }

    /// The sequence number of the latest entry, or 0 if there's none.
    ///
    /// # Errors
    ///
    /// Fails if the log cannot be read.
    pub fn last_seq(&self) -> Result<u64, StoreError> {
        let Some(&first) = self.segments()?.last() else {
            return Ok(0);
        };
        let path = self.segment(first);
        let peeked = match File::open(&path).and_then(|file| Self::FRAMES.peek_last(&file)) {
            Ok(peeked) => peeked,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(StoreError::io(&path, source)),
        };
        let last = match peeked {
            Some(Some(payload)) => Reader(&payload).u64(),
            Some(None) => Some(first - 1),
            // A partial or damaged end: read up to it.
            None => None,
        };
        if let Some(last) = last {
            return Ok(last);
        }
        Ok(self
            .read_segment(first, 0)?
            .and_then(|entries| entries.last().map(|entry| entry.seq))
            .unwrap_or(first - 1))
    }

    /// The entries after the sequence number `after`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Expired`] if the log no longer keeps the
    /// entry after `after`, or if it cannot be read.
    pub fn after(&self, after: u64) -> Result<Vec<Entry>, StoreError> {
        // The common case of a poll: nothing new, known from the last frame.
        if self.last_seq()? <= after {
            return Ok(Vec::new());
        }
        let segments = self.segments()?;
        let expired = |oldest| StoreError::Expired { after, oldest };
        if let Some(&oldest) = segments.first()
            && oldest > after + 1
        {
            return Err(expired(oldest));
        }
        // The segment holding the entry after `after`, and those after it.
        let start = segments.partition_point(|&first| first <= after + 1);
        let mut entries = Vec::new();
        for &first in &segments[start.saturating_sub(1)..] {
            let segment = self
                .read_segment(first, after)?
                .ok_or_else(|| expired(first))?;
            entries.extend(segment.into_iter().filter(|entry| entry.seq > after));
        }
        Ok(entries)
    }

    /// The sequence number of the latest entry, once no write is in
    /// progress: the writes of every entry up to it are as published as
    /// they will ever be.
    ///
    /// # Errors
    ///
    /// Fails if the log cannot be locked or read.
    pub(crate) fn settled(&self) -> Result<u64, StoreError> {
        let _settled = self.lock(Self::WRITERS, LockMode::Exclusive)?;
        self.last_seq()
    }

    /// Every entry the log keeps, newest first, read a segment at a time.
    ///
    /// # Errors
    ///
    /// Fails if the segments cannot be listed, or, as an item, if one cannot
    /// be read.
    pub fn newest_first(
        &self,
    ) -> Result<impl Iterator<Item = Result<Entry, StoreError>> + '_, StoreError> {
        let segments = self.segments()?;
        Ok(segments
            .into_iter()
            .rev()
            .flat_map(|first| match self.read_segment(first, 0) {
                Ok(entries) => entries
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                    .map(Ok)
                    .collect(),
                Err(error) => vec![Err(error)],
            }))
    }

    /// Whether no write is in progress: then every entry already appended
    /// is as published as it will ever be.
    ///
    /// # Errors
    ///
    /// Fails if the lock cannot be taken.
    pub fn idle(&self) -> Result<bool, StoreError> {
        let dir = self.dir();
        if !dir.is_dir() {
            return Ok(true);
        }
        let path = dir.join(Self::WRITERS);
        Ok(LockedFile::try_open(&self.store.platform, &path, LockMode::Exclusive)?.is_some())
    }

    /// Appends an entry for `effect`, as the store's actor, without syncing
    /// it, returning its sequence number and segment.
    fn append(&self, effect: &Effect) -> Result<(u64, PathBuf), StoreError> {
        let _lock = self.lock(Self::APPENDING, LockMode::Exclusive)?;
        let segments = self.segments()?;
        let (seq, open) = match segments.last() {
            None => (1, None),
            Some(&first) => self.tail(first)?,
        };
        let segment = match open {
            Some(segment) => segment,
            None => self.start_segment(seq, &segments)?,
        };
        let actor = &self.store.actor;
        let entry = Entry {
            seq,
            op: actor.op.get().copied().unwrap_or(seq),
            workspace: actor.workspace.clone(),
            command: actor.command.clone(),
            effect: effect.clone(),
        };
        Self::FRAMES
            .append(&self.store.platform, &segment.file, &entry.encode())
            .map_err(|source| StoreError::io(&segment.path, source))?;
        actor.op.get_or_init(|| seq);
        Ok((seq, segment.path))
    }

    /// The next sequence number after the last segment, which starts at
    /// `first`, and that segment, opened for appending, unless it's full or
    /// damaged, and the next entry starts a new one.
    fn tail(&self, first: u64) -> Result<(u64, Option<Segment>), StoreError> {
        let path = self.segment(first);
        let io_error = |source| StoreError::io(&path, source);
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;
        let last = match Self::FRAMES.last(&self.store.platform, &file) {
            Ok(Some(payload)) => Reader(&payload)
                .u64()
                .ok_or_else(|| StoreError::Malformed(path.clone()))?,
            Ok(None) => first - 1,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                // Damage hides the entries after it. Skip as many sequence
                // numbers as the damaged bytes could hold, so none is
                // reused, and continue in a new segment.
                let bytes = fs::read(&path).map_err(io_error)?;
                let (entries, _) = Self::FRAMES.parse(&bytes, Entry::decode);
                let (end, last) = entries.last().map_or(
                    (Self::FRAMES.header().len() as u64, first - 1),
                    |(offset, entry)| (offset + entry.encode().len() as u64, entry.seq),
                );
                let hidden = (bytes.len() as u64 - end) / Self::SMALLEST_FRAME;
                return Ok((last + hidden + 1, None));
            }
            Err(source) => return Err(io_error(source)),
        };
        let full = file.metadata().map_err(io_error)?.len() >= Self::SEGMENT_BYTES;
        Ok((last + 1, (!full).then_some(Segment { path, file })))
    }

    /// Creates the segment starting at `first`, holding only its header,
    /// durably, and deletes the oldest of `segments` beyond the cap.
    fn start_segment(&self, first: u64, segments: &[u64]) -> Result<Segment, StoreError> {
        let path = self.segment(first);
        let io_error = |source| StoreError::io(&path, source);
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&path)
            .map_err(io_error)?;
        (&file)
            .write_all(Self::FRAMES.header())
            .and_then(|()| self.store.platform.sync_file(&file))
            .map_err(io_error)?;
        // Its entries are synced with their writes; its name, and its
        // header, now, so the first entry's sync costs no more than the
        // next's.
        self.store.sync_dir(&self.dir())?;
        let excess = (segments.len() + 1).saturating_sub(Self::SEGMENTS);
        for &old in &segments[..excess] {
            // Best effort: a segment left behind is deleted with the next.
            let _ = fs::remove_file(self.segment(old));
        }
        Ok(Segment { path, file })
    }
}

/// A segment open for appending.
struct Segment {
    path: PathBuf,
    file: File,
}

impl<P: Platform> Store<P> {
    /// The op log of the store.
    #[must_use]
    pub fn oplog(&self) -> OpLog<'_, P> {
        OpLog::new(self)
    }

    /// Appends an entry for `effect` without syncing it: it becomes durable
    /// with the next entry that is, as the module explains.
    ///
    /// # Errors
    ///
    /// Fails if the entry cannot be appended.
    pub(crate) fn log_unsynced(&self, effect: &Effect) -> Result<u64, StoreError> {
        Ok(self.oplog().append(effect)?.0)
    }

    /// Holds `writers` shared, as every write in progress does, until the
    /// lock is dropped: for writes made of several commits.
    ///
    /// # Errors
    ///
    /// Fails if the lock cannot be taken.
    pub(crate) fn writing(&self) -> Result<LockedFile, StoreError> {
        let oplog = self.oplog();
        oplog.lock(OpLog::<P>::WRITERS, LockMode::Shared)
    }

    /// Commits `write`, logging `effect` as an entry synced with it, at no
    /// extra sync, returning the entry's sequence number.
    ///
    /// # Errors
    ///
    /// Fails if the entry cannot be appended or the write committed.
    pub(crate) fn commit_logged(
        &self,
        write: DurableWrite<'_, P>,
        effect: &Effect,
    ) -> Result<u64, StoreError> {
        // Held until the write is published, so readers can tell an entry
        // whose write is in progress from one whose write crashed.
        let _writing = self.writing()?;
        let (seq, segment) = self.oplog().append(effect)?;
        write.sync_with(segment);
        write.commit()?;
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use origin_platform::Native;
    use origin_shared::{RecordId, StatementId};

    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default())
            .with_actor(Actor::new("test").in_workspace(dir.path()));
        store.init().unwrap();
        (dir, store)
    }

    /// Logs `effect` with an empty write.
    fn log(store: &Store, effect: &Effect) -> u64 {
        store.commit_logged(store.durable_write(), effect).unwrap()
    }

    fn wrote(seed: u8) -> Effect {
        Effect::Wrote(vec![index::Entry::Statement {
            id: StatementId::from(ObjectId::from_bytes([seed; 32])),
            subject: RecordId::Statement(ObjectId::from_bytes([seed; 32]).into()),
            supersedes: None,
            group: None,
        }])
    }

    #[test]
    fn entries_round_trip() {
        let revision = |seed| RevisionId::from(ObjectId::from_bytes([seed; 32]));
        let effects = [
            wrote(1),
            Effect::Waiting {
                onto: TargetName::MAIN,
                proposal: Proposal::Change(ChangeId::from_bytes([3; 16])),
            },
            Effect::Waiting {
                onto: TargetName::new("release/v3").unwrap(),
                proposal: Proposal::Target(TargetName::MAIN),
            },
            Effect::Moved(Move {
                from: None,
                to: revision(4),
                why: Why::Command,
            }),
            Effect::Moved(Move {
                from: Some(revision(4)),
                to: revision(5),
                why: Why::Undo(7),
            }),
        ];
        for (seq, effect) in (1..).zip(effects) {
            let entry = Entry {
                seq,
                op: 1,
                workspace: (seq % 2 == 0).then(|| PathBuf::from("/work/ws")),
                command: "land 3fa2".to_owned(),
                effect,
            };
            let frame = entry.encode();
            let payload = &frame[4..frame.len() - Frames::FRAMING + 4];
            assert_eq!(Entry::decode(payload), Some(entry));
        }
        for why in [Why::Snapshot, Why::Command, Why::Undo(12)] {
            assert_eq!(Why::parse(&why.to_string()), Some(why));
        }
    }

    #[test]
    fn sequence_numbers_continue_across_segments_and_retention() {
        let (_dir, store) = store();
        let oplog = store.oplog();
        assert_eq!(oplog.last_seq().unwrap(), 0);
        assert_eq!(oplog.after(0).unwrap(), []);
        let seqs: Vec<u64> = (0..200).map(|i| log(&store, &wrote(i))).collect();
        assert_eq!(seqs, (1..=200).collect::<Vec<_>>());
        assert_eq!(oplog.last_seq().unwrap(), 200);

        let segments = oplog.segments().unwrap();
        assert_eq!(
            segments.len(),
            OpLog::<Native>::SEGMENTS,
            "the oldest were deleted"
        );
        let oldest = segments[0];
        assert!(matches!(
            oplog.after(0),
            Err(StoreError::Expired { after: 0, oldest: o }) if o == oldest
        ));
        let kept = oplog.after(oldest - 1).unwrap();
        assert_eq!(kept.first().unwrap().seq, oldest);
        assert!(kept.windows(2).all(|pair| pair[1].seq == pair[0].seq + 1));
        let tail = oplog.after(197).unwrap();
        assert_eq!(
            tail.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            [198, 199, 200]
        );
        assert_eq!(tail[0].effect, wrote(197));

        let newest: Vec<_> = oplog.newest_first().unwrap().map(Result::unwrap).collect();
        assert_eq!(newest.len(), kept.len());
        assert_eq!(newest[0].seq, 200);
        assert!(newest.iter().all(|entry| entry.op == 1), "one operation");
        assert_eq!(newest[0].command, "test");
    }

    #[test]
    fn partial_appends_are_cut_and_damage_never_reuses_numbers() {
        let (_dir, store) = store();
        let oplog = store.oplog();
        log(&store, &wrote(1));
        let segment = oplog.segment(1);
        let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
        // A crash in the middle of an append.
        let frame = Entry {
            seq: 2,
            op: 2,
            workspace: None,
            command: String::new(),
            effect: wrote(2),
        }
        .encode();
        file.write_all(&frame[..frame.len() / 2]).unwrap();
        assert_eq!(oplog.last_seq().unwrap(), 1, "readers skip it");
        assert_eq!(log(&store, &wrote(3)), 2, "the next append cuts it off");
        assert_eq!(oplog.after(0).unwrap().len(), 2);

        // Damage at the end of the segment, rather than a partial frame.
        let mut bytes = fs::read(&segment).unwrap();
        let len = bytes.len();
        bytes[len - 6] ^= 1;
        fs::write(&segment, &bytes).unwrap();
        assert_eq!(oplog.last_seq().unwrap(), 1, "readers stop at it");
        let next = log(&store, &wrote(4));
        assert!(next > 2, "{next} reuses a hidden number");
        assert_eq!(oplog.segments().unwrap(), [1, next], "in a new segment");
        assert_eq!(oplog.after(next - 1).unwrap()[0].effect, wrote(4));
    }
}
