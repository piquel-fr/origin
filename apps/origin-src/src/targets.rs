//! Target chains: the acceptances of each target, in order.
//!
//! A target lives in `targets/<id>/`, where the [`TargetId`] is the hash of
//! its name, so names never become file names:
//!
//! - `name`: the target's name.
//! - `log`: the id of each acceptance in hex, one line each, append-only.
//!   Appenders hold an exclusive lock on it, which the kernel releases if
//!   the process dies, and it is never replaced, so the lock always
//!   protects it.
//! - `head`: `<seq> <id>`, the latest published acceptance, replaced
//!   atomically.
//!
//! Appending writes the acceptance durably, then appends its id to the log
//! and syncs it, and only then publishes the new head. Readers go by the
//! head, so they only ever see durable acceptances. If a crash leaves the
//! log ahead of the head, or a partial line at its end, the next append
//! repairs it first, syncing the log before publishing what it holds.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::slice;

use origin_platform::{DurableWrite, LockMode, LockedFile, Platform};
use origin_shared::{
    Acceptance, AcceptanceId, Base, EncodedObject, Head, Record, Revision, RevisionId, Source,
    TargetId, TargetName,
};

use crate::oplog::Effect;
use crate::store::{Store, StoreError};

/// Describes where a target is, for error messages.
pub(crate) fn describe(head: Option<Head>) -> String {
    head.map_or_else(|| "nothing".to_owned(), |head| head.to_string())
}

/// The files of one target.
struct TargetFiles {
    dir: PathBuf,
}

impl TargetFiles {
    /// The length of a log line: an id in hex and a newline.
    const LINE: usize = 2 * AcceptanceId::LEN + 1;

    fn name(&self) -> PathBuf {
        self.dir.join("name")
    }

    fn log(&self) -> PathBuf {
        self.dir.join("log")
    }

    fn head(&self) -> PathBuf {
        self.dir.join("head")
    }

    /// Reads a file, or `None` if it doesn't exist.
    fn read(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StoreError::io(path, source)),
        }
    }

    /// The published head.
    fn read_head(&self) -> Result<Option<Head>, StoreError> {
        let path = self.head();
        let Some(bytes) = Self::read(&path)? else {
            return Ok(None);
        };
        str::from_utf8(&bytes)
            .ok()
            .and_then(Self::parse_head)
            .map(Some)
            .ok_or(StoreError::Malformed(path))
    }

    /// Parses `head`'s `<seq> <id>` line.
    fn parse_head(line: &str) -> Option<Head> {
        let (seq, acceptance) = line.strip_suffix('\n')?.split_once(' ')?;
        Some(Head {
            seq: seq.parse().ok()?,
            acceptance: acceptance.parse().ok()?,
        })
    }

    /// Parses the complete lines of a log, ignoring a partial last one.
    fn parse_log(&self, bytes: &[u8]) -> Result<Vec<AcceptanceId>, StoreError> {
        bytes
            .as_chunks::<{ Self::LINE }>()
            .0
            .iter()
            .map(|line| {
                str::from_utf8(line)
                    .ok()
                    .and_then(|line| line.strip_suffix('\n')?.parse().ok())
                    .ok_or_else(|| StoreError::Malformed(self.log()))
            })
            .collect()
    }

    /// The complete entries of a locked log from position `since` on.
    fn entries_since(&self, log: &File, since: u64) -> Result<Vec<Head>, StoreError> {
        let mut bytes = Vec::new();
        let mut reader = log;
        reader
            .seek(SeekFrom::Start(since * Self::LINE as u64))
            .and_then(|_| reader.read_to_end(&mut bytes))
            .map_err(|source| StoreError::io(&self.log(), source))?;
        Ok((since..)
            .zip(self.parse_log(&bytes)?)
            .map(|(seq, acceptance)| Head { seq, acceptance })
            .collect())
    }

    /// The last complete entry of a locked log, after cutting off a partial
    /// line left by a crash.
    fn last_entry(&self, log: &File) -> Result<Option<Head>, StoreError> {
        let io_error = |source| StoreError::io(&self.log(), source);
        let len = log.metadata().map_err(io_error)?.len();
        let line = Self::LINE as u64;
        let entries = len / line;
        if len % line != 0 {
            log.set_len(entries * line).map_err(io_error)?;
        }
        let Some(seq) = entries.checked_sub(1) else {
            return Ok(None);
        };
        let mut bytes = [0; Self::LINE];
        let mut reader = log;
        reader
            .seek(SeekFrom::Start(seq * line))
            .and_then(|_| reader.read_exact(&mut bytes))
            .map_err(io_error)?;
        let acceptance = self.parse_log(&bytes)?[0];
        Ok(Some(Head { seq, acceptance }))
    }
}

impl<P: Platform> Store<P> {
    fn target_files(&self, id: TargetId) -> TargetFiles {
        TargetFiles {
            dir: self.targets_dir().join(id.to_hex()),
        }
    }

    /// The latest acceptance of `target`, or `None` if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Fails if the head cannot be read or is malformed.
    pub fn head(&self, target: &TargetName) -> Result<Option<Head>, StoreError> {
        self.target_files(target.id()).read_head()
    }

    /// The acceptances of `target`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails if the target's files cannot be read or are malformed.
    pub fn log(&self, target: &TargetName) -> Result<Vec<AcceptanceId>, StoreError> {
        self.log_since(target, 0)
    }

    /// The acceptances of `target` from position `since` on, oldest first.
    /// Reads only those lines of the log.
    ///
    /// # Errors
    ///
    /// Fails if the target's files cannot be read or are malformed.
    pub fn log_since(
        &self,
        target: &TargetName,
        since: u64,
    ) -> Result<Vec<AcceptanceId>, StoreError> {
        let files = self.target_files(target.id());
        let Some(head) = files.read_head()? else {
            return Ok(Vec::new());
        };
        if since > head.seq {
            return Ok(Vec::new());
        }
        // The log can be ahead of the head while an append is in progress:
        // read only up to the head.
        let path = files.log();
        let line = TargetFiles::LINE as u64;
        let len = usize::try_from((head.seq + 1 - since) * line)
            .map_err(|_| StoreError::Malformed(path.clone()))?;
        let mut bytes = vec![0; len];
        File::open(&path)
            .and_then(|log| log.read_exact_at(&mut bytes, since * line))
            .map_err(|source| match source.kind() {
                io::ErrorKind::UnexpectedEof => StoreError::Malformed(path.clone()),
                _ => StoreError::io(&path, source),
            })?;
        let log = files.parse_log(&bytes)?;
        if log.last() != Some(&head.acceptance) {
            return Err(StoreError::Malformed(path));
        }
        Ok(log)
    }

    /// The acceptance at position `seq` of `target`'s log, or `None` if
    /// none is published there yet. Reads one line of the log.
    ///
    /// # Errors
    ///
    /// Fails if the target's files cannot be read or are malformed.
    pub fn acceptance_at(
        &self,
        target: TargetId,
        seq: u64,
    ) -> Result<Option<AcceptanceId>, StoreError> {
        let files = self.target_files(target);
        let Some(head) = files.read_head()? else {
            return Ok(None);
        };
        if seq >= head.seq {
            return Ok((seq == head.seq).then_some(head.acceptance));
        }
        let path = files.log();
        let mut line = [0; TargetFiles::LINE];
        File::open(&path)
            .and_then(|log| log.read_exact_at(&mut line, seq * TargetFiles::LINE as u64))
            .map_err(|source| StoreError::io(&path, source))?;
        Ok(files.parse_log(&line)?.first().copied())
    }

    /// The acceptance that landed `revision`, `id`, if one did. A revision
    /// only lands on the tip it's based on, so the acceptance after that tip
    /// tells.
    ///
    /// # Errors
    ///
    /// Fails if the acceptances or the target's files cannot be read.
    pub fn landing(
        &self,
        id: RevisionId,
        revision: &Revision,
    ) -> Result<Option<Acceptance>, StoreError> {
        let Base::Acceptance(base) = revision.base else {
            return Ok(None);
        };
        let base = self.get(base)?;
        let Some(next) = self.acceptance_at(base.body().target, base.body().seq + 1)? else {
            return Ok(None);
        };
        let next = self.get(next)?;
        Ok(next
            .body()
            .landed
            .contains(&Source::Revision(id))
            .then_some(next))
    }

    /// The target the stack of `revision` is on: that of the acceptance at
    /// the bottom of its stack, found by following bases.
    ///
    /// # Errors
    ///
    /// Fails if a record of the stack cannot be read, or the store doesn't
    /// know the target's name.
    pub fn stack_target(&self, revision: RevisionId) -> Result<TargetName, StoreError> {
        let mut base = self.get(revision)?.base;
        let acceptance = loop {
            match base {
                Base::Revision(id) => base = self.get(id)?.base,
                Base::Acceptance(id) => break id,
            }
        };
        let target = self.get(acceptance)?.body().target;
        self.target_name(target)?
            .ok_or(StoreError::NotFound(acceptance.into()))
    }

    /// The name of the target `id`, or `None` if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Fails if its name cannot be read or is malformed.
    pub fn target_name(&self, id: TargetId) -> Result<Option<TargetName>, StoreError> {
        let path = self.target_files(id).name();
        TargetFiles::read(&path)?
            .map(|bytes| {
                String::from_utf8(bytes)
                    .ok()
                    .and_then(|name| TargetName::new(name).ok())
                    .ok_or(StoreError::Malformed(path))
            })
            .transpose()
    }

    /// The names of every target, in no particular order.
    ///
    /// # Errors
    ///
    /// Fails if the targets directory or a name cannot be read.
    pub fn targets(&self) -> Result<Vec<TargetName>, StoreError> {
        let dir = self.targets_dir();
        let entries = fs::read_dir(&dir).map_err(|source| StoreError::io(&dir, source))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::io(&dir, source))?;
            if DurableWrite::<P>::is_temporary(&entry.file_name()) {
                continue;
            }
            let files = TargetFiles { dir: entry.path() };
            // A target is created by publishing its first head.
            if files.read_head()?.is_none() {
                continue;
            }
            let path = files.name();
            let name = TargetFiles::read(&path)?
                .and_then(|bytes| TargetName::new(String::from_utf8(bytes).ok()?).ok())
                .ok_or(StoreError::Malformed(path))?;
            names.push(name);
        }
        Ok(names)
    }

    /// Appends `acceptance` to the chain of `target`, creating the target
    /// with its first acceptance. See [`Store::append_all`].
    ///
    /// # Errors
    ///
    /// Fails like [`Store::append_all`].
    pub fn append(
        &self,
        target: &TargetName,
        acceptance: &Acceptance,
    ) -> Result<AcceptanceId, StoreError> {
        self.append_all(target, slice::from_ref(acceptance), &[])?;
        Ok(acceptance.encode().id().into())
    }

    /// Appends `acceptances`, in order, to the chain of `target`, creating
    /// the target with its first acceptance.
    /// `with`, the records the acceptances rely on, such as promoted
    /// statements, are written in the same batch, so that costs no extra
    /// sync.
    ///
    /// This is a compare-and-swap: the first acceptance must follow the
    /// target's latest one, with the next sequence number and naming it as
    /// `prev`, and each the one before it. Concurrent appends are
    /// serialized, and all but one of those following the same acceptance
    /// fail. Verifying the signatures is the caller's job, since only it
    /// knows the target's authority.
    ///
    /// The op log says the target advanced to each acceptance appended,
    /// even one the store already had, such as one fetched before as
    /// another target's landing: its `target.advanced` event comes from
    /// that entry.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Conflict`] if an acceptance doesn't follow
    /// the one before it, [`StoreError::WrongTarget`] if one belongs to
    /// another target, or if the target's files cannot be locked, read or
    /// written.
    pub fn append_all(
        &self,
        target: &TargetName,
        acceptances: &[Acceptance],
        with: &[EncodedObject],
    ) -> Result<(), StoreError> {
        let Some(first) = acceptances.first() else {
            return Ok(());
        };
        let mut records = with.to_vec();
        let mut ids = Vec::with_capacity(acceptances.len());
        for acceptance in acceptances {
            if acceptance.body().target != target.id() {
                return Err(StoreError::WrongTarget(target.clone()));
            }
            let encoded = acceptance.encode();
            ids.push(AcceptanceId::from(encoded.id()));
            records.push(encoded);
        }
        // The acceptances are durable before the log refers to them.
        self.write_all(&records)?;

        let files = self.target_files(target.id());
        fs::create_dir_all(&files.dir).map_err(|source| StoreError::io(&files.dir, source))?;
        let log = LockedFile::open(&self.platform, &files.log(), LockMode::Exclusive)?;
        let latest = files.last_entry(&log)?;
        let head = files.read_head()?;
        let log_error = |source| StoreError::io(&files.log(), source);
        if head != latest {
            match latest {
                // A crash left the log ahead of the head. Its entries name
                // durable acceptances, but may not be durable themselves.
                Some(latest) if head.is_none_or(|head| head.seq < latest.seq) => {
                    self.platform.sync_file(&log).map_err(log_error)?;
                    let next = head.map_or(0, |head| head.seq + 1);
                    let advanced = files.entries_since(&log, next)?;
                    self.publish(&files, target, head.is_none(), &advanced)?;
                }
                _ => return Err(StoreError::Malformed(files.log())),
            }
        }
        let mut previous = latest;
        for (acceptance, &id) in acceptances.iter().zip(&ids) {
            let body = acceptance.body();
            let follows = match previous {
                None => body.seq == 0 && body.prev.is_none(),
                Some(previous) => {
                    body.seq == previous.seq + 1 && body.prev == Some(previous.acceptance)
                }
            };
            if !follows {
                return Err(StoreError::Conflict {
                    target: target.clone(),
                    expected: body.seq,
                    actual: latest,
                });
            }
            previous = Some(Head {
                seq: body.seq,
                acceptance: id,
            });
        }

        let mut lines = String::with_capacity(ids.len() * TargetFiles::LINE);
        for id in &ids {
            lines.push_str(&id.to_hex());
            lines.push('\n');
        }
        let mut writer = &*log;
        writer
            .seek(SeekFrom::Start(first.body().seq * TargetFiles::LINE as u64))
            .and_then(|_| writer.write_all(lines.as_bytes()))
            .map_err(log_error)?;
        self.platform.sync_file(&log).map_err(log_error)?;

        let advanced: Vec<Head> = acceptances
            .iter()
            .zip(&ids)
            .map(|(acceptance, &id)| Head {
                seq: acceptance.body().seq,
                acceptance: id,
            })
            .collect();
        self.publish(&files, target, latest.is_none(), &advanced)
    }

    /// Remembers the name of `target`, which this store doesn't follow, so
    /// that acceptances of it it holds, such as another target's landings,
    /// can be described. The target doesn't exist for this store until its
    /// first acceptance is appended.
    ///
    /// # Errors
    ///
    /// Fails if the name cannot be written.
    pub fn name_target(&self, target: &TargetName) -> Result<(), StoreError> {
        let files = self.target_files(target.id());
        if files.name().exists() {
            return Ok(());
        }
        fs::create_dir_all(&files.dir).map_err(|source| StoreError::io(&files.dir, source))?;
        let write = self.durable_write();
        write.stage(files.name(), target.as_str().as_bytes())?;
        write.commit()?;
        self.sync_dir(&self.targets_dir())
    }

    /// Durably replaces the head of a target with the last of `advanced`,
    /// the acceptances appended, logging that it advanced to each, which
    /// their events come from. Publishing its first head, `created`, which
    /// may name a later acceptance than the first, creates the target: its
    /// name is written too, and its directory made durable.
    fn publish(
        &self,
        files: &TargetFiles,
        target: &TargetName,
        created: bool,
        advanced: &[Head],
    ) -> Result<(), StoreError> {
        let Some(&head) = advanced.last() else {
            return Ok(());
        };
        let write = self.durable_write();
        let line = format!("{} {}\n", head.seq, head.acceptance);
        write.stage(files.head(), line.as_bytes())?;
        if created {
            write.stage(files.name(), target.as_str().as_bytes())?;
        }
        let advanced: Vec<_> = advanced
            .iter()
            .map(|advanced| Effect::Advanced {
                target: target.clone(),
                seq: advanced.seq,
                acceptance: advanced.acceptance,
            })
            .collect();
        self.commit_logged(write, &advanced)?;
        if created {
            self.sync_dir(&self.targets_dir())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::thread;

    use origin_platform::Native;
    use origin_shared::{AcceptanceBody, NodeKey, Tree, TreeId};

    use super::*;
    use crate::store::tests::{Recorder, Synced};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Native::default());
        store.init().unwrap();
        (dir, store)
    }

    fn key() -> NodeKey {
        NodeKey::from_bytes(&[42; 32])
    }

    /// An acceptance of `target` following `prev`.
    fn acceptance(target: &TargetName, prev: Option<Head>) -> Acceptance {
        AcceptanceBody {
            target: target.id(),
            seq: prev.map_or(0, |prev| prev.seq + 1),
            prev: prev.map(|prev| prev.acceptance),
            state: TreeId::from(Tree::default().encode().id()),
            landed: Vec::new(),
            evidence: Vec::new(),
            policy: None,
        }
        .sign(&key())
    }

    /// Appends `count` acceptances to `target`, returning their ids.
    fn land<P: Platform>(store: &Store<P>, target: &TargetName, count: usize) -> Vec<AcceptanceId> {
        (0..count)
            .map(|_| {
                let head = store.head(target).unwrap();
                store.append(target, &acceptance(target, head)).unwrap()
            })
            .collect()
    }

    #[test]
    fn appending_several_acceptances_creates_the_target() {
        let (_dir, store) = store();
        let main = TargetName::MAIN;
        let first = acceptance(&main, None);
        let second = acceptance(
            &main,
            Some(Head {
                seq: 0,
                acceptance: first.encode().id().into(),
            }),
        );
        store.append_all(&main, &[first, second], &[]).unwrap();
        assert_eq!(store.targets().unwrap(), [TargetName::MAIN]);
        assert_eq!(store.head(&main).unwrap().unwrap().seq, 1);
    }

    #[test]
    fn appends_form_a_chain() {
        let (_dir, store) = store();
        let main = TargetName::MAIN;
        assert_eq!(store.head(&main).unwrap(), None);
        assert_eq!(store.log(&main).unwrap(), Vec::<AcceptanceId>::new());

        let ids = land(&store, &main, 3);
        assert_eq!(
            store.head(&main).unwrap(),
            Some(Head {
                seq: 2,
                acceptance: ids[2]
            })
        );
        assert_eq!(store.log(&main).unwrap(), ids);
        for (seq, &id) in (0..).zip(&ids) {
            assert_eq!(store.acceptance_at(main.id(), seq).unwrap(), Some(id));
        }
        assert_eq!(store.acceptance_at(main.id(), 3).unwrap(), None);
        assert_eq!(store.log_since(&main, 1).unwrap(), ids[1..]);
        assert_eq!(store.log_since(&main, 3).unwrap(), []);
        let last = store.get(ids[2]).unwrap();
        assert_eq!(last.body().prev, Some(ids[1]));
        last.verify(&key().public()).unwrap();
        assert_eq!(store.targets().unwrap(), [main]);
    }

    #[test]
    fn appends_must_follow_the_latest_acceptance() {
        let (_dir, store) = store();
        let main = TargetName::MAIN;
        let ids = land(&store, &main, 2);
        let latest = store.head(&main).unwrap();

        let genesis = Head {
            seq: 0,
            acceptance: ids[0],
        };
        for stale in [None, Some(genesis)] {
            let error = store.append(&main, &acceptance(&main, stale)).unwrap_err();
            assert!(
                matches!(error, StoreError::Conflict { actual, .. } if actual == latest),
                "{error}"
            );
        }
        let error = store
            .append(&TargetName::POLICY, &acceptance(&main, None))
            .unwrap_err();
        assert!(matches!(error, StoreError::WrongTarget(_)), "{error}");
        assert_eq!(store.log(&main).unwrap(), ids, "nothing was appended");
    }

    #[test]
    fn concurrent_appends_serialize() {
        let (_dir, store) = store();
        let main = TargetName::MAIN;
        let landed = Mutex::new(Vec::new());
        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..3 {
                        // Retry on conflict, as a landing queue would.
                        loop {
                            let head = store.head(&main).unwrap();
                            match store.append(&main, &acceptance(&main, head)) {
                                Ok(id) => break landed.lock().unwrap().push(id),
                                Err(StoreError::Conflict { .. }) => {}
                                Err(error) => panic!("{error}"),
                            }
                        }
                    }
                });
            }
        });
        let log = store.log(&main).unwrap();
        assert_eq!(log.len(), 24);
        let mut landed = landed.into_inner().unwrap();
        landed.sort();
        let mut sorted = log.clone();
        sorted.sort();
        assert_eq!(sorted, landed, "every success is in the log once");
        for pair in log.windows(2) {
            assert_eq!(store.get(pair[1]).unwrap().body().prev, Some(pair[0]));
        }
    }

    #[test]
    fn appends_repair_what_a_crash_left() {
        let (_dir, store) = store();
        let main = TargetName::MAIN;
        let ids = land(&store, &main, 2);
        let files = store.target_files(main.id());

        // A crash after syncing a log entry, before publishing the head.
        let unpublished = acceptance(
            &main,
            Some(Head {
                seq: 1,
                acceptance: ids[1],
            }),
        );
        let unpublished_id = AcceptanceId::from(unpublished.encode().id());
        store.write_all(&[unpublished.encode()]).unwrap();
        let mut log = fs::OpenOptions::new()
            .append(true)
            .open(files.log())
            .unwrap();
        writeln!(log, "{unpublished_id}").unwrap();
        // Then a crash in the middle of writing the next line.
        log.write_all(b"0123").unwrap();
        assert_eq!(store.log(&main).unwrap(), ids, "readers go by the head");

        // Landing on the published head finds the target moved.
        let stale = acceptance(&main, store.head(&main).unwrap());
        let error = store.append(&main, &stale).unwrap_err();
        let repaired = Head {
            seq: 2,
            acceptance: unpublished_id,
        };
        assert!(
            matches!(error, StoreError::Conflict { actual, .. } if actual == Some(repaired)),
            "{error}"
        );
        assert_eq!(store.head(&main).unwrap(), Some(repaired));
        land(&store, &main, 1);
        assert_eq!(store.log(&main).unwrap().len(), 4);
    }

    #[test]
    fn repairs_advance_the_target_to_every_acceptance_left_unpublished() {
        let (_dir, store) = store();
        let main = TargetName::MAIN;
        let ids = land(&store, &main, 1);
        let before = store.oplog().last_seq().unwrap();
        // A crash after appending three acceptances to the log at once,
        // before publishing the head.
        let files = store.target_files(main.id());
        let mut log = fs::OpenOptions::new()
            .append(true)
            .open(files.log())
            .unwrap();
        let mut head = Head {
            seq: 0,
            acceptance: ids[0],
        };
        for _ in 0..3 {
            let next = acceptance(&main, Some(head)).encode();
            store.write_all(slice::from_ref(&next)).unwrap();
            head = Head {
                seq: head.seq + 1,
                acceptance: next.id().into(),
            };
            writeln!(log, "{}", head.acceptance).unwrap();
        }

        let stale = acceptance(&main, store.head(&main).unwrap());
        assert!(store.append(&main, &stale).is_err(), "repaired first");
        let advanced: Vec<_> = store
            .oplog()
            .after(before)
            .unwrap()
            .entries
            .into_iter()
            .filter_map(|entry| match entry.effect {
                Effect::Advanced { seq, .. } => Some(seq),
                _ => None,
            })
            .collect();
        assert_eq!(advanced, [1, 2, 3], "one event per acceptance");
        assert_eq!(store.head(&main).unwrap(), Some(head));
    }

    #[test]
    fn repairs_sync_the_log_before_publishing_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Recorder::default());
        store.init().unwrap();
        let main = TargetName::MAIN;
        let ids = land(&store, &main, 1);
        // A crash after appending a log entry, before syncing it.
        let unsynced = acceptance(
            &main,
            Some(Head {
                seq: 0,
                acceptance: ids[0],
            }),
        );
        store.write_all(&[unsynced.encode()]).unwrap();
        let log = store.target_files(main.id()).log();
        let mut file = fs::OpenOptions::new().append(true).open(log).unwrap();
        writeln!(file, "{}", unsynced.encode().id()).unwrap();
        store.platform.0.lock().unwrap().clear();

        assert!(store.append(&main, &unsynced).is_err(), "it's there now");
        let targets = store.targets_dir();
        let target = targets.join(main.id().to_hex());
        let shard = &unsynced.encode().id().to_hex()[..2];
        assert_eq!(
            store.platform.0.into_inner().unwrap(),
            [
                Synced::Batch(0),
                Synced::Dir(dir.path().join(".origin/objects/loose").join(shard)),
                Synced::File,
                // The head, and the op log entry saying it advanced.
                Synced::Batch(2),
                Synced::Dir(targets),
                Synced::Dir(target),
            ],
            "the log, then the head, with its op log entry"
        );
    }

    #[test]
    fn acceptances_are_durable_before_the_log_refers_to_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join(".origin"), Recorder::default());
        store.init().unwrap();
        let main = TargetName::MAIN;
        land(&store, &main, 1);
        store.platform.0.lock().unwrap().clear();

        let id = land(&store, &main, 1)[0];
        let hex = id.to_hex();
        let targets = store.targets_dir();
        let target = targets.join(main.id().to_hex());
        assert_eq!(
            store.platform.0.into_inner().unwrap(),
            [
                // The acceptance and its op log entry, which costs no sync
                // of its own.
                Synced::Batch(2),
                Synced::Dir(dir.path().join(".origin/objects/loose")),
                Synced::Dir(dir.path().join(".origin/objects/loose").join(&hex[..2])),
                Synced::File,
                // The head, and the op log entry saying it advanced.
                Synced::Batch(2),
                Synced::Dir(targets),
                Synced::Dir(target),
            ],
            "the acceptance and its shard, then the log, then the head"
        );
    }
}
