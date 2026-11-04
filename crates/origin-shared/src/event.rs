//! Events: what happened on a node, for plugins and agents to react to.
//! Each carries the cursor of the op log entry it comes from, and reading
//! the events after a cursor returns a [`Page`] and the next cursor. Events
//! are delivered at least once, so consumers deduplicate by
//! [`EventKind::record`].

use std::fmt;

use crate::policy::{KindPattern, Requirement};
use crate::{
    AcceptanceId, ChangeId, Kind, Principal, RecordId, RevisionId, StatementId, TargetName, Verdict,
};

/// Something that happened, and where it is in the op log.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Event {
    /// The sequence number of the op log entry it comes from. Reading from
    /// it delivers the events of the entries after it.
    pub cursor: u64,
    /// What happened.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// What happened. In JSON, the `event` field names it, as
/// [`EventKind::name`] does.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(tag = "event")]
pub enum EventKind {
    /// A statement was issued.
    #[serde(rename = "statement.issued")]
    StatementIssued {
        /// The statement.
        statement: StatementId,
        /// What it's about.
        subject: RecordId,
        /// Its kind.
        kind: Kind,
        /// Its verdict.
        verdict: Option<Verdict>,
        /// Who issued it.
        issuer: Principal,
    },
    /// A revision was created.
    #[serde(rename = "revision.created")]
    RevisionCreated {
        /// The revision.
        revision: RevisionId,
        /// Its change.
        change: ChangeId,
        /// Who made it.
        author: Principal,
    },
    /// A target advanced.
    #[serde(rename = "target.advanced")]
    TargetAdvanced {
        /// The target.
        target: TargetName,
        /// The acceptance's position in its log.
        seq: u64,
        /// The acceptance.
        acceptance: AcceptanceId,
    },
    /// A landing waits for a statement it requires.
    #[serde(rename = "check.requested")]
    CheckRequested {
        /// The target it lands on.
        onto: TargetName,
        /// What the statement must be about.
        subject: RecordId,
        /// The statement required.
        requirement: Requirement,
    },
}

/// What an event is about, which `--subject` matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    /// A record.
    Record(RecordId),
    /// A change: the subject of its revisions' events.
    Change(ChangeId),
    /// A target.
    Target(TargetName),
}

impl EventKind {
    /// The event's name, such as `statement.issued`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::StatementIssued { .. } => "statement.issued",
            Self::RevisionCreated { .. } => "revision.created",
            Self::TargetAdvanced { .. } => "target.advanced",
            Self::CheckRequested { .. } => "check.requested",
        }
    }

    /// The record the event is about, to deduplicate by: the statement,
    /// revision or acceptance, or for a check, the record to check.
    #[must_use]
    pub fn record(&self) -> RecordId {
        match *self {
            Self::StatementIssued { statement, .. } => RecordId::Statement(statement),
            Self::RevisionCreated { revision, .. } => RecordId::Revision(revision),
            Self::TargetAdvanced { acceptance, .. } => RecordId::Acceptance(acceptance),
            Self::CheckRequested { subject, .. } => subject,
        }
    }

    /// What the event is about: a statement's or a check's subject, a
    /// revision's change, or the target that advanced.
    #[must_use]
    pub fn subject(&self) -> Subject {
        match self {
            Self::StatementIssued { subject, .. } | Self::CheckRequested { subject, .. } => {
                Subject::Record(*subject)
            }
            Self::RevisionCreated { change, .. } => Subject::Change(*change),
            Self::TargetAdvanced { target, .. } => Subject::Target(target.clone()),
        }
    }

    /// The kinds a [`Filter`] matches the event by: its name, and the kind
    /// of the statement issued or requested.
    ///
    /// # Panics
    ///
    /// Never: event names are valid kinds.
    #[must_use]
    pub fn kinds(&self) -> Vec<Kind> {
        let name = Kind::new(self.name()).expect("event names are valid kinds");
        match self {
            Self::StatementIssued { kind, .. } => vec![name, kind.clone()],
            Self::CheckRequested { requirement, .. } => vec![name, requirement.kind.clone()],
            Self::RevisionCreated { .. } | Self::TargetAdvanced { .. } => vec![name],
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(record) => record.fmt(f),
            Self::Change(id) => write!(f, "change:{id}"),
            Self::Target(name) => write!(f, "target:{name}"),
        }
    }
}

/// One line: the cursor, the name, then `key=value` fields with full ids.
impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.cursor, self.kind.name())?;
        let verdict = |verdict: &Option<Verdict>| {
            verdict.map_or_else(|| "none".to_owned(), |verdict| verdict.to_string())
        };
        match &self.kind {
            EventKind::StatementIssued {
                statement,
                subject,
                kind,
                verdict: issued,
                issuer,
            } => write!(
                f,
                " statement={statement} subject={} kind={kind} verdict={} issuer={issuer}",
                Subject::Record(*subject),
                verdict(issued)
            ),
            EventKind::RevisionCreated {
                revision,
                change,
                author,
            } => write!(f, " revision={revision} change={change} author={author}"),
            EventKind::TargetAdvanced {
                target,
                seq,
                acceptance,
            } => write!(f, " target={target} seq={seq} acceptance={acceptance}"),
            EventKind::CheckRequested {
                onto,
                subject,
                requirement,
            } => write!(
                f,
                " onto={onto} subject={} kind={} verdict={} issuer={}",
                Subject::Record(*subject),
                requirement.kind,
                requirement.verdict,
                requirement.issuer
            ),
        }
    }
}

/// Which events to deliver. An empty list matches everything.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Event names or statement kinds, such as `check.requested`,
    /// `core.conflict` or `ci.*`: an event matches one if its name, or the
    /// kind it issues or requests, does.
    pub kinds: Vec<KindPattern>,
    /// Starts of subjects, such as a revision id prefix, `change:3fa2` or
    /// `main`: an event matches one if its [subject](EventKind::subject),
    /// written as `revision:<id>`, or its id or name alone, starts with it.
    pub subjects: Vec<String>,
}

impl Filter {
    /// Whether `event` passes the filter.
    #[must_use]
    pub fn matches(&self, event: &EventKind) -> bool {
        let kind = self.kinds.is_empty()
            || event
                .kinds()
                .iter()
                .any(|kind| self.kinds.iter().any(|pattern| pattern.matches(kind)));
        kind && (self.subjects.is_empty() || {
            let subject = event.subject().to_string();
            let bare = subject.split_once(':').map_or("", |(_, bare)| bare);
            self.subjects.iter().any(|start| {
                subject.starts_with(start.as_str()) || bare.starts_with(start.as_str())
            })
        })
    }
}

/// A batch of events, and the cursor to read the next batch from.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
pub struct Page {
    /// The events, oldest first.
    pub events: Vec<Event>,
    /// The cursor after the entries read: pass it to the next read.
    pub next: u64,
}
