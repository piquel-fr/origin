//! The `origin` command line: argument parsing. The commands themselves are
//! methods of [`Workspace`], which print their output.

use std::convert::Infallible;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand};
use origin_shared::{Digest, Inputs, Kind, RepoPath, TargetName, Verdict};

use crate::config::UserConfig;
use crate::workspace::{Annotated, Payload, Workspace};

/// A modern, git-compatible source control system.
#[derive(Debug, Parser)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a repository, with the targets `policy` and `main`.
    Init {
        /// Where to create it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    #[command(flatten)]
    Workspace(WorkspaceCommand),
}

/// Commands run in the repository containing the current directory. Those
/// that read the working copy snapshot it first. Changes are named by a
/// unique prefix of their id, or `@` for the working copy's.
#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Show the working copy's change and the files it changes.
    Status,
    /// Show the working copy's changes, line by line.
    Diff,
    /// Show a change and its diff: the working copy's by default.
    Show {
        /// The change.
        change: Option<String>,
    },
    /// Finish the current change, and start a new one on top of it.
    New {
        /// What the new change is for.
        #[arg(default_value = "")]
        description: String,
    },
    /// Describe what the current change is for.
    Describe {
        /// The description.
        description: String,
    },
    /// Switch the working copy to a change.
    Edit {
        /// The change.
        change: String,
    },
    /// Give a change up, keeping it and the reason for others to see.
    Abandon {
        /// The change: the working copy's by default, which then starts a
        /// new change where it started.
        change: Option<String>,
        /// Why it was given up.
        #[arg(long)]
        reason: String,
    },
    /// End a change's divergence, with a revision evolving from every head.
    Converge {
        /// The change.
        change: String,
    },
    /// Show the working copy's changes and the target they're on.
    Log {
        /// Show this target's history instead.
        target: Option<TargetName>,
        /// Show at most this many acceptances.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// Show every revision of a change.
    Evolog {
        /// The change.
        #[arg(default_value = "@")]
        change: String,
    },
    /// Manage the record index.
    #[command(subcommand)]
    Index(IndexCommand),
    /// Issue a statement about a revision, such as a check result or a
    /// review, or with `--on`, about another statement, such as a `flaky`
    /// mark on a failed check, superseding your previous one of the same
    /// kind about it, or withdraw that one.
    #[command(
        override_usage = "origin annotate [OPTIONS] <--pass|--fail|--neutral|--withdraw> \
                                <REV> <KIND>\n       origin annotate [OPTIONS] \
                                <--pass|--fail|--neutral|--withdraw> --on <STATEMENT> <KIND>"
    )]
    Annotate(AnnotateArgs),
    /// Show the statements about a revision, each with the statements
    /// about it indented under it.
    Statements {
        /// The revision: `@`, a change for its head, or a revision.
        #[arg(default_value = "@")]
        rev: String,
    },
    /// Carry results over to a revision from the records with the same
    /// input digest.
    Promote {
        /// The revision: `@`, a change for its head, or a revision.
        #[arg(default_value = "@")]
        rev: String,
    },
}

#[derive(Debug, Args)]
struct AnnotateArgs {
    /// The revision, `@`, a change for its head, or a revision; with
    /// `--on`, the kind instead.
    #[arg(value_name = "REV|KIND")]
    first: String,
    /// What kind of statement, such as `ci.test` or `review`, after a
    /// revision.
    #[arg(value_name = "KIND")]
    kind: Option<String>,
    /// The statement it's about instead of a revision, by a unique prefix
    /// of its id. It's never promoted, so it declares no inputs.
    #[arg(
        long,
        value_name = "STATEMENT",
        conflicts_with_all = ["inputs", "definition", "toolchain", "external"]
    )]
    on: Option<String>,
    #[command(flatten)]
    verdict: VerdictArgs,
    /// Instead of a verdict, take back your current statement of this kind
    /// about the revision or statement: the details may say why.
    #[arg(
        long,
        group = "VerdictArgs",
        conflicts_with_all = ["inputs", "definition", "toolchain", "external"]
    )]
    withdraw: bool,
    /// The files and directories the claim depended on, which lets it be
    /// promoted to revisions where they're the same.
    #[arg(long, num_args = 1..)]
    inputs: Vec<RepoPath>,
    /// The check's command and configuration: text, or the hex digest of
    /// it.
    #[arg(long, requires = "inputs")]
    definition: Option<DigestArg>,
    /// The tools that ran it, such as the output of `rustc -V`: text, or the
    /// hex digest of it.
    #[arg(long, requires = "inputs")]
    toolchain: Option<DigestArg>,
    /// The claim also depended on things outside the inputs, such as the
    /// network, so it is never promoted.
    #[arg(long, requires = "inputs")]
    external: bool,
    #[command(flatten)]
    payload: PayloadArgs,
}

/// At most one source of details.
#[derive(Debug, Args)]
#[group(multiple = false)]
struct PayloadArgs {
    /// Details as text, such as a review's message.
    #[arg(short, long)]
    message: Option<String>,
    /// A file holding details, such as a log, or `-` to read them from
    /// standard input.
    #[arg(long)]
    payload: Option<PathBuf>,
}

impl PayloadArgs {
    fn payload(&self) -> Option<Payload> {
        let message = self.message.clone().map(Payload::Text);
        message.or_else(|| self.payload.clone().map(Payload::File))
    }
}

/// Exactly one verdict.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct VerdictArgs {
    /// The check passed, or you approve.
    #[arg(long)]
    pass: bool,
    /// The check failed, or you object.
    #[arg(long)]
    fail: bool,
    /// Neither: informational.
    #[arg(long)]
    neutral: bool,
}

impl VerdictArgs {
    fn verdict(&self) -> Verdict {
        match (self.pass, self.fail) {
            (true, _) => Verdict::Pass,
            (_, true) => Verdict::Fail,
            _ => Verdict::Neutral,
        }
    }
}

impl AnnotateArgs {
    /// What the statement is about, and its kind: the revision, then the
    /// kind, or with `--on`, the kind alone.
    ///
    /// # Errors
    ///
    /// Fails, as parsing does, if the arguments aren't one of those, or the
    /// kind isn't a valid one.
    fn subject(&self) -> Result<(Annotated<'_>, Kind), clap::Error> {
        let (annotated, kind) = match (&self.on, &self.kind) {
            (Some(on), None) => (Annotated::Statement(on), &self.first),
            (None, Some(kind)) => (Annotated::Revision(&self.first), kind),
            (Some(_), Some(_)) => {
                let message = "with --on, give the kind alone, without a revision";
                return Err(clap::Error::raw(ErrorKind::TooManyValues, message));
            }
            (None, None) => {
                let message = "give the revision, then the kind, or --on and the kind";
                return Err(clap::Error::raw(
                    ErrorKind::MissingRequiredArgument,
                    message,
                ));
            }
        };
        let kind = kind.parse().map_err(|error| {
            let message = format!("invalid kind `{kind}`: {error}");
            clap::Error::raw(ErrorKind::InvalidValue, message)
        })?;
        Ok((annotated, kind))
    }

    /// The verdict, or `None` to withdraw.
    fn verdict(&self) -> Option<Verdict> {
        (!self.withdraw).then(|| self.verdict.verdict())
    }

    /// The inputs declared, if any. Without a definition or toolchain, they
    /// are the digest of nothing.
    fn inputs(&self) -> Option<Inputs> {
        let digest = |arg: &Option<DigestArg>| arg.map_or_else(|| Digest::of(b""), |arg| arg.0);
        (!self.inputs.is_empty()).then(|| Inputs {
            paths: self.inputs.clone(),
            definition: digest(&self.definition),
            toolchain: digest(&self.toolchain),
            external: self.external,
        })
    }
}

/// A digest given as its hex, or as the text it's the digest of.
#[derive(Clone, Copy, Debug)]
struct DigestArg(Digest);

impl FromStr for DigestArg {
    type Err = Infallible;

    fn from_str(arg: &str) -> Result<Self, Self::Err> {
        Ok(Self(
            arg.parse().unwrap_or_else(|_| Digest::of(arg.as_bytes())),
        ))
    }
}

#[derive(Debug, Subcommand)]
enum IndexCommand {
    /// Rebuild the index from the store.
    Rebuild,
}

impl Cli {
    /// Parses the command line, then checks what parsing can't: an
    /// `annotate`'s arguments, whose meaning depends on `--on`. Exits as a
    /// parse error does on either, with the usage.
    ///
    /// # Panics
    ///
    /// Never: `annotate` is a subcommand.
    #[must_use]
    pub fn parse_checked() -> Self {
        let cli = Self::parse();
        if let Command::Workspace(WorkspaceCommand::Annotate(args)) = &cli.command
            && let Err(error) = args.subject()
        {
            let mut command = Self::command();
            let annotate = command
                .find_subcommand_mut("annotate")
                .expect("`annotate` is a subcommand");
            error.format(annotate).exit();
        }
        cli
    }

    /// Runs the command, printing its output.
    ///
    /// # Errors
    ///
    /// Fails if the command fails; the error is meant for the user.
    pub fn run(self) -> Result<()> {
        let config = UserConfig::load()?;
        match self.command {
            Command::Init { path } => {
                Workspace::init(&path, config)?;
            }
            Command::Workspace(command) => command.run(config)?,
        }
        Ok(())
    }
}

impl WorkspaceCommand {
    fn run(self, config: UserConfig) -> Result<()> {
        let cwd = env::current_dir().context("cannot read the current directory")?;
        let mut workspace = Workspace::discover(&cwd, config)?;
        match self {
            Self::Status => workspace.status()?,
            Self::Diff => workspace.diff()?,
            Self::Show { change } => workspace.show(change.as_deref())?,
            Self::New { description } => workspace.new_change(&description)?,
            Self::Describe { description } => workspace.describe(&description)?,
            Self::Edit { change } => workspace.edit(&change)?,
            Self::Abandon { change, reason } => workspace.abandon(change.as_deref(), &reason)?,
            Self::Converge { change } => workspace.converge(&change)?,
            Self::Log { target, limit } => workspace.log(target, limit)?,
            Self::Evolog { change } => workspace.evolog(&change)?,
            Self::Index(IndexCommand::Rebuild) => workspace.rebuild_index()?,
            Self::Annotate(args) => {
                let (annotated, kind) = args.subject()?;
                workspace.annotate(
                    annotated,
                    kind,
                    args.verdict(),
                    args.inputs(),
                    args.payload.payload().as_ref(),
                )?;
            }
            Self::Statements { rev } => workspace.statements(&rev)?,
            Self::Promote { rev } => workspace.promote(&rev)?,
        }
        Ok(())
    }
}
