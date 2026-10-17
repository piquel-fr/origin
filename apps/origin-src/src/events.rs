//! Events: what happened on this node, for plugins and agents to react to.
//! They are derived from the [op log](crate::oplog), the records its
//! entries name and the queues' waiting landings, so nothing is stored
//! twice:
//!
//! - `statement.issued`, for every statement written, including conflicts,
//!   divergence and promotions: its subject, kind, verdict and issuer;
//! - `revision.created`, for every revision written;
//! - `target.advanced`, for every acceptance appended to a target;
//! - `check.requested`, for each statement a waiting landing lacks.
//!
//! An event's cursor is the sequence number of the op log entry it comes
//! from, which never changes, across restarts and index rebuilds alike.
//! Reading the events after a cursor returns the next cursor to read from.
//! Events are delivered at least once: an entry with several events is
//! delivered whole, and reading from an earlier cursor delivers its events
//! again, so consumers deduplicate by [`EventKind::record`].
//!
//! Events are delivered in the op log's order. An entry whose write is in
//! progress holds back the events after it until the write is published.
//! One whose writer crashed delivers what was published (see
//! [`OpLog::settled`](crate::oplog::OpLog::settled)). A cursor is never one
//! a power loss could take back, so none is reused. Entries lost to damage
//! deliver nothing, and the damage is reported on stderr.

use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use origin_platform::{Native, Platform};
use origin_shared::event::{Event, EventKind, Filter, Page};
use origin_shared::landing::{Blocked, Pending};
use origin_shared::{Object, ObjectId};

use crate::index;
use crate::oplog::{Effect, Entry};
use crate::queue::Queue;
use crate::store::{Store, StoreError};

/// The events of a store.
#[derive(Debug)]
pub struct Events<'a, P = Native> {
    store: &'a Store<P>,
    /// What [`Events::wait`] waits on between reads, if not just the time.
    wakeup: Option<&'a Wakeup>,
}

/// Wakes the readers waiting for events in this process as soon as it
/// writes, rather than at their next read of the op log. Writes from other
/// processes still wait for that read.
#[derive(Debug, Default)]
pub struct Wakeup {
    /// How many writes this process made.
    writes: Mutex<u64>,
    written: Condvar,
}

impl Wakeup {
    /// Wakes every reader waiting: this process wrote.
    pub fn notify(&self) {
        *self.writes.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        self.written.notify_all();
    }

    /// How many writes this process made so far.
    fn writes(&self) -> u64 {
        *self.writes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits up to `timeout`, or until a write after the first `seen`.
    fn sleep(&self, seen: u64, timeout: Duration) {
        let writes = self.writes.lock().unwrap_or_else(PoisonError::into_inner);
        let _woken = self
            .written
            .wait_timeout_while(writes, timeout, |writes| *writes == seen)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

impl<'a, P: Platform> Events<'a, P> {
    /// How often [`Events::wait`] reads the op log.
    pub const POLL: Duration = Duration::from_millis(50);

    /// The events of `store`.
    pub fn new(store: &'a Store<P>) -> Self {
        Self {
            store,
            wakeup: None,
        }
    }

    /// The events, with [`Events::wait`] also woken by `wakeup`.
    #[must_use]
    pub fn waking_on(self, wakeup: &'a Wakeup) -> Self {
        Self {
            wakeup: Some(wakeup),
            ..self
        }
    }

    /// The cursor after every event so far: reading from it delivers only
    /// later ones.
    ///
    /// # Errors
    ///
    /// Fails if the op log cannot be read.
    pub fn end(&self) -> Result<u64, StoreError> {
        self.store.oplog().settled()
    }

    /// The events after `cursor` that pass `filter`, oldest first: those
    /// of whole entries, until there are `limit` or more.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Expired`] if the op log no longer keeps the
    /// entries after `cursor`, or if it or a record cannot be read.
    pub fn after(&self, cursor: u64, filter: &Filter, limit: usize) -> Result<Page, StoreError> {
        let mut page = Page {
            events: Vec::new(),
            next: cursor,
        };
        let oplog = self.store.oplog();
        let read = oplog.after(cursor)?;
        if read.entries.is_empty() {
            return Ok(page);
        }
        let settled = oplog.settled()?;
        let mut waiting = None;
        for entry in read.entries.iter().take_while(|entry| entry.seq <= settled) {
            page.events.extend(
                self.derive(entry, &mut waiting)?
                    .into_iter()
                    .filter(|kind| filter.matches(kind))
                    .map(|kind| Event {
                        cursor: entry.seq,
                        kind,
                    }),
            );
            page.next = entry.seq;
            if page.events.len() >= limit {
                break;
            }
        }
        Ok(page)
    }

    /// Like [`Events::after`], but waits up to `timeout` for events, reading
    /// the op log every [`Events::POLL`], and whenever the
    /// [`Wakeup`] says this process wrote. The page is empty if none came.
    ///
    /// # Errors
    ///
    /// Fails like [`Events::after`].
    pub fn wait(
        &self,
        cursor: u64,
        filter: &Filter,
        limit: usize,
        timeout: Duration,
    ) -> Result<Page, StoreError> {
        let start = Instant::now();
        let mut cursor = cursor;
        loop {
            // Read before the op log, so no write after it is missed.
            let seen = self.wakeup.map(Wakeup::writes);
            let page = self.after(cursor, filter, limit)?;
            let left = timeout.saturating_sub(start.elapsed());
            if !page.events.is_empty() || left.is_zero() {
                return Ok(page);
            }
            // Entries with no matching events are read past.
            cursor = page.next;
            let pause = Self::POLL.min(left);
            match (self.wakeup, seen) {
                (Some(wakeup), Some(seen)) => wakeup.sleep(seen, pause),
                _ => thread::sleep(pause),
            }
        }
    }

    /// The events of a settled entry: of what its write published. The
    /// waiting landings are read once, the first time an entry needs them.
    fn derive(
        &self,
        entry: &Entry,
        waiting: &mut Option<Vec<Pending>>,
    ) -> Result<Vec<EventKind>, StoreError> {
        Ok(match &entry.effect {
            Effect::Wrote(records) => self.wrote(records)?,
            Effect::Waiting { onto, proposal } => {
                let waiting = match waiting {
                    Some(waiting) => waiting,
                    None => waiting.insert(Queue::pending(self.store)?),
                };
                // A landing no longer waiting landed since, or was never
                // kept, if its writer crashed: either way there's nothing
                // to check.
                let pending = waiting
                    .iter()
                    .find(|pending| pending.onto == *onto && pending.proposal == *proposal);
                match pending {
                    Some(Pending {
                        subject,
                        blocked: Blocked::Evidence(missing),
                        ..
                    }) => missing
                        .iter()
                        .map(|requirement| EventKind::CheckRequested {
                            onto: onto.clone(),
                            subject: *subject,
                            requirement: requirement.clone(),
                        })
                        .collect(),
                    _ => Vec::new(),
                }
            }
            Effect::Moved(_) => Vec::new(),
            // A crash may have cut the move short.
            &Effect::Advanced {
                ref target,
                seq,
                acceptance,
            } => {
                let landed = self.store.acceptance_at(target.id(), seq)? == Some(acceptance);
                landed
                    .then(|| EventKind::TargetAdvanced {
                        target: target.clone(),
                        seq,
                        acceptance,
                    })
                    .into_iter()
                    .collect()
            }
        })
    }

    /// The events of the records written that are published.
    fn wrote(&self, records: &[index::Entry]) -> Result<Vec<EventKind>, StoreError> {
        let mut events = Vec::new();
        for record in records {
            let id = ObjectId::from(record.record());
            let Some(object) = self.store.try_read(id)? else {
                continue;
            };
            let object = object
                .decode()
                .map_err(|source| StoreError::Decode { id, source })?;
            events.extend(match object {
                Object::Statement(statement) => Some(EventKind::StatementIssued {
                    statement: id.into(),
                    subject: statement.subject,
                    kind: statement.kind,
                    verdict: statement.verdict,
                    issuer: statement.issuer,
                }),
                Object::Revision(revision) => Some(EventKind::RevisionCreated {
                    revision: id.into(),
                    change: revision.change,
                    author: revision.author,
                }),
                // Its event comes once a target advances to it.
                Object::Acceptance(_) | Object::Blob(_) | Object::Tree(_) => None,
            });
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use origin_shared::event::Subject;
    use origin_shared::{Base, ChangeId, ObjectId, RecordId, TargetName};

    use super::*;
    use crate::annotations::Annotations;
    use crate::index::Index;
    use crate::queue::tests::{Repo, ci_test};

    /// A repository where landing on `main` requires a `ci.test` pass from
    /// `node:ci`.
    fn repo() -> Repo {
        Repo::new(
            "[groups]\nrunners = [\"node:ci\"]\n[targets.main]\nauthority = \"node:queue\"\n\
             require = [{ kind = \"ci.test\", verdict = \"pass\", issuer = \"group:runners\" }]\n",
        )
    }

    /// Every event after `cursor`, read in pages of one entry or more.
    fn all(events: &Events<'_>, mut cursor: u64, filter: &Filter) -> Vec<Event> {
        let mut all = Vec::new();
        loop {
            let page = events.after(cursor, filter, 1).unwrap();
            if page.next == cursor {
                return all;
            }
            all.extend(page.events);
            cursor = page.next;
        }
    }

    /// The name of each event, and the statement kind it issues or
    /// requests.
    fn summary(events: &[Event]) -> Vec<(&'static str, Option<String>)> {
        events
            .iter()
            .map(|event| {
                let kind = match &event.kind {
                    EventKind::StatementIssued { kind, .. } => Some(kind.to_string()),
                    EventKind::CheckRequested { requirement, .. } => {
                        Some(requirement.kind.to_string())
                    }
                    _ => None,
                };
                (event.kind.name(), kind)
            })
            .collect()
    }

    #[test]
    fn every_statement_revision_landing_and_request_fires_an_event() {
        let repo = repo();
        let events = Events::new(&repo.store);
        let start = events.end().unwrap();

        let code = repo.change_on_main(1, &[("src/lib.rs", "a")]);
        let docs = repo.change_on_main(2, &[("docs/guide.md", "# Guide")]);
        let clash = repo.change_on_main(3, &[("src/lib.rs", "b")]);
        repo.land(1);
        let requested = all(&events, start, &Filter::default());
        assert_eq!(
            summary(&requested).last(),
            Some(&("check.requested", Some("ci.test".to_owned())))
        );
        let annotations = Annotations::open(&repo.store).unwrap();
        annotations.issue(&ci_test(code, "node:ci"), None).unwrap();
        repo.land(1);
        // Rebased onto the tested code, the docs change gets its result
        // promoted; the other change conflicts.
        repo.land(2);
        repo.land(3);
        // Two revisions of the docs change from the same one: divergence.
        let base = Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance);
        repo.revision(2, &[docs], base, &[("docs/guide.md", "left")]);
        repo.revision(2, &[docs], base, &[("docs/guide.md", "right")]);
        Annotations::open(&repo.store)
            .unwrap()
            .flag_divergence(ChangeId::from_bytes([2; 16]), "node:queue".parse().unwrap())
            .unwrap();

        let all = all(&events, start, &Filter::default());
        let summary = summary(&all);
        let named = |name, kind: Option<&str>| {
            summary
                .iter()
                .filter(|(n, k)| *n == name && k.as_deref() == kind)
                .count()
        };
        assert_eq!(named("revision.created", None), 6, "{summary:?}");
        assert_eq!(
            named("check.requested", Some("ci.test")),
            0,
            "the landing is no longer waiting, so nothing is left to check"
        );
        assert_eq!(
            named("statement.issued", Some("ci.test")),
            2,
            "issued and promoted"
        );
        assert_eq!(named("statement.issued", Some("core.conflict")), 1);
        assert_eq!(named("statement.issued", Some("core.divergent")), 1);
        assert_eq!(named("target.advanced", None), 2);
        assert!(all.windows(2).all(|pair| pair[0].cursor <= pair[1].cursor));
        let conflict = all
            .iter()
            .find(|event| summary_kind(event) == Some("core.conflict"))
            .unwrap();
        assert_eq!(
            conflict.kind.subject(),
            Subject::Record(RecordId::Revision(clash))
        );
    }

    fn summary_kind(event: &Event) -> Option<&str> {
        match &event.kind {
            EventKind::StatementIssued { kind, .. } => Some(kind.as_str()),
            _ => None,
        }
    }

    #[test]
    fn cursors_survive_index_rebuilds_and_filters_select() {
        let repo = repo();
        let events = Events::new(&repo.store);
        let code = repo.change_on_main(1, &[("src/lib.rs", "a")]);
        Annotations::open(&repo.store)
            .unwrap()
            .issue(&ci_test(code, "node:ci"), None)
            .unwrap();
        let before = all(&events, 0, &Filter::default());
        fs::remove_dir_all(repo.store.root().join("index")).unwrap();
        Index::rebuild(&repo.store).unwrap();
        assert_eq!(all(&events, 0, &Filter::default()), before);

        let kinds = |pattern: &str| Filter {
            kinds: vec![pattern.parse().unwrap()],
            ..Filter::default()
        };
        assert_eq!(
            summary(&all(&events, 0, &kinds("ci.*"))),
            [("statement.issued", Some("ci.test".to_owned()))]
        );
        assert_eq!(all(&events, 0, &kinds("target.advanced")).len(), 2);
        let about = |start: String| Filter {
            subjects: vec![start],
            ..Filter::default()
        };
        let code_hex = code.to_hex();
        assert_eq!(all(&events, 0, &about(code_hex[..8].to_owned())).len(), 1);
        assert_eq!(
            all(&events, 0, &about(format!("revision:{}", &code_hex[..8]))).len(),
            1
        );
        assert_eq!(all(&events, 0, &about("main".to_owned())).len(), 1);
    }

    #[test]
    fn unpublished_entries_hold_back_later_events_while_writes_are_in_progress() {
        let repo = repo();
        let events = Events::new(&repo.store);
        // An entry for a revision that was never published, as a crashed
        // write leaves.
        let missing = index::Entry::Revision {
            id: ObjectId::from_bytes([9; 32]).into(),
            change: ChangeId::from_bytes([9; 16]),
            predecessors: Vec::new(),
        };
        let (crashed, writing) = repo
            .store
            .oplog()
            .append_in_progress(&Effect::Wrote(vec![missing]))
            .unwrap();
        repo.change_on_main(1, &[("a", "a")]);

        let held = events
            .after(crashed - 1, &Filter::default(), usize::MAX)
            .unwrap();
        assert_eq!(
            held,
            Page {
                events: Vec::new(),
                next: crashed - 1
            },
            "it may be in progress"
        );
        assert_eq!(events.end().unwrap(), crashed - 1, "no cursor passes it");
        // Its writer crashes.
        drop(writing);
        let page = events
            .after(crashed - 1, &Filter::default(), usize::MAX)
            .unwrap();
        assert_eq!(summary(&page.events), [("revision.created", None)]);
        assert!(page.next > crashed);
    }

    #[test]
    fn a_landing_waiting_again_after_a_conflict_requests_its_checks() {
        let repo = repo();
        let events = Events::new(&repo.store);
        let code = repo.change_on_main(1, &[("src/lib.rs", "a")]);
        let clash = repo.change_on_main(2, &[("src/lib.rs", "b")]);
        Annotations::open(&repo.store)
            .unwrap()
            .issue(&ci_test(code, "node:ci"), None)
            .unwrap();
        repo.land(1);
        repo.land(2);
        let start = events.end().unwrap();
        // While the landing waits again, its entry is in the log before its
        // file replaces the conflict's: readers wait for both.
        let again = Effect::Waiting {
            onto: TargetName::MAIN,
            proposal: origin_shared::landing::Proposal::Change(ChangeId::from_bytes([2; 16])),
        };
        let (_, writing) = repo.store.oplog().append_in_progress(&again).unwrap();
        assert_eq!(
            events.after(start, &Filter::default(), 10).unwrap().next,
            start
        );
        drop(writing);

        let base = Base::Acceptance(repo.tip(&TargetName::MAIN).acceptance);
        repo.revision(2, &[clash], base, &[("src/lib.rs", "a and b")]);
        repo.land(2);
        let requested = all(&events, start, &Filter::default());
        assert_eq!(
            summary(&requested).last(),
            Some(&("check.requested", Some("ci.test".to_owned()))),
            "{requested:?}"
        );
    }

    #[test]
    fn waiting_returns_as_soon_as_an_event_arrives() {
        let repo = repo();
        let events = Events::new(&repo.store);
        let end = events.end().unwrap();
        let empty = events
            .wait(end, &Filter::default(), 10, Events::<Native>::POLL)
            .unwrap();
        assert_eq!(
            empty,
            Page {
                events: Vec::new(),
                next: end
            }
        );
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Events::<Native>::POLL * 2);
                repo.change_on_main(1, &[("a", "a")]);
            });
            let page = events
                .wait(end, &Filter::default(), 10, Duration::from_secs(10))
                .unwrap();
            assert_eq!(summary(&page.events), [("revision.created", None)]);
        });
    }
}
