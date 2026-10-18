//! Worker mode: answering a remote's `check.requested` events, which its
//! landing queue emits for each `ci.*` statement a waiting landing lacks.
//!
//! The worker long-polls the events and runs each check requested, so the
//! statement issued lets the landing continue. It runs the check on the
//! landing's subject as defined by the `checks.toml` at the tip of the
//! target it lands on, never the candidate's own: the statements of a
//! trusted runner satisfy the target's requirements, so a change that
//! redefines a required check, to pass trivially, must not make the worker
//! say it passed. A target without a tip or a `checks.toml` there leaves
//! the request unanswered, until the worker reconciles. The events are only a
//! prompt: one can come while the worker is down, and a check can fail to
//! run. So the worker also reconciles: when it starts, once the remote can
//! be reached again, and every [`Worker::RECONCILE`], it lists the landings
//! that wait, which the remote enumerates, and answers every check they
//! lack that it hasn't answered yet. Its cursor then moves to the end of
//! the events, read first, so none after the landings listed is missed.
//! While the remote can't be reached, it waits and tries again, then
//! reconciles, so a check the remote cut short by going away runs again.
//! Events come at least once, and a check that already has a result is
//! skipped, so nothing runs twice.

use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};

use origin_platform::LockedFile;
use origin_shared::api::{At, ErrorCode, EventsQuery, List};
use origin_shared::event::{EventKind, Page};
use origin_shared::landing::{Blocked, Pending};
use origin_shared::policy::{KindPattern, Requirement};
use origin_shared::{Kind, Period, RecordId, TargetName};

use crate::checks::Checks;
use origin_src::remote::RemoteError;

use crate::runner::{Outcome, RunError, Runner};
use crate::state::{State, StateError};

/// A worker running the checks a remote requests.
#[derive(Debug)]
pub struct Worker {
    runner: Runner,
    /// The cursor to read after: `None` until the waiting landings were
    /// answered.
    cursor: Option<u64>,
    /// When the waiting landings were last answered.
    reconciled: Instant,
    /// The checks answered, by subject and kind, of the landings that
    /// still wait: reconciling skips them.
    answered: HashSet<(RecordId, Kind)>,
    _lock: LockedFile,
}

impl Worker {
    /// How long each long-poll waits.
    pub const WAIT: Duration = Duration::from_secs(30);

    /// How often the worker answers every landing that waits, whatever the
    /// events said.
    pub const RECONCILE: Duration = Duration::from_mins(1);

    /// How long the worker first waits for a remote it can't reach, which
    /// doubles each time, up to [`Worker::MAX_RETRY`].
    const FIRST_RETRY: Duration = Duration::from_millis(250);

    /// The longest the worker waits before trying a remote again.
    const MAX_RETRY: Duration = Duration::from_secs(8);

    /// A worker for `runner`'s remote, which answers every landing that
    /// waits first.
    ///
    /// # Errors
    ///
    /// Fails if another worker uses `state`.
    pub fn new(runner: Runner, state: &State) -> Result<Self, WorkerError> {
        let lock = state
            .lock_worker()?
            .ok_or_else(|| WorkerError::Busy(state.dir().display().to_string()))?;
        Ok(Self {
            runner,
            cursor: None,
            reconciled: Instant::now(),
            answered: HashSet::new(),
            _lock: lock,
        })
    }

    /// Answers requests until stopped, printing each outcome, and each
    /// check that cannot run, which is tried again when the worker next
    /// reconciles.
    ///
    /// # Errors
    ///
    /// Fails if the remote refuses.
    pub fn run(&mut self) -> Result<(), WorkerError> {
        loop {
            for Answer {
                subject,
                kind,
                result,
            } in self.next(Self::WAIT)?
            {
                match result {
                    Ok(outcome) => println!("{outcome}, about {subject}"),
                    Err(error) => eprintln!(
                        "cannot check {subject} for {kind}, which waits until the worker \
                         reconciles: {:#}",
                        anyhow::Error::from(error)
                    ),
                }
            }
        }
    }

    /// Answers requests like [`Worker::poll`], waiting for the remote as
    /// long as it can't be reached, then reconciling: a check may have
    /// been cut short meanwhile.
    ///
    /// # Errors
    ///
    /// Fails like [`Worker::poll`], but for a remote that can't be reached.
    pub fn next(&mut self, wait: Duration) -> Result<Vec<Answer>, WorkerError> {
        let mut retry = Self::FIRST_RETRY;
        loop {
            match self.poll(wait) {
                Err(WorkerError::Remote(error)) if error.is_unreachable() => {
                    eprintln!(
                        "Cannot reach the remote, trying again in {:.2}s: {:#}",
                        retry.as_secs_f64(),
                        anyhow::Error::from(error)
                    );
                    thread::sleep(retry);
                    retry = (retry * 2).min(Self::MAX_RETRY);
                    self.cursor = None;
                }
                answers => return answers,
            }
        }
    }

    /// Waits up to `wait` for requests, and answers them; or reconciles
    /// instead, without a cursor, or once [`Worker::RECONCILE`] passed.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached or refuses.
    pub fn poll(&mut self, wait: Duration) -> Result<Vec<Answer>, WorkerError> {
        let cursor = match self.cursor {
            Some(cursor) if self.reconciled.elapsed() < Self::RECONCILE => cursor,
            _ => return self.reconcile(),
        };
        let page = match self.events(Some(cursor), Some(wait)) {
            Ok(page) => page,
            Err(error) if error.code() == Some(ErrorCode::Expired) => {
                eprintln!("The remote no longer keeps the events after the cursor: reconciling");
                return self.reconcile();
            }
            Err(error) => return Err(error.into()),
        };
        let Page { events, next, .. } = page;
        let mut answered = Vec::new();
        for event in events {
            if let EventKind::CheckRequested {
                onto,
                subject,
                requirement,
            } = &event.kind
            {
                answered.extend(self.answer(*subject, onto, requirement)?);
            }
        }
        self.cursor = Some(next);
        Ok(answered)
    }

    /// Answers every check a landing that waits lacks, but those answered
    /// already, then moves the cursor to the end of the events, read first.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached or refuses.
    pub fn reconcile(&mut self) -> Result<Vec<Answer>, WorkerError> {
        let end = self.events(None, None)?.next;
        let mut lacking = Vec::new();
        for Pending {
            onto,
            subject,
            blocked,
            ..
        } in self.runner.remote().landings()?
        {
            if let Blocked::Evidence(missing) = blocked {
                lacking.extend(
                    missing
                        .into_iter()
                        .map(|requirement| (subject, onto.clone(), requirement)),
                );
            }
        }
        // What no longer waits needs no answer.
        self.answered.retain(|(subject, kind)| {
            lacking
                .iter()
                .any(|(lacks, _, requirement)| lacks == subject && requirement.kind == *kind)
        });
        let mut answered = Vec::new();
        for (subject, onto, requirement) in &lacking {
            if !self
                .answered
                .contains(&(*subject, requirement.kind.clone()))
            {
                answered.extend(self.answer(*subject, onto, requirement)?);
            }
        }
        self.cursor = Some(end);
        self.reconciled = Instant::now();
        Ok(answered)
    }

    /// The `check.requested` events after `cursor`, or from now, waiting up
    /// to `wait` for some.
    fn events(&self, cursor: Option<u64>, wait: Option<Duration>) -> Result<Page, RemoteError> {
        self.runner.remote().events(&EventsQuery {
            cursor,
            wait: wait.map(Period),
            kind: List(vec![KindPattern::Exact(
                Kind::new("check.requested").expect("a valid kind"),
            )]),
            ..EventsQuery::default()
        })
    }

    /// Runs the check `requirement` asks for about `subject`, landing
    /// `onto` a target, if it's one of `ci.*`, as the target's tip defines
    /// it, remembering it once it ran or was covered.
    ///
    /// # Errors
    ///
    /// Fails if the remote can't be reached, which cut the check short.
    fn answer(
        &mut self,
        subject: RecordId,
        onto: &TargetName,
        requirement: &Requirement,
    ) -> Result<Option<Answer>, WorkerError> {
        let namespace = KindPattern::Prefix(Kind::new(Checks::NAMESPACE).expect("a valid kind"));
        if !namespace.matches(&requirement.kind) {
            return Ok(None);
        }
        let kind = requirement.kind.clone();
        let from = At::Target(onto.clone());
        let result = self
            .runner
            .check(subject, &from, Some(&kind))
            .and_then(|outcomes| {
                outcomes
                    .into_iter()
                    .next()
                    .ok_or_else(|| RunError::UnknownCheck {
                        at: from.clone(),
                        kind: kind.clone(),
                    })
            });
        match result {
            Err(RunError::Remote(error)) if error.is_unreachable() => return Err(error.into()),
            Ok(_) => {
                self.answered.insert((subject, kind.clone()));
            }
            Err(_) => {}
        }
        Ok(Some(Answer {
            subject,
            kind,
            result,
        }))
    }
}

/// How a request for a check went.
#[derive(Debug)]
pub struct Answer {
    /// The record to check.
    pub subject: RecordId,
    /// The kind of statement requested.
    pub kind: Kind,
    /// What became of the check, or why it cannot run, so it's tried again
    /// when the worker next reconciles.
    pub result: Result<Outcome, RunError>,
}

/// A failure of the worker.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// Another worker uses the state directory.
    #[error("another worker answers this remote's requests, with {0}")]
    Busy(String),
    /// The worker's lock cannot be taken.
    #[error(transparent)]
    State(#[from] StateError),
    /// The remote cannot be reached or refused.
    #[error(transparent)]
    Remote(#[from] RemoteError),
}
