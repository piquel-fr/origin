//! Worker mode: answering a remote's `check.requested` events, which its
//! landing queue emits for each `ci.*` statement a waiting landing lacks.
//!
//! The worker long-polls the events and runs each check requested, so the
//! statement issued lets the landing continue. After each page it durably
//! saves the cursor in its [state](crate::state), and resumes after it when
//! restarted. Without a cursor, or once the remote no longer keeps the
//! events after it, it first answers every landing that waits, then reads
//! the events from then on. Events come at least once, and a check that
//! already has a result is skipped, so nothing runs twice.

use std::time::Duration;

use origin_platform::LockedFile;
use origin_shared::api::{ErrorCode, EventsQuery, List};
use origin_shared::event::{EventKind, Page};
use origin_shared::landing::{Blocked, Pending};
use origin_shared::policy::{KindPattern, Requirement};
use origin_shared::{Kind, Period, RecordId};

use crate::checks::Checks;
use origin_src::remote::RemoteError;

use crate::runner::{Outcome, RunError, Runner};
use crate::state::{State, StateError};

/// A worker running the checks a remote requests.
#[derive(Debug)]
pub struct Worker {
    runner: Runner,
    state: State,
    /// The cursor to read after: `None` until the waiting landings were
    /// answered.
    cursor: Option<u64>,
    _lock: LockedFile,
}

impl Worker {
    /// How long each long-poll waits.
    pub const WAIT: Duration = Duration::from_secs(30);

    /// A worker for `runner`'s remote, resuming after the cursor saved in
    /// `state`.
    ///
    /// # Errors
    ///
    /// Fails if another worker uses `state`, or the cursor cannot be read.
    pub fn new(runner: Runner, state: State) -> Result<Self, WorkerError> {
        let lock = state
            .lock_worker()?
            .ok_or_else(|| WorkerError::Busy(state.dir().display().to_string()))?;
        Ok(Self {
            cursor: state.cursor()?,
            runner,
            state,
            _lock: lock,
        })
    }

    /// Answers requests until stopped, printing each outcome, and each
    /// check that cannot run, which waits until it's requested again.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached, or the cursor cannot be
    /// saved.
    pub fn run(&mut self) -> Result<(), WorkerError> {
        loop {
            for Answer {
                subject,
                kind,
                result,
            } in self.poll(Self::WAIT)?
            {
                match result {
                    Ok(outcome) => println!("{outcome}, about {subject}"),
                    Err(error) => eprintln!(
                        "cannot check {subject} for {kind}: {:#}",
                        anyhow::Error::from(error)
                    ),
                }
            }
        }
    }

    /// Waits up to `wait` for requests, and answers them; or without a
    /// cursor, answers every landing that waits at once instead.
    ///
    /// # Errors
    ///
    /// Fails like [`Worker::run`].
    pub fn poll(&mut self, wait: Duration) -> Result<Vec<Answer>, WorkerError> {
        let Some(cursor) = self.cursor else {
            return self.catch_up();
        };
        let page = match self.events(Some(cursor), Some(wait)) {
            Ok(page) => page,
            Err(error) if error.code() == Some(ErrorCode::Expired) => {
                eprintln!("The remote no longer keeps the events after the cursor: catching up");
                return self.catch_up();
            }
            Err(error) => return Err(error.into()),
        };
        let Page { events, next } = page;
        let mut answered = Vec::new();
        for event in events {
            if let EventKind::CheckRequested {
                subject,
                requirement,
                ..
            } = &event.kind
            {
                answered.extend(self.answer(*subject, requirement));
            }
        }
        self.advance(next)?;
        Ok(answered)
    }

    /// Answers every landing that waits, then moves the cursor to the end
    /// of the events, read first, so none after the landings listed is
    /// missed.
    fn catch_up(&mut self) -> Result<Vec<Answer>, WorkerError> {
        let end = self.events(None, None)?.next;
        let mut answered = Vec::new();
        for Pending {
            subject, blocked, ..
        } in self.runner.remote().landings()?
        {
            if let Blocked::Evidence(missing) = blocked {
                answered.extend(
                    missing
                        .iter()
                        .filter_map(|requirement| self.answer(subject, requirement)),
                );
            }
        }
        self.advance(end)?;
        Ok(answered)
    }

    /// Moves the cursor to `cursor`, durably.
    fn advance(&mut self, cursor: u64) -> Result<(), WorkerError> {
        self.state.save_cursor(cursor)?;
        self.cursor = Some(cursor);
        Ok(())
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

    /// Runs the check `requirement` asks for about `subject`, if it's one
    /// of `ci.*`.
    fn answer(&self, subject: RecordId, requirement: &Requirement) -> Option<Answer> {
        let namespace = KindPattern::Prefix(Kind::new(Checks::NAMESPACE).expect("a valid kind"));
        if !namespace.matches(&requirement.kind) {
            return None;
        }
        let kind = requirement.kind.clone();
        let result = self
            .runner
            .check(subject, Some(&kind))
            .and_then(|outcomes| {
                outcomes
                    .into_iter()
                    .next()
                    .ok_or_else(|| RunError::UnknownCheck {
                        subject,
                        kind: kind.clone(),
                    })
            });
        Some(Answer {
            subject,
            kind,
            result,
        })
    }
}

/// How a request for a check went.
#[derive(Debug)]
pub struct Answer {
    /// The record to check.
    pub subject: RecordId,
    /// The kind of statement requested.
    pub kind: Kind,
    /// What became of the check, or why it cannot run, so it waits until
    /// it's requested again.
    pub result: Result<Outcome, RunError>,
}

/// A failure of the worker.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// Another worker uses the state directory.
    #[error("another worker answers this remote's requests, with {0}")]
    Busy(String),
    /// The cursor cannot be read or saved.
    #[error(transparent)]
    State(#[from] StateError),
    /// The remote cannot be reached or refused.
    #[error(transparent)]
    Remote(#[from] RemoteError),
}
