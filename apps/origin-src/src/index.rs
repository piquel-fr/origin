//! The record index: what reading every record would tell, without reading
//! them. It knows each change's revisions and heads (more than one head is
//! divergence), the statements about each record, and the order this node
//! received its records in.
//!
//! The index is derived from the store, so it is never trusted over it: a
//! missing or corrupt index is rebuilt from the store's records
//! ([`Index::rebuild`]), never fatal. A rebuild skips the objects that are
//! corrupt or don't decode, which readers can't use either, and reports
//! them ([`Skipped`]).
//!
//! On disk it is one append-only file, `index/log`: a header, then frames.
//! A frame is its payload's `u32` length, the payload, the first 8 bytes of
//! the payload's BLAKE3 hash, and the length again, so the last frame can be
//! checked from the end. A payload is either what the index keeps of each
//! record of a batch, or the mark that the batch at some offset was
//! published.
//!
//! [`Batch::commit`](crate::store::Batch::commit) appends a batch's entries
//! before publishing its records, and the batch sync makes them durable
//! with the records, so indexing costs no sync of its own. Once the records
//! are published, it marks the batch, without syncing. Opening the index
//! trusts the marked batches, and checks that the records of the others are
//! in the store: those belong to batches that crashed, failed, or are being
//! published. A partial last frame is an append in progress, or one a crash
//! cut short, and is skipped. Any other damage rebuilds the index.
//!
//! Batches append under a shared lock on `index/lock`, held from appending
//! their entries to marking them, and appends serialize on an exclusive
//! lock of the log itself. A rebuild holds `index/lock` exclusively, so it
//! never misses a batch: it sees every batch's records published, or none
//! of them appended yet. Rebuilding keeps the arrival order the old log
//! still knows, and puts the records it didn't after them.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use origin_platform::{DurableWrite, LockMode, LockedFile, Platform};
use origin_shared::object::{DecodeError, IdMismatch, Reader};
use origin_shared::{
    AcceptanceId, ChangeId, Digest, EncodedObject, ObjectId, ObjectKind, RecordId, Revision,
    RevisionId, Statement, StatementId, TreeId,
};

use crate::store::{Store, StoreError};

/// The records of a store, by the fields they're looked up by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    /// Every record indexed, in the order this node received them.
    arrivals: Vec<RecordId>,
    /// The records in `arrivals`, to skip duplicates.
    known: HashSet<RecordId>,
    /// The revisions of each change.
    changes: HashMap<ChangeId, Change>,
    /// The statements about each record, in arrival order.
    statements: HashMap<RecordId, Vec<StatementId>>,
}

/// The revisions of one change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Change {
    /// In arrival order.
    revisions: Vec<RevisionId>,
    /// The revisions another revision of the change evolves from: all but
    /// the heads.
    superseded: HashSet<RevisionId>,
}

/// A rebuilt index, and the objects the rebuild skipped.
#[derive(Debug)]
pub struct Rebuilt {
    /// The index.
    pub index: Index,
    /// The objects skipped, also reported on stderr.
    pub skipped: Vec<Skipped>,
}

/// An object a rebuild skips, since readers can't use it either.
#[derive(Debug, thiserror::Error)]
pub enum Skipped {
    /// Its content doesn't hash to its id.
    #[error("the index skips a corrupt object: {0}")]
    Corrupt(IdMismatch),
    /// It's a record that doesn't decode.
    #[error("the index skips record {id}, which doesn't decode: {error}")]
    Undecodable {
        /// The record.
        id: ObjectId,
        /// Why it doesn't decode.
        error: DecodeError,
    },
}

impl Index {
    /// Reads the index of `store`, rebuilding it if it's missing or
    /// corrupt.
    ///
    /// # Errors
    ///
    /// Fails if the index or the store cannot be read, or a rebuilt index
    /// cannot be written.
    pub fn open<P: Platform>(store: &Store<P>) -> Result<Self, StoreError> {
        let (index, intact) = Self::read(store)?;
        if intact {
            return Ok(index);
        }
        let _lock = store.lock_index(LockMode::Exclusive)?;
        // Another process may have rebuilt it while this one waited.
        let (index, intact) = Self::read(store)?;
        if intact {
            return Ok(index);
        }
        Ok(Self::rebuild_locked(store, &index)?.index)
    }

    /// Reconstructs the index of `store` from its records, and replaces the
    /// one on disk. Records the old index knew keep their arrival order.
    /// Objects that are corrupt or don't decode are skipped, and reported on
    /// stderr.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read or the index cannot be written.
    pub fn rebuild<P: Platform>(store: &Store<P>) -> Result<Rebuilt, StoreError> {
        let _lock = store.lock_index(LockMode::Exclusive)?;
        let (old, _) = Self::read(store)?;
        Self::rebuild_locked(store, &old)
    }

    /// Rebuilds the index while holding its lock exclusively, keeping the
    /// arrival order of `old`.
    fn rebuild_locked<P: Platform>(store: &Store<P>, old: &Self) -> Result<Rebuilt, StoreError> {
        let order: HashMap<RecordId, usize> = old
            .arrivals
            .iter()
            .enumerate()
            .map(|(position, &record)| (record, position))
            .collect();
        let (mut entries, skipped) = store.entries()?;
        for skipped in &skipped {
            eprintln!("warning: {skipped}");
        }
        entries.sort_unstable_by_key(|entry| {
            let record = entry.record();
            (order.get(&record).copied().unwrap_or(usize::MAX), record)
        });
        store.write_index(&entries)?;
        let mut index = Self::default();
        for entry in entries {
            index.insert(entry);
        }
        Ok(Rebuilt { index, skipped })
    }

    /// Reads as much of the index as is intact, returning whether all of it
    /// was. A missing index is empty and not intact.
    fn read<P: Platform>(store: &Store<P>) -> Result<(Self, bool), StoreError> {
        let path = store.index_log();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((Self::default(), false));
            }
            Err(source) => return Err(StoreError::io(&path, source)),
        };
        let (frames, tail) = Log::parse(&bytes);
        let published: HashSet<u64> = frames
            .iter()
            .filter_map(|(_, frame)| match frame {
                Frame::Published(offset) => Some(*offset),
                Frame::Records(_) => None,
            })
            .collect();
        let mut index = Self::default();
        for (offset, frame) in frames {
            let Frame::Records(entries) = frame else {
                continue;
            };
            let trusted = published.contains(&offset);
            for entry in entries.into_owned() {
                if trusted || store.contains(entry.id())? {
                    index.insert(entry);
                }
            }
        }
        Ok((index, tail != Tail::Damaged))
    }

    /// Adds a record, unless it's already indexed.
    fn insert(&mut self, entry: Entry) {
        if !self.known.insert(entry.record()) {
            return;
        }
        self.arrivals.push(entry.record());
        match entry {
            Entry::Revision {
                id,
                change,
                predecessors,
            } => {
                let change = self.changes.entry(change).or_default();
                change.revisions.push(id);
                change.superseded.extend(predecessors);
            }
            Entry::Statement { id, subject } => {
                self.statements.entry(subject).or_default().push(id);
            }
            Entry::Acceptance(_) => {}
        }
    }

    /// Every record indexed, in the order this node received them. A
    /// position in it is a cursor: records received later come after it.
    #[must_use]
    pub fn arrivals(&self) -> &[RecordId] {
        &self.arrivals
    }

    /// The revisions of `change`, in arrival order.
    #[must_use]
    pub fn revisions(&self, change: ChangeId) -> &[RevisionId] {
        self.changes
            .get(&change)
            .map_or(&[], |change| &change.revisions)
    }

    /// The heads of `change`, in arrival order: its revisions that no other
    /// revision of it evolves from.
    #[must_use]
    pub fn heads(&self, change: ChangeId) -> Vec<RevisionId> {
        self.changes.get(&change).map_or_else(Vec::new, |change| {
            change
                .revisions
                .iter()
                .filter(|revision| !change.superseded.contains(revision))
                .copied()
                .collect()
        })
    }

    /// Whether `change` has more than one head.
    #[must_use]
    pub fn is_divergent(&self, change: ChangeId) -> bool {
        self.heads(change).len() > 1
    }

    /// The statements about `subject`, in arrival order.
    #[must_use]
    pub fn statements(&self, subject: RecordId) -> &[StatementId] {
        self.statements.get(&subject).map_or(&[], Vec::as_slice)
    }

    /// The one change whose id starts with `prefix`, a few hex digits.
    ///
    /// # Errors
    ///
    /// Fails if no change, or more than one, has an id starting with
    /// `prefix`.
    pub fn find_change(&self, prefix: &str) -> Result<ChangeId, FindChangeError> {
        let prefix = prefix.to_ascii_lowercase();
        if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(FindChangeError::Invalid(prefix));
        }
        let mut matches: Vec<ChangeId> = self
            .changes
            .keys()
            .filter(|change| change.to_hex().starts_with(&prefix))
            .copied()
            .collect();
        match matches.len() {
            0 => Err(FindChangeError::NotFound(prefix)),
            1 => Ok(matches[0]),
            _ => {
                matches.sort_unstable();
                Err(FindChangeError::Ambiguous { prefix, matches })
            }
        }
    }
}

/// A change id prefix that doesn't name exactly one change.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FindChangeError {
    /// It isn't hex digits.
    #[error("`{0}` is not a change id")]
    Invalid(String),
    /// No change has an id starting with it.
    #[error("no change has an id starting with `{0}`")]
    NotFound(String),
    /// Several changes do.
    #[error(
        "`{prefix}` could be any of the changes {}",
        matches.iter().map(ChangeId::to_hex).collect::<Vec<_>>().join(", ")
    )]
    Ambiguous {
        /// The prefix.
        prefix: String,
        /// The changes whose ids start with it.
        matches: Vec<ChangeId>,
    },
}

/// What the index keeps of one record: its id, and the fields it's looked
/// up by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Revision {
        id: RevisionId,
        change: ChangeId,
        predecessors: Vec<RevisionId>,
    },
    Statement {
        id: StatementId,
        subject: RecordId,
    },
    Acceptance(AcceptanceId),
}

impl Entry {
    /// The entry of `object`, or `None` if it isn't indexed: blobs and
    /// trees.
    ///
    /// # Errors
    ///
    /// Fails if the object is a record that doesn't decode.
    pub(crate) fn of(object: &EncodedObject) -> Result<Option<Self>, DecodeError> {
        let id = object.id();
        Ok(match object.kind()? {
            ObjectKind::Blob | ObjectKind::Tree => None,
            ObjectKind::Revision => {
                let revision: Revision = object.decode_as()?;
                Some(Self::Revision {
                    id: id.into(),
                    change: revision.change,
                    predecessors: revision.predecessors,
                })
            }
            ObjectKind::Statement => Some(Self::Statement {
                id: id.into(),
                subject: object.decode_as::<Statement>()?.subject,
            }),
            ObjectKind::Acceptance => {
                // Decoding checks it's well formed, as for other records.
                object.decode_as::<origin_shared::Acceptance>()?;
                Some(Self::Acceptance(id.into()))
            }
        })
    }

    fn record(&self) -> RecordId {
        match *self {
            Self::Revision { id, .. } => RecordId::Revision(id),
            Self::Statement { id, .. } => RecordId::Statement(id),
            Self::Acceptance(id) => RecordId::Acceptance(id),
        }
    }

    fn id(&self) -> ObjectId {
        Log::split(self.record()).1
    }

    /// Appends the entry: its record's kind and id, then for a revision its
    /// change and a `u32` count of predecessors and their ids, and for a
    /// statement its subject's kind and id.
    fn write(&self, out: &mut Vec<u8>) {
        Log::write_record(self.record(), out);
        match self {
            Self::Revision {
                change,
                predecessors,
                ..
            } => {
                out.extend_from_slice(change.as_bytes());
                let count = u32::try_from(predecessors.len()).expect("fewer than 2^32");
                out.extend_from_slice(&count.to_le_bytes());
                for predecessor in predecessors {
                    out.extend_from_slice(predecessor.as_bytes());
                }
            }
            Self::Statement { subject, .. } => Log::write_record(*subject, out),
            Self::Acceptance(_) => {}
        }
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(match Log::read_record(reader)? {
            RecordId::Revision(id) => {
                let change = ChangeId::from_bytes(reader.array()?);
                let count = reader.read_len()?;
                let predecessors = (0..count)
                    .map(|_| reader.array().map(|id| ObjectId::from_bytes(id).into()))
                    .collect::<Result<_, _>>()?;
                Self::Revision {
                    id,
                    change,
                    predecessors,
                }
            }
            RecordId::Statement(id) => Self::Statement {
                id,
                subject: Log::read_record(reader)?,
            },
            RecordId::Acceptance(id) => Self::Acceptance(id),
            RecordId::Snapshot(_) => {
                return Err(DecodeError::UnknownTag {
                    field: "index entry",
                    tag: ObjectKind::Tree as u8,
                });
            }
        })
    }
}

/// A frame of the log.
#[derive(Debug)]
enum Frame<'a> {
    /// The entries of a batch, appended before its records are published.
    Records(Cow<'a, [Entry]>),
    /// The records of the batch at this offset are published.
    Published(u64),
}

impl Frame<'_> {
    const RECORDS: u8 = 1;
    const PUBLISHED: u8 = 2;

    /// Encodes the frame: its payload's length, the payload (a tag and the
    /// contents), a check, and the length again, so the last frame of a log
    /// can be read backwards.
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![0; 4];
        match self {
            Self::Records(entries) => {
                out.push(Self::RECORDS);
                for entry in entries.iter() {
                    entry.write(&mut out);
                }
            }
            Self::Published(offset) => {
                out.push(Self::PUBLISHED);
                out.extend_from_slice(&offset.to_le_bytes());
            }
        }
        let len = u32::try_from(out.len() - 4)
            .expect("index frames are under 4 GiB")
            .to_le_bytes();
        out[..4].copy_from_slice(&len);
        let check = Log::check(&out[4..]);
        out.extend_from_slice(&check);
        out.extend_from_slice(&len);
        out
    }

    fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(payload);
        let frame = match reader.u8()? {
            Self::RECORDS => {
                let mut entries = Vec::new();
                while reader.remaining() > 0 {
                    entries.push(Entry::read(&mut reader)?);
                }
                Self::Records(Cow::Owned(entries))
            }
            Self::PUBLISHED => Self::Published(reader.u64()?),
            tag => {
                return Err(DecodeError::UnknownTag {
                    field: "index frame",
                    tag,
                });
            }
        };
        reader.finish()?;
        Ok(frame)
    }
}

/// The format of `index/log`.
struct Log;

impl Log {
    /// The start of every log, with the format's version.
    const HEADER: &[u8] = b"origin index 1\n";

    /// The length of a frame's check.
    const CHECK: usize = 8;

    /// The bytes a frame adds to its payload: two lengths and a check.
    const FRAMING: usize = 4 + Self::CHECK + 4;

    fn check(payload: &[u8]) -> [u8; Self::CHECK] {
        Digest::of(payload).as_bytes()[..Self::CHECK]
            .try_into()
            .expect("a digest is longer than a check")
    }

    /// Encodes a log holding `entries`, all published.
    fn encode(entries: &[Entry]) -> Vec<u8> {
        let mut log = Self::HEADER.to_vec();
        let offset = log.len() as u64;
        log.extend(Frame::Records(Cow::Borrowed(entries)).encode());
        log.extend(Frame::Published(offset).encode());
        log
    }

    /// The frames of a log and their offsets, up to the first broken one,
    /// and how it ends after them.
    fn parse(bytes: &[u8]) -> (Vec<(u64, Frame<'static>)>, Tail) {
        let mut frames = Vec::new();
        let Some(mut rest) = bytes.strip_prefix(Self::HEADER) else {
            return (frames, Tail::Damaged);
        };
        while !rest.is_empty() {
            let offset = (bytes.len() - rest.len()) as u64;
            let frame = Self::payload(rest).and_then(|(payload, len)| {
                let frame = Frame::decode(payload).map_err(|_| Broken::Damaged)?;
                Ok((frame, len))
            });
            match frame {
                Ok((frame, len)) => {
                    frames.push((offset, frame));
                    rest = &rest[len..];
                }
                Err(Broken::Partial) => return (frames, Tail::Partial(offset)),
                Err(Broken::Damaged) => return (frames, Tail::Damaged),
            }
        }
        (frames, Tail::Clean)
    }

    /// Checks the frame `bytes` start with, returning its payload and its
    /// length.
    fn payload(bytes: &[u8]) -> Result<(&[u8], usize), Broken> {
        let (len, rest) = bytes.split_first_chunk::<4>().ok_or(Broken::Partial)?;
        let (payload, rest) = rest
            .split_at_checked(u32::from_le_bytes(*len) as usize)
            .ok_or(Broken::Partial)?;
        let (check, rest) = rest
            .split_first_chunk::<{ Self::CHECK }>()
            .ok_or(Broken::Partial)?;
        let (end, _) = rest.split_first_chunk::<4>().ok_or(Broken::Partial)?;
        if end != len || *check != Self::check(payload) {
            return Err(Broken::Damaged);
        }
        Ok((payload, payload.len() + Self::FRAMING))
    }

    /// Whether the locked `log` ends with an intact frame, or the header.
    /// Reads only the last frame, backwards from its trailing length.
    fn ends_cleanly(mut log: &File) -> io::Result<bool> {
        let len = log.metadata()?.len();
        let header = Self::HEADER.len() as u64;
        if len <= header {
            return Ok(len == header);
        }
        let mut end = [0; 4];
        log.seek(SeekFrom::Start(len.saturating_sub(4)))?;
        log.read_exact(&mut end)?;
        let frame = u64::from(u32::from_le_bytes(end)) + Self::FRAMING as u64;
        if frame > len - header {
            return Ok(false);
        }
        let mut bytes = vec![0; usize::try_from(frame).map_err(io::Error::other)?];
        log.seek(SeekFrom::Start(len - frame))?;
        log.read_exact(&mut bytes)?;
        Ok(Self::payload(&bytes).is_ok())
    }

    /// Appends a record's kind and id.
    fn write_record(record: RecordId, out: &mut Vec<u8>) {
        let (kind, id) = Self::split(record);
        out.push(kind as u8);
        out.extend_from_slice(id.as_bytes());
    }

    fn read_record(reader: &mut Reader<'_>) -> Result<RecordId, DecodeError> {
        let kind = reader.u8()?;
        let id = ObjectId::from_bytes(reader.array()?);
        Ok(match kind {
            k if k == ObjectKind::Tree as u8 => RecordId::Snapshot(TreeId::from(id)),
            k if k == ObjectKind::Revision as u8 => RecordId::Revision(id.into()),
            k if k == ObjectKind::Statement as u8 => RecordId::Statement(id.into()),
            k if k == ObjectKind::Acceptance as u8 => RecordId::Acceptance(id.into()),
            tag => {
                return Err(DecodeError::UnknownTag {
                    field: "record kind",
                    tag,
                });
            }
        })
    }

    /// A record's kind and id.
    fn split(record: RecordId) -> (ObjectKind, ObjectId) {
        match record {
            RecordId::Snapshot(id) => (ObjectKind::Tree, id.into()),
            RecordId::Revision(id) => (ObjectKind::Revision, id.into()),
            RecordId::Statement(id) => (ObjectKind::Statement, id.into()),
            RecordId::Acceptance(id) => (ObjectKind::Acceptance, id.into()),
        }
    }
}

/// How a log ends after its last intact frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tail {
    /// It ends there.
    Clean,
    /// With part of a frame, at this offset: an append in progress, or one a
    /// crash cut short. Appenders cut such a leftover off before appending,
    /// so it is always last.
    Partial(u64),
    /// With damage, or a header from another version: the index must be
    /// rebuilt.
    Damaged,
}

/// Why a frame cannot be decoded.
enum Broken {
    /// The bytes end before it does.
    Partial,
    /// It is not what was appended: it fails its check, or its payload
    /// doesn't decode. Either way the index is rebuilt, so why doesn't
    /// matter.
    Damaged,
}

impl<P: Platform> Store<P> {
    fn index_log(&self) -> PathBuf {
        self.index_dir().join("log")
    }

    /// Locks the index: shared by batches being indexed, exclusive for a
    /// rebuild.
    fn lock_index(&self, mode: LockMode) -> Result<LockedFile, StoreError> {
        let dir = self.index_dir();
        fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
        Ok(LockedFile::open(&self.platform, &dir.join("lock"), mode)?)
    }

    /// Durably replaces the index with one holding `entries`.
    pub(crate) fn write_index(&self, entries: &[Entry]) -> Result<(), StoreError> {
        let write = self.durable_write();
        write.stage(self.index_log(), &Log::encode(entries))?;
        Ok(write.commit()?)
    }

    /// Commits `write`, which stages the records `entries` describe, and
    /// indexes them: their entries are durable with the records, and marked
    /// published after them.
    ///
    /// # Errors
    ///
    /// Fails if the entries cannot be appended or the write cannot be
    /// committed.
    pub(crate) fn commit_indexed(
        &self,
        write: DurableWrite<'_, P>,
        entries: &[Entry],
    ) -> Result<(), StoreError> {
        let _lock = self.lock_index(LockMode::Shared)?;
        let offset = self.append_to_index(&Frame::Records(Cow::Borrowed(entries)))?;
        if offset.is_some() {
            write.sync_with(self.index_log());
        }
        write.commit()?;
        if let Some(offset) = offset {
            // Best effort: without the mark, opening the index checks that
            // the records are in the store instead.
            let _ = self.append_to_index(&Frame::Published(offset));
        }
        Ok(())
    }

    /// Appends `frame` to the log, without syncing it, returning its offset,
    /// or `None` if there's no log: opening the index will rebuild it from
    /// the store.
    fn append_to_index(&self, frame: &Frame<'_>) -> Result<Option<u64>, StoreError> {
        let path = self.index_log();
        let io_error = |source| StoreError::io(&path, source);
        let log = match OpenOptions::new().read(true).append(true).open(&path) {
            Ok(log) => log,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error(source)),
        };
        // Closing the log releases the lock.
        self.platform
            .lock(&log, LockMode::Exclusive)
            .map_err(io_error)?;
        if !Log::ends_cleanly(&log).map_err(io_error)? {
            self.cut_partial_frame(&log).map_err(io_error)?;
        }
        let offset = log.metadata().map_err(io_error)?.len();
        if let Err(source) = (&log).write_all(&frame.encode()) {
            // Best effort: the next append cuts it off otherwise.
            let _ = log.set_len(offset);
            return Err(io_error(source));
        }
        Ok(Some(offset))
    }

    /// Cuts off the partial frame a crash left at the end of the locked
    /// `log`, durably, so appends never follow one. Damage is left for
    /// readers to find, so they rebuild the index.
    fn cut_partial_frame(&self, mut log: &File) -> io::Result<()> {
        let mut bytes = Vec::new();
        log.seek(SeekFrom::Start(0))?;
        log.read_to_end(&mut bytes)?;
        if let (_, Tail::Partial(offset)) = Log::parse(&bytes) {
            log.set_len(offset)?;
            self.platform.sync_file(log)?;
        }
        Ok(())
    }

    /// The entry of every record in the store, read from every core, and
    /// the objects skipped because they are corrupt or don't decode.
    fn entries(&self) -> Result<(Vec<Entry>, Vec<Skipped>), StoreError> {
        let ids = self.ids()?;
        let chunks = Self::on_every_core(&ids, |ids| {
            let (mut entries, mut skipped) = (Vec::new(), Vec::new());
            for &id in ids {
                match self.try_read(id) {
                    Ok(Some(object)) => match Entry::of(&object) {
                        Ok(entry) => entries.extend(entry),
                        Err(error) => skipped.push(Skipped::Undecodable { id, error }),
                    },
                    Ok(None) => {}
                    Err(StoreError::Corrupt(mismatch)) => skipped.push(Skipped::Corrupt(mismatch)),
                    Err(error) => return Err(error),
                }
            }
            Ok((entries, skipped))
        });
        let (entries, skipped): (Vec<Vec<_>>, Vec<Vec<_>>) =
            chunks.into_iter().collect::<Result<_, _>>()?;
        Ok((entries.concat(), skipped.into_iter().flatten().collect()))
    }
}

#[cfg(test)]
mod tests {
    use std::slice;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use origin_platform::Native;
    use origin_shared::{Base, BlobId, Kind, Record, Tree, Verdict};

    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        (dir, store)
    }

    fn change(seed: u8) -> ChangeId {
        ChangeId::from_bytes([seed; 16])
    }

    /// A revision of `change` evolving from `predecessors`.
    fn revision(change: ChangeId, predecessors: &[&EncodedObject], seed: &str) -> EncodedObject {
        Revision {
            change,
            predecessors: predecessors.iter().map(|p| p.id().into()).collect(),
            base: Base::Acceptance(ObjectId::from_bytes([0; 32]).into()),
            deps: Vec::new(),
            snapshot: Tree::default().encode().id().into(),
            description: BlobId::from(EncodedObject::blob(seed.as_bytes()).id()),
            author: "user:alice".parse().unwrap(),
        }
        .encode()
    }

    /// A statement about `subject`.
    fn statement(subject: RecordId, kind: &str) -> EncodedObject {
        Statement {
            subject,
            kind: Kind::new(kind).unwrap(),
            verdict: Some(Verdict::Pass),
            issuer: "node:ci-1".parse().unwrap(),
            inputs: None,
            payload: None,
            supersedes: None,
        }
        .encode()
    }

    fn revision_id(object: &EncodedObject) -> RevisionId {
        object.id().into()
    }

    #[test]
    fn detects_divergence_and_convergence() {
        let (_dir, store) = store();
        let c = change(1);
        let first = revision(c, &[], "first");
        let left = revision(c, &[&first], "left");
        let right = revision(c, &[&first], "right");
        let merged = revision(c, &[&left, &right], "merged");

        store.write_all(slice::from_ref(&first)).unwrap();
        let index = Index::open(&store).unwrap();
        assert_eq!(index.heads(c), [revision_id(&first)]);
        assert!(!index.is_divergent(c));

        store.write_all(slice::from_ref(&left)).unwrap();
        store.write_all(slice::from_ref(&right)).unwrap();
        let index = Index::open(&store).unwrap();
        assert_eq!(index.heads(c), [revision_id(&left), revision_id(&right)]);
        assert!(
            index.is_divergent(c),
            "two agents revised the same revision"
        );

        store.write_all(slice::from_ref(&merged)).unwrap();
        let index = Index::open(&store).unwrap();
        assert_eq!(index.heads(c), [revision_id(&merged)]);
        assert!(!index.is_divergent(c));
        assert_eq!(index.revisions(c).len(), 4);
        assert_eq!(index.heads(change(2)), [], "unknown changes have none");
    }

    #[test]
    fn finds_changes_by_unique_prefix() {
        let (_dir, store) = store();
        let ab = ChangeId::from_bytes([0xab; 16]);
        let ac = ChangeId::from_bytes([0xac; 16]);
        store
            .write_all(&[revision(ab, &[], "ab"), revision(ac, &[], "ac")])
            .unwrap();
        let index = Index::open(&store).unwrap();

        assert_eq!(index.find_change("ab"), Ok(ab));
        assert_eq!(index.find_change("AC"), Ok(ac), "either case");
        assert_eq!(index.find_change(&ab.to_hex()), Ok(ab));
        assert_eq!(
            index.find_change("a"),
            Err(FindChangeError::Ambiguous {
                prefix: "a".to_owned(),
                matches: vec![ab, ac]
            })
        );
        assert!(matches!(
            index.find_change("ff"),
            Err(FindChangeError::NotFound(_))
        ));
        for invalid in ["", "xyz"] {
            assert!(matches!(
                index.find_change(invalid),
                Err(FindChangeError::Invalid(_))
            ));
        }
    }

    /// Records arriving in several batches, out of order and some twice.
    fn write_history(store: &Store) {
        let (a, b) = (change(1), change(2));
        let a1 = revision(a, &[], "a1");
        let a2 = revision(a, &[&a1], "a2");
        let b1 = revision(b, &[], "b1");
        let b2 = revision(b, &[&b1], "b2");
        let on_a2 = statement(RecordId::Revision(a2.id().into()), "ci.test");
        let on_statement = statement(RecordId::Statement(on_a2.id().into()), "flaky");
        let on_snapshot = statement(
            RecordId::Snapshot(Tree::default().encode().id().into()),
            "lint",
        );
        let batches = [
            // A successor before its predecessor, as a fetch can deliver.
            vec![a2, on_a2, EncodedObject::blob(b"file")],
            vec![a1.clone(), b1, on_statement],
            vec![b2, on_snapshot, a1],
        ];
        for batch in &batches {
            store.write_all(batch).unwrap();
        }
    }

    #[test]
    fn rebuilding_equals_updating() {
        let (_dir, store) = store();
        write_history(&store);
        let updated = Index::open(&store).unwrap();
        assert_eq!(updated.arrivals().len(), 7, "every record once, no blob");
        assert_eq!(updated.heads(change(1)).len(), 1);

        assert_eq!(Index::rebuild(&store).unwrap().index, updated);
        assert_eq!(Index::open(&store).unwrap(), updated, "rebuilt on disk");

        // Without the old index, only the arrival order is lost.
        fs::remove_file(store.index_log()).unwrap();
        assert_eq!(sorted(Index::open(&store).unwrap()), sorted(updated));
    }

    /// The index with every list sorted: what's left without arrival order.
    fn sorted(mut index: Index) -> Index {
        index.arrivals.sort();
        for change in index.changes.values_mut() {
            change.revisions.sort();
        }
        for statements in index.statements.values_mut() {
            statements.sort();
        }
        index
    }

    #[test]
    fn damage_rebuilds_the_index() {
        let (_dir, store) = store();
        write_history(&store);
        let index = Index::open(&store).unwrap();
        let log = store.index_log();
        let bytes = fs::read(&log).unwrap();

        // Damage past the first frame keeps the arrival order before it:
        // here, the mark that the last batch was published.
        let mut late = bytes.clone();
        *late.last_mut().unwrap() ^= 1;
        fs::write(&log, late).unwrap();
        assert_eq!(Index::open(&store).unwrap(), index);
        assert_eq!(
            Log::parse(&fs::read(&log).unwrap()).1,
            Tail::Clean,
            "repaired on disk"
        );

        let mut early = bytes.clone();
        early[Log::HEADER.len() + 10] ^= 1;
        let mut versioned = bytes;
        versioned[Log::HEADER.len() - 2] = b'9';
        for damaged in [early, versioned, Vec::new()] {
            fs::write(&log, damaged).unwrap();
            assert_eq!(sorted(Index::open(&store).unwrap()), sorted(index.clone()));
            assert_eq!(
                Log::parse(&fs::read(&log).unwrap()).1,
                Tail::Clean,
                "repaired on disk"
            );
        }
    }

    #[test]
    fn recovers_from_crashed_batches() {
        let (_dir, store) = store();
        let c = change(1);
        let published = revision(c, &[], "published");
        let unpublished = revision(c, &[], "unpublished");
        store.write_all(slice::from_ref(&published)).unwrap();

        // A batch that crashed after appending its entries: one record made
        // it into the store before the crash, one didn't, and neither was
        // marked published.
        let entries = [&published, &unpublished].map(|object| Entry::of(object).unwrap().unwrap());
        let frame = Frame::Records(Cow::Borrowed(&entries)).encode();
        // Then a crash in the middle of the next append.
        let mut log = OpenOptions::new()
            .append(true)
            .open(store.index_log())
            .unwrap();
        log.write_all(&frame).unwrap();
        log.write_all(&frame[..frame.len() / 2]).unwrap();

        let (index, intact) = Index::read(&store).unwrap();
        assert!(intact, "a partial last frame is no damage");
        assert_eq!(index.heads(c), [revision_id(&published)]);

        // The next append cuts the partial frame off instead of following it.
        let next = revision(c, &[&published], "next");
        store.write_all(slice::from_ref(&next)).unwrap();
        let log = fs::read(store.index_log()).unwrap();
        assert_eq!(Log::parse(&log).1, Tail::Clean);
        assert_eq!(Index::open(&store).unwrap().heads(c), [revision_id(&next)]);
    }

    #[test]
    fn rebuilds_skip_and_report_corrupt_records() {
        let (_dir, store) = store();
        let c = change(1);
        let intact = revision(c, &[], "intact");
        let corrupt = revision(c, &[], "corrupt");
        store.write_all(&[intact.clone(), corrupt.clone()]).unwrap();
        let other = zstd::bulk::compress(intact.bytes(), 1).unwrap();
        fs::write(store.path(corrupt.id()), other).unwrap();

        let Rebuilt { index, skipped } = Index::rebuild(&store).unwrap();
        assert_eq!(index.heads(c), [revision_id(&intact)]);
        assert!(
            matches!(&skipped[..], [Skipped::Corrupt(mismatch)] if mismatch.expected == corrupt.id()),
            "{skipped:?}"
        );
    }

    #[test]
    fn rejects_records_that_do_not_decode() {
        let (_dir, store) = store();
        let bytes = vec![ObjectKind::Revision as u8, 1, 2, 3];
        let id = ObjectId::from_bytes(*Digest::of(&bytes).as_bytes());
        let malformed = EncodedObject::verify(id, bytes).unwrap();
        let error = store.write_all(&[malformed]).unwrap_err();
        assert!(matches!(error, StoreError::Decode { .. }), "{error}");
        assert!(!store.contains(id).unwrap());
    }

    #[test]
    fn rebuilds_never_miss_concurrent_writes() {
        let (_dir, store) = store();
        let done = AtomicBool::new(false);
        let written: Vec<_> = thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    Index::rebuild(&store).unwrap();
                }
            });
            let writers: Vec<_> = (0..4)
                .map(|writer| {
                    let store = &store;
                    scope.spawn(move || {
                        (0..20)
                            .map(|i| {
                                let object = revision(change(writer), &[], &format!("{i}"));
                                store.write_all(slice::from_ref(&object)).unwrap();
                                object.id()
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let written = writers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect();
            done.store(true, Ordering::Relaxed);
            written
        });
        let index = Index::open(&store).unwrap();
        for id in written {
            assert!(index.known.contains(&RecordId::Revision(id.into())), "{id}");
        }
    }
}
