//! Running the checks of a record and issuing their results, through the
//! public API only: a [`Remote`], over HTTP or in-process.
//!
//! The runner reads the checks from the `checks.toml` of a snapshot it's
//! given, an [`At`]: the record's own for `origin ci run`, which issues
//! results as the user, but for a worker, the tip of the target the record
//! lands on, so a change can't redefine the checks that guard the target.
//! It fetches that snapshot and the record's, unless it kept one with the
//! same root tree, and asks the remote to promote results with the same
//! input digest to the record. Then for each check, it:
//!
//! 1. checks the snapshot out in a directory of its own, so no check sees
//!    what another changed;
//! 2. runs the check's toolchain command there, whose output, with the
//!    command and the inputs, makes what a result declares it depended on;
//! 3. skips the check if a result of the runner's issuer, ran or promoted,
//!    declaring the same, already covers the record;
//! 4. otherwise runs the command there, and issues a statement as its
//!    issuer, `pass` if it exited with 0, with the end of its output as the
//!    payload, superseding the issuer's previous result about the record.
//!
//! `checks.toml` is always one of the inputs: for `origin ci run`, as the
//! record holds it, and for a worker, as the state of the target the
//! record is on holds it (see [`checks`](crate::checks)). A check defined
//! by another `checks.toml` than the one its results declare, as when the
//! target's tip moved past the record's base, depended on more than it
//! declares: it's declared `external`, so its result is never promoted.
//!
//! A check without a toolchain command declares what it depends on
//! without a checkout, so whether a result covers it is known first: then
//! it's only checked out to run. Each checkout is removed afterwards, and
//! the commands run there are killed with what they left running (see
//! [`state`](crate::state)).

use std::fmt;
use std::io::{self, Read};
use std::panic;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use origin_shared::api::{
    Annotation, At, FetchRecords, FetchSnapshot, PromoteRequest, StatementsQuery,
};
use origin_shared::object::DecodeError;
use origin_shared::pack::Pack;
use origin_shared::{
    Acceptance, Base, EncodedObject, Kind, ObjectId, Principal, Record, RecordId, Revision,
    Statement, StatementId, TargetName, TreeId, Verdict,
};
use origin_src::remote::{Remote, RemoteError};

use crate::checks::{Check, Checks, Defined};
use crate::state::{Checkout, Snapshot, State, StateError};

/// Runs checks on the records of a remote, issuing results as one issuer.
#[derive(Debug)]
pub struct Runner {
    remote: Remote,
    issuer: Principal,
    state: State,
    /// The snapshots used last, least recently first: checks of one
    /// record, of records with the same snapshot, or reading the same
    /// target's checks, fetch it once.
    snapshots: Mutex<Vec<Arc<Snapshot>>>,
}

/// When a check runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rerun {
    /// Unless a result of the issuer's already covers the record.
    Uncovered,
    /// Always: it was asked for.
    Always,
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

    /// How many snapshots are kept once fetched.
    const SNAPSHOTS: usize = 2;

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
            snapshots: Mutex::default(),
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

    /// Runs the checks of `subject`, a revision or an acceptance, as the
    /// `checks.toml` of the snapshot `from` defines them: only the one of
    /// `kind`, if given, or else every check it lists.
    ///
    /// # Errors
    ///
    /// Fails if the subject or `from` has no snapshot, `from` has no
    /// `checks.toml` or no check of `kind`, a command cannot be started or
    /// a toolchain command fails, or the remote refuses.
    pub fn check(
        &self,
        subject: RecordId,
        from: &At,
        kind: Option<&Kind>,
    ) -> Result<Vec<Outcome>, RunError> {
        let (defined, in_force) = match from {
            At::Record(record) => (*record, Defined::InSnapshot),
            At::Target(target) => (self.tip(target)?, Defined::OnTarget),
        };
        let text = self
            .fetch_snapshot(defined)?
            .file(Checks::FILE)?
            .ok_or_else(|| RunError::NoChecks(from.clone()))?;
        let snapshot = self.fetch_snapshot(subject)?;
        // The checks the results declare they read.
        let declared = match in_force {
            Defined::InSnapshot => snapshot.file(Checks::FILE)?,
            Defined::OnTarget => match self.base(subject)? {
                Some(base) => self.fetch_snapshot(base)?.file(Checks::FILE)?,
                None => None,
            },
        };
        // Checks defined otherwise depend on more than that.
        let foreign = declared.as_ref() != Some(&text);
        let checks =
            Checks::parse(&String::from_utf8_lossy(&text), in_force).map_err(|source| {
                RunError::InvalidChecks {
                    at: from.clone(),
                    source: Box::new(source),
                }
            })?;
        let selected: Vec<&Check> = match kind {
            Some(kind) => vec![checks.get(kind).ok_or_else(|| RunError::UnknownCheck {
                at: from.clone(),
                kind: kind.clone(),
            })?],
            None => checks.iter().collect(),
        };
        self.remote.promote(&PromoteRequest { subject })?;
        let statements = self.claims(subject)?;
        selected
            .into_iter()
            .map(|check| Check {
                external: check.external || foreign,
                ..check.clone()
            })
            .map(|check| self.run(&snapshot, subject, &check, &statements, Rerun::Uncovered))
            .collect()
    }

    /// Runs `check`, given rather than read from a `checks.toml`, on
    /// `subject` as [`Runner::check`] runs those: always, even where a
    /// result of the issuer's covers it, since it was asked for. The result
    /// supersedes the issuer's previous one of its kind.
    ///
    /// # Errors
    ///
    /// Fails if the subject has no snapshot, the command cannot be started
    /// or the toolchain command fails, or the remote refuses.
    pub fn exec(&self, subject: RecordId, check: &Check) -> Result<Outcome, RunError> {
        let snapshot = self.fetch_snapshot(subject)?;
        let statements = self.claims(subject)?;
        self.run(&snapshot, subject, check, &statements, Rerun::Always)
    }

    /// The acceptance at the tip of `target`.
    fn tip(&self, target: &TargetName) -> Result<RecordId, RunError> {
        let targets = self.remote.targets()?;
        let info = targets.iter().find(|info| info.name == *target);
        info.map(|info| RecordId::Acceptance(info.head.acceptance))
            .ok_or_else(|| RunError::NoTip(target.clone()))
    }

    /// The acceptance `subject` is based on: an acceptance's own, or a
    /// revision's base, through the revisions below it. `None` for a
    /// snapshot, or past a revision the caller can't fetch.
    fn base(&self, subject: RecordId) -> Result<Option<RecordId>, RunError> {
        let mut revision = match subject {
            RecordId::Acceptance(_) => return Ok(Some(subject)),
            RecordId::Revision(id) => id,
            RecordId::Snapshot(_) | RecordId::Statement(_) => return Ok(None),
        };
        loop {
            let id = ObjectId::from(revision);
            let [object] = &self.fetch(&[id])?[..] else {
                return Ok(None);
            };
            let decoded: Revision = object
                .decode_as()
                .map_err(|source| RunError::Decode { id, source })?;
            match decoded.base {
                Base::Acceptance(acceptance) => return Ok(Some(RecordId::Acceptance(acceptance))),
                Base::Revision(below) => revision = below,
            }
        }
    }

    /// The snapshot of `subject`, fetched unless it's one of those kept.
    fn fetch_snapshot(&self, subject: RecordId) -> Result<Arc<Snapshot>, RunError> {
        let root = self.snapshot(subject)?;
        let kept = || {
            self.snapshots
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        let mut snapshots = kept();
        if let Some(at) = snapshots
            .iter()
            .position(|snapshot| snapshot.root() == root)
        {
            let snapshot = snapshots.remove(at);
            snapshots.push(Arc::clone(&snapshot));
            return Ok(snapshot);
        }
        drop(snapshots);
        let mut request = FetchSnapshot {
            of: subject,
            have: Vec::new(),
            paths: Vec::new(),
            skip: 0,
        };
        let mut pack = Pack::default();
        loop {
            let part = self.remote.snapshot(&request)?;
            request.skip += part.pack.objects.len() as u64;
            pack.objects.extend(part.pack.objects);
            if !part.more {
                break;
            }
        }
        let snapshot = Arc::new(Snapshot::new(root, pack));
        let mut kept = kept();
        kept.push(Arc::clone(&snapshot));
        if kept.len() > Self::SNAPSHOTS {
            kept.remove(0);
        }
        Ok(snapshot)
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

    /// The records `ids` the caller sees, without the attachments they
    /// carry, asking for the rest after each part that stops short.
    fn fetch(&self, ids: &[ObjectId]) -> Result<Vec<EncodedObject>, RunError> {
        let mut records = Vec::new();
        let mut rest = ids.to_vec();
        while !rest.is_empty() {
            let part = self.remote.records(&FetchRecords { ids: rest.clone() })?;
            let fetched: Vec<_> = part
                .pack
                .objects
                .into_iter()
                .filter(|object| ids.contains(&object.id()))
                .collect();
            rest = match fetched.last() {
                Some(last) if part.more => {
                    let after = rest
                        .iter()
                        .position(|&id| id == last.id())
                        .map_or(0, |at| at + 1);
                    rest.split_off(after)
                }
                _ => Vec::new(),
            };
            records.extend(fetched);
        }
        Ok(records)
    }

    /// The statements about `subject` whose claims hold, as the server
    /// counts them for the landing queue (see [`Annotation::holds`]), each
    /// with the claim it makes: itself, or for a promotion, its original.
    fn claims(&self, subject: RecordId) -> Result<Vec<(Annotation, Statement)>, RunError> {
        let current: Vec<Annotation> = self
            .remote
            .statements(&StatementsQuery { subject })?
            .into_iter()
            .filter(|annotation| annotation.holds)
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

    /// Runs `check` in a checkout of `snapshot`, unless `rerun` says to
    /// skip it where one of `statements` about `subject` already covers it.
    fn run(
        &self,
        snapshot: &Snapshot,
        subject: RecordId,
        check: &Check,
        statements: &[(Annotation, Statement)],
        rerun: Rerun,
    ) -> Result<Outcome, RunError> {
        let mut checkout = None;
        let toolchain = match &check.toolchain {
            Some(command) => {
                let checkout = checkout.insert(self.state.checkout(snapshot)?);
                Self::toolchain(checkout, &check.kind, command)?
            }
            None => Vec::new(),
        };
        let inputs = check.inputs(&toolchain);
        let mine = |claim: &Statement| claim.kind == check.kind && claim.issuer == self.issuer;
        // A withdrawal is no result, whatever inputs it declares.
        let covering = statements.iter().find(|(_, claim)| {
            mine(claim) && !claim.withdraws() && claim.inputs.as_ref() == Some(&inputs)
        });
        if let Some((annotation, claim)) = covering.filter(|_| rerun == Rerun::Uncovered) {
            return Ok(Outcome::Covered {
                kind: check.kind.clone(),
                verdict: claim.verdict,
                statement: annotation.id,
                promoted: annotation.statement.promoted_from.is_some(),
            });
        }
        let checkout = match checkout {
            Some(checkout) => checkout,
            None => self.state.checkout(snapshot)?,
        };
        let start = Instant::now();
        let (status, output) = Self::execute(&checkout, &check.command, &check.kind)?;
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

    /// The output of a check's toolchain command, run in `checkout`.
    fn toolchain(checkout: &Checkout, kind: &Kind, command: &str) -> Result<Vec<u8>, RunError> {
        let (reader, writer) = io::pipe().map_err(|source| RunError::command(kind, source))?;
        let mut shell = Self::shell(command);
        shell.stdout(writer).stderr(Stdio::inherit());
        // All of it: it's what the result declares it depended on.
        let read_all = |mut reader: io::PipeReader| {
            let mut output = Vec::new();
            reader.read_to_end(&mut output).map(|_| output)
        };
        let (status, output) = Self::collect(checkout, shell, reader, read_all, kind)?;
        if !status.success() {
            return Err(RunError::Toolchain {
                kind: kind.clone(),
                command: command.to_owned(),
                status,
            });
        }
        Ok(output)
    }

    /// Runs `command`, of the check of `kind`, in `checkout`, returning how
    /// it exited and the end of its output and errors, interleaved: at most
    /// [`Runner::MAX_OUTPUT`] bytes, after a line saying how many were
    /// dropped.
    fn execute(
        checkout: &Checkout,
        command: &str,
        kind: &Kind,
    ) -> Result<(ExitStatus, Vec<u8>), RunError> {
        let failed = |source| RunError::command(kind, source);
        let (reader, writer) = io::pipe().map_err(failed)?;
        let mut shell = Self::shell(command);
        shell
            .stdout(writer.try_clone().map_err(failed)?)
            .stderr(writer);
        Self::collect(checkout, shell, reader, Self::read_output, kind)
    }

    /// Runs `shell`, of the check of `kind`, in `checkout`, returning how it
    /// exited and what `read` makes of the pipe `reader` reads. The pipe is
    /// read on a thread until the command exits and what it left running
    /// is stopped, which closes their copies of it: so nothing it left
    /// running keeps the runner waiting, unless it escaped being stopped
    /// (see [`state`](crate::state)).
    fn collect(
        checkout: &Checkout,
        shell: Command,
        reader: io::PipeReader,
        read: fn(io::PipeReader) -> io::Result<Vec<u8>>,
        kind: &Kind,
    ) -> Result<(ExitStatus, Vec<u8>), RunError> {
        // `spawn` drops the command, and with it this process's copies of
        // the pipe.
        let mut child = checkout.spawn(shell)?;
        let group = child.id();
        let output = thread::spawn(move || read(reader));
        let status = child.wait();
        checkout.stop(group)?;
        let output = output
            .join()
            .unwrap_or_else(|panic| panic::resume_unwind(panic));
        let failed = |source| RunError::command(kind, source);
        Ok((status.map_err(failed)?, output.map_err(failed)?))
    }

    /// The end of the output `reader` gives, as [`Runner::execute`] keeps
    /// it.
    fn read_output(mut reader: io::PipeReader) -> io::Result<Vec<u8>> {
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
        Ok(output)
    }

    /// `sh -c <command>`, reading nothing, without the runner's token: the
    /// commands run what the record holds, which anyone may have written.
    fn shell(command: &str) -> Command {
        let mut shell = Command::new("sh");
        shell
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .env_remove("ORIGIN_TOKEN");
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
    /// The target to read the checks from has no tip the caller sees.
    #[error("target {0} has no tip to read checks.toml from")]
    NoTip(TargetName),
    /// The snapshot holding the checks has no `checks.toml`, or the caller
    /// may not read it.
    #[error("the snapshot at {0} has no checks.toml")]
    NoChecks(At),
    /// The `checks.toml` read is invalid.
    #[error("the checks.toml at {at} is invalid")]
    InvalidChecks {
        /// The snapshot holding it.
        at: At,
        /// What is wrong, boxed: it's large, and rare.
        source: Box<toml::de::Error>,
    },
    /// A check was asked for that `checks.toml` doesn't list.
    #[error("the checks.toml at {at} has no check issuing `{kind}`")]
    UnknownCheck {
        /// The snapshot holding it.
        at: At,
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

impl RunError {
    /// The command of the check of `kind` cannot be started, or its output
    /// read.
    fn command(kind: &Kind, source: io::Error) -> Self {
        Self::Command {
            kind: kind.clone(),
            source,
        }
    }
}
