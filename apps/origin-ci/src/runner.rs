//! Running the checks of a record and issuing their results, through the
//! public API only: a [`Remote`], over HTTP or in-process.
//!
//! For each check, the runner:
//!
//! 1. checks the record's snapshot out in a directory of its own, and reads
//!    its `checks.toml`;
//! 2. runs the check's toolchain command there, whose output, with the
//!    command and the inputs, makes what a result declares it depended on;
//! 3. asks the remote to promote results with the same input digest, and
//!    skips the check if a result of the runner's issuer, ran or promoted,
//!    declaring the same, already covers the record;
//! 4. otherwise runs the command there, and issues a statement as its
//!    issuer, `pass` if it exited with 0, with the end of its output as the
//!    payload, superseding the issuer's previous result about the record.
//!
//! The checkout is removed afterwards.

use std::fmt;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use origin_shared::api::{
    Annotation, FetchRecords, FetchSnapshot, PromoteRequest, StatementsQuery,
};
use origin_shared::object::DecodeError;
use origin_shared::pack::Pack;
use origin_shared::{
    Acceptance, EncodedObject, Kind, ObjectId, Principal, Record, RecordId, Revision, Statement,
    StatementId, TreeId, Verdict,
};
use origin_src::remote::{Remote, RemoteError};

use crate::checks::{Check, Checks};
use crate::state::{Checkout, State, StateError};

/// Runs checks on the records of a remote, issuing results as one issuer.
#[derive(Debug)]
pub struct Runner {
    remote: Remote,
    issuer: Principal,
    state: State,
}

/// What became of one check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It ran, and its result was issued.
    Ran {
        /// The check's kind.
        kind: Kind,
        /// Its result.
        verdict: Verdict,
        /// The statement issued.
        statement: StatementId,
        /// How long the command ran.
        took: Duration,
        /// The end of its output, as the payload holds it.
        output: Vec<u8>,
    },
    /// A result of the issuer declaring the same inputs covers the record,
    /// so it didn't run.
    Covered {
        /// The check's kind.
        kind: Kind,
        /// The result.
        verdict: Option<Verdict>,
        /// The statement about the record: the result, or its promotion.
        statement: StatementId,
        /// Whether it's a promotion of a result about another record.
        promoted: bool,
    },
}

impl Runner {
    /// The most output a payload keeps: the end of it.
    pub const MAX_OUTPUT: usize = 64 * 1024;

    /// Runs checks on `remote`'s records, issuing results as the one it
    /// knows the caller as, or else, for an in-process remote, whose
    /// caller is the owner, as `user`, keeping checkouts in `state`.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot tell who the caller is.
    pub fn new(remote: Remote, user: Principal, state: State) -> Result<Self, RunError> {
        let issuer = remote.whoami()?.principal.unwrap_or(user);
        Ok(Self {
            remote,
            issuer,
            state,
        })
    }

    /// The remote the runner checks the records of.
    #[must_use]
    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    /// Who issues the results.
    #[must_use]
    pub fn issuer(&self) -> &Principal {
        &self.issuer
    }

    /// Runs the checks of `subject`, a revision, an acceptance or a waiting
    /// landing's snapshot: only the one of `kind`, if given, or else every
    /// check its `checks.toml` lists.
    ///
    /// # Errors
    ///
    /// Fails if the subject has no snapshot or `checks.toml`, has no check
    /// of `kind`, a command cannot be started or a toolchain command fails,
    /// or the remote refuses.
    pub fn check(&self, subject: RecordId, kind: Option<&Kind>) -> Result<Vec<Outcome>, RunError> {
        let root = self.snapshot(subject)?;
        let pack = self.remote.snapshot(&FetchSnapshot {
            of: subject,
            have: Vec::new(),
            paths: Vec::new(),
        })?;
        let checkout = self.state.checkout(&pack, root)?;
        let file = checkout.path().join(Checks::FILE);
        let text = std::fs::read_to_string(&file)
            .map_err(|source| RunError::NoChecks { subject, source })?;
        let checks = Checks::parse(&text).map_err(|source| RunError::InvalidChecks {
            subject,
            source: Box::new(source),
        })?;
        let selected: Vec<&Check> = match kind {
            Some(kind) => vec![checks.get(kind).ok_or_else(|| RunError::UnknownCheck {
                subject,
                kind: kind.clone(),
            })?],
            None => checks.iter().collect(),
        };
        self.remote.promote(&PromoteRequest { subject })?;
        let statements = self.claims(subject)?;
        selected
            .into_iter()
            .map(|check| self.run(&checkout, subject, check, &statements))
            .collect()
    }

    /// The root tree of the snapshot of `subject`.
    fn snapshot(&self, subject: RecordId) -> Result<TreeId, RunError> {
        let id = match subject {
            RecordId::Snapshot(tree) => return Ok(tree),
            RecordId::Revision(id) => ObjectId::from(id),
            RecordId::Acceptance(id) => ObjectId::from(id),
            RecordId::Statement(_) => return Err(RunError::NoSnapshot(subject)),
        };
        let [object] = &self.fetch(&[id])?[..] else {
            return Err(RunError::NoSnapshot(subject));
        };
        let decoded = match subject {
            RecordId::Revision(_) => object
                .decode_as::<Revision>()
                .map(|revision| revision.snapshot),
            _ => object
                .decode_as::<Acceptance>()
                .map(|acceptance| acceptance.body().state),
        };
        decoded.map_err(|source| RunError::Decode { id, source })
    }

    /// The records `ids`, without the attachments they carry.
    fn fetch(&self, ids: &[ObjectId]) -> Result<Vec<EncodedObject>, RunError> {
        let pack = self.remote.records(&FetchRecords { ids: ids.to_vec() })?;
        Ok(pack
            .objects
            .into_iter()
            .filter(|object| ids.contains(&object.id()))
            .collect())
    }

    /// The statements about `subject` that nothing superseded, each with the
    /// claim it makes: itself, or for a promotion, its original.
    fn claims(&self, subject: RecordId) -> Result<Vec<(Annotation, Statement)>, RunError> {
        let current: Vec<Annotation> = self
            .remote
            .statements(&StatementsQuery { subject })?
            .into_iter()
            .filter(|annotation| annotation.superseded_by.is_none())
            .collect();
        let originals: Vec<ObjectId> = current
            .iter()
            .filter_map(|annotation| annotation.statement.promoted_from)
            .map(ObjectId::from)
            .collect();
        let originals = self.fetch(&originals)?;
        current
            .into_iter()
            .map(|annotation| {
                let Some(original) = annotation.statement.promoted_from else {
                    let claim = annotation.statement.clone();
                    return Ok((annotation, claim));
                };
                let id = ObjectId::from(original);
                let object = originals
                    .iter()
                    .find(|object| object.id() == id)
                    .ok_or(RunError::Missing(id))?;
                let claim = object
                    .decode_as()
                    .map_err(|source| RunError::Decode { id, source })?;
                Ok((annotation, claim))
            })
            .collect()
    }

    /// Runs `check` in `checkout`, unless one of `statements` about
    /// `subject` already covers it.
    fn run(
        &self,
        checkout: &Checkout,
        subject: RecordId,
        check: &Check,
        statements: &[(Annotation, Statement)],
    ) -> Result<Outcome, RunError> {
        let toolchain = match &check.toolchain {
            Some(command) => Self::toolchain(checkout.path(), &check.kind, command)?,
            None => Vec::new(),
        };
        let inputs = check.inputs(&toolchain);
        let mine = |claim: &Statement| claim.kind == check.kind && claim.issuer == self.issuer;
        let covering = statements
            .iter()
            .find(|(_, claim)| mine(claim) && claim.inputs.as_ref() == Some(&inputs));
        if let Some((annotation, claim)) = covering {
            return Ok(Outcome::Covered {
                kind: check.kind.clone(),
                verdict: claim.verdict,
                statement: annotation.id,
                promoted: annotation.statement.promoted_from.is_some(),
            });
        }
        let start = Instant::now();
        let (status, output) =
            Self::execute(checkout.path(), &check.command).map_err(|source| RunError::Command {
                kind: check.kind.clone(),
                source,
            })?;
        let took = start.elapsed();
        let verdict = if status.success() {
            Verdict::Pass
        } else {
            Verdict::Fail
        };
        let payload = (!output.is_empty()).then(|| EncodedObject::attachment(&output));
        let supersedes = statements
            .iter()
            .find(|(annotation, claim)| mine(claim) && annotation.statement.promoted_from.is_none())
            .map(|(annotation, _)| annotation.id);
        let statement = Statement {
            subject,
            kind: check.kind.clone(),
            verdict: Some(verdict),
            issuer: self.issuer.clone(),
            inputs: Some(inputs),
            payload: payload.as_ref().map(|payload| payload.id().into()),
            supersedes,
            promoted_from: None,
        }
        .encode();
        let id = statement.id().into();
        let objects = payload.into_iter().chain([statement]).collect();
        self.remote.put_records(Pack { objects })?;
        Ok(Outcome::Ran {
            kind: check.kind.clone(),
            verdict,
            statement: id,
            took,
            output,
        })
    }

    /// The output of a check's toolchain command, run in `dir`.
    fn toolchain(dir: &Path, kind: &Kind, command: &str) -> Result<Vec<u8>, RunError> {
        let output = Self::shell(dir, command)
            .stderr(Stdio::inherit())
            .output()
            .map_err(|source| RunError::Command {
                kind: kind.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(RunError::Toolchain {
                kind: kind.clone(),
                command: command.to_owned(),
                status: output.status,
            });
        }
        Ok(output.stdout)
    }

    /// Runs `command` in `dir`, returning how it exited and the end of its
    /// output and errors, interleaved: at most [`Runner::MAX_OUTPUT`]
    /// bytes, after a line saying how many were dropped.
    fn execute(dir: &Path, command: &str) -> io::Result<(ExitStatus, Vec<u8>)> {
        let (mut reader, writer) = io::pipe()?;
        let mut shell = Self::shell(dir, command);
        let mut child = shell.stdout(writer.try_clone()?).stderr(writer).spawn()?;
        // The pipe ends once the command, and whatever it started, close
        // their copies of it.
        drop(shell);
        let mut output = Vec::new();
        let mut dropped = 0;
        let mut chunk = [0; 8192];
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            output.extend_from_slice(&chunk[..read]);
            if output.len() > 2 * Self::MAX_OUTPUT {
                let excess = output.len() - Self::MAX_OUTPUT;
                output.drain(..excess);
                dropped += excess;
            }
        }
        let status = child.wait()?;
        if output.len() > Self::MAX_OUTPUT {
            let excess = output.len() - Self::MAX_OUTPUT;
            output.drain(..excess);
            dropped += excess;
        }
        if dropped > 0 {
            let mut kept = format!("[{dropped} bytes of output dropped]\n").into_bytes();
            kept.append(&mut output);
            output = kept;
        }
        Ok((status, output))
    }

    /// `sh -c <command>` in `dir`, reading nothing.
    fn shell(dir: &Path, command: &str) -> Command {
        let mut shell = Command::new("sh");
        shell
            .arg("-c")
            .arg(command)
            .current_dir(dir)
            .stdin(Stdio::null());
        shell
    }
}

impl Outcome {
    /// The check's kind.
    #[must_use]
    pub fn kind(&self) -> &Kind {
        match self {
            Self::Ran { kind, .. } | Self::Covered { kind, .. } => kind,
        }
    }

    /// Whether the check failed, ran or covered.
    #[must_use]
    pub fn failed(&self) -> bool {
        match self {
            Self::Ran { verdict, .. } => *verdict == Verdict::Fail,
            Self::Covered { verdict, .. } => *verdict == Some(Verdict::Fail),
        }
    }
}

/// One line: the kind, the verdict, and the statement saying so.
impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ran {
                kind,
                verdict,
                statement,
                took,
                ..
            } => write!(
                f,
                "{kind} {verdict} in {:.2}s: statement {}",
                took.as_secs_f64(),
                statement.short()
            ),
            Self::Covered {
                kind,
                verdict,
                statement,
                promoted,
            } => {
                let verdict =
                    verdict.map_or_else(|| "none".to_owned(), |verdict| verdict.to_string());
                let how = if *promoted { "promoted" } else { "already ran" };
                write!(
                    f,
                    "{kind} {verdict}, {how}: statement {}",
                    statement.short()
                )
            }
        }
    }
}

/// A failure to run checks.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The subject is a statement, or isn't a record the remote has.
    #[error("{0} has no snapshot to check")]
    NoSnapshot(RecordId),
    /// The subject's snapshot has no `checks.toml`.
    #[error("the snapshot of {subject} has no checks.toml")]
    NoChecks {
        /// The record checked.
        subject: RecordId,
        /// Why it cannot be read.
        source: io::Error,
    },
    /// The subject's `checks.toml` is invalid.
    #[error("the checks.toml of {subject} is invalid")]
    InvalidChecks {
        /// The record checked.
        subject: RecordId,
        /// What is wrong, boxed: it's large, and rare.
        source: Box<toml::de::Error>,
    },
    /// A check was asked for that `checks.toml` doesn't list.
    #[error("the checks.toml of {subject} has no check issuing `{kind}`")]
    UnknownCheck {
        /// The record checked.
        subject: RecordId,
        /// The kind asked for.
        kind: Kind,
    },
    /// A command cannot be started, or its output read.
    #[error("cannot run the command of `{kind}`")]
    Command {
        /// The check.
        kind: Kind,
        /// What went wrong.
        source: io::Error,
    },
    /// A toolchain command failed.
    #[error("the toolchain command of `{kind}`, `{command}`, failed: {status}")]
    Toolchain {
        /// The check.
        kind: Kind,
        /// The command.
        command: String,
        /// How it exited.
        status: ExitStatus,
    },
    /// A record the remote sent is malformed.
    #[error("record {id} is malformed")]
    Decode {
        /// The record.
        id: ObjectId,
        /// What is wrong.
        source: DecodeError,
    },
    /// The remote didn't send a record it listed.
    #[error("the remote didn't send record {0}")]
    Missing(ObjectId),
    /// The checkout cannot be written.
    #[error(transparent)]
    State(#[from] StateError),
    /// The remote cannot be reached or refused.
    #[error(transparent)]
    Remote(#[from] RemoteError),
}
