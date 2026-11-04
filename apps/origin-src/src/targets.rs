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

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::slice;

use origin_platform::{DurableWrite, LockMode, LockedFile, Platform};
use origin_shared::{Acceptance, AcceptanceId, Record, TargetId, TargetName};

use crate::store::{Store, StoreError};

/// The latest acceptance of a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// Its position in the log, from 0.
    pub seq: u64,
    /// The acceptance.
    pub acceptance: AcceptanceId,
}

impl Head {
    /// Describes where a target is, for error messages.
    pub(crate) fn describe(head: Option<Self>) -> String {
        head.map_or_else(|| "nothing".to_owned(), |head| head.to_string())
    }

    fn parse(line: &str) -> Option<Self> {
        let (seq, acceptance) = line.strip_suffix('\n')?.split_once(' ')?;
        Some(Self {
            seq: seq.parse().ok()?,
            acceptance: acceptance.parse().ok()?,
        })
    }
}

impl fmt::Display for Head {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "acceptance {} ({})", self.seq, self.acceptance.short())
    }
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
            .and_then(Head::parse)
            .map(Some)
            .ok_or(StoreError::Malformed(path))
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
        let files = self.target_files(target.id());
        let Some(head) = files.read_head()? else {
            return Ok(Vec::new());
        };
        let bytes = TargetFiles::read(&files.log())?.unwrap_or_default();
        let mut log = files.parse_log(&bytes)?;
        // The log can be ahead of the head while an append is in progress.
        let published = usize::try_from(head.seq + 1).unwrap_or(usize::MAX);
        if log.len() < published || log[published - 1] != head.acceptance {
            return Err(StoreError::Malformed(files.log()));
        }
        log.truncate(published);
        Ok(log)
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
    /// with its first acceptance.
    ///
    /// This is a compare-and-swap: the acceptance must follow the target's
    /// latest one, with the next sequence number and naming it as `prev`.
    /// Concurrent appends are serialized, and all but one of those following
    /// the same acceptance fail. Verifying the signature is the caller's job,
    /// since only it knows the target's authority.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Conflict`] if the acceptance doesn't follow
    /// the latest one, [`StoreError::WrongTarget`] if it belongs to another
    /// target, or if the target's files cannot be locked, read or written.
    pub fn append(
        &self,
        target: &TargetName,
        acceptance: &Acceptance,
    ) -> Result<AcceptanceId, StoreError> {
        let body = acceptance.body();
        if body.target != target.id() {
            return Err(StoreError::WrongTarget(target.clone()));
        }
        let encoded = acceptance.encode();
        // The acceptance is durable before the log refers to it.
        self.write_all(slice::from_ref(&encoded))?;
        let id = AcceptanceId::from(encoded.id());

        let files = self.target_files(body.target);
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
                    self.publish(&files, target, latest)?;
                }
                _ => return Err(StoreError::Malformed(files.log())),
            }
        }
        let follows = match latest {
            None => body.seq == 0 && body.prev.is_none(),
            Some(latest) => body.seq == latest.seq + 1 && body.prev == Some(latest.acceptance),
        };
        if !follows {
            return Err(StoreError::Conflict {
                target: target.clone(),
                expected: body.seq,
                actual: latest,
            });
        }

        let mut writer = &*log;
        writer
            .seek(SeekFrom::Start(body.seq * TargetFiles::LINE as u64))
            .and_then(|_| writeln!(writer, "{id}"))
            .map_err(log_error)?;
        self.platform.sync_file(&log).map_err(log_error)?;

        let head = Head {
            seq: body.seq,
            acceptance: id,
        };
        self.publish(&files, target, head)?;
        Ok(id)
    }

    /// Durably replaces the head of a target. Publishing the first
    /// acceptance creates the target: its name is written too, and its
    /// directory made durable.
    fn publish(
        &self,
        files: &TargetFiles,
        target: &TargetName,
        head: Head,
    ) -> Result<(), StoreError> {
        let write = self.durable_write();
        let line = format!("{} {}\n", head.seq, head.acceptance);
        write.stage(files.head(), line.as_bytes())?;
        let created = head.seq == 0;
        if created {
            write.stage(files.name(), target.as_str().as_bytes())?;
        }
        write.commit()?;
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
                Synced::Batch(1),
                Synced::Dir(targets),
                Synced::Dir(target),
            ],
            "the log, then the head"
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
                // The acceptance and the index log, which costs no sync of
                // its own.
                Synced::Batch(2),
                Synced::Dir(dir.path().join(".origin/objects/loose")),
                Synced::Dir(dir.path().join(".origin/objects/loose").join(&hex[..2])),
                Synced::File,
                Synced::Batch(1),
                Synced::Dir(targets),
                Synced::Dir(target),
            ],
            "the acceptance and its shard, then the log, then the head"
        );
    }
}
