//! The `origin` command line: argument parsing. The commands themselves are
//! methods of [`Workspace`], which print their output.

use std::convert::Infallible;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use origin_shared::{Digest, Inputs, Kind, RepoPath, TargetName, Verdict};

use crate::config::UserConfig;
use crate::workspace::Workspace;

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
        /// Start it on the tip of this target instead.
        #[arg(long)]
        on: Option<TargetName>,
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
    /// review, superseding your previous one of the same kind about it.
    Annotate(AnnotateArgs),
    /// Show the statements about a revision.
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
    /// Drop the payloads past the retention set in `policy.toml`, pack
    /// small objects, and remove the leftovers of crashed writes.
    Gc,
    /// Land a change on a target, with the changes below it the target
    /// doesn't have yet. The queue rebases it onto the target's tip, checks
    /// the evidence `policy.toml` requires, and if it's there, appends a
    /// signed acceptance. A landing that conflicts or lacks evidence waits.
    Land(LandArgs),
    /// Manage targets.
    #[command(subcommand)]
    Target(TargetCommand),
    /// List the targets, with their tips, authorities and requirements.
    Targets,
    /// Manage workspaces: working copies sharing this repository.
    #[command(subcommand)]
    Workspace(WorkspacesCommand),
}

#[derive(Debug, Args)]
struct LandArgs {
    /// The change: the working copy's by default.
    #[arg(conflicts_with_all = ["from", "status"])]
    change: Option<String>,
    /// The target to land on: by default, the one the change is on.
    #[arg(long)]
    onto: Option<TargetName>,
    /// Land the tip of this target instead of a change.
    #[arg(long, requires = "onto")]
    from: Option<TargetName>,
    /// List the landings that wait, and why.
    #[arg(long, conflicts_with_all = ["onto", "from"])]
    status: bool,
}

#[derive(Debug, Subcommand)]
enum TargetCommand {
    /// Create a target at the tip of another.
    New {
        /// Its name, such as `release/v3`.
        name: TargetName,
        /// The target it starts from.
        #[arg(long)]
        from: TargetName,
    },
}

#[derive(Debug, Args)]
struct AnnotateArgs {
    /// The revision: `@`, a change for its head, or a revision.
    rev: String,
    /// What kind of statement, such as `ci.test` or `review`.
    kind: Kind,
    #[command(flatten)]
    verdict: VerdictArgs,
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
    /// A file holding details, such as a log.
    #[arg(long)]
    payload: Option<PathBuf>,
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

#[derive(Debug, Subcommand)]
enum WorkspacesCommand {
    /// Create a workspace, with a new change beside the working copy's.
    New {
        /// Check out this change's head instead.
        #[arg(long)]
        change: Option<String>,
        /// Where: by default beside the main working copy, named after the
        /// change.
        path: Option<PathBuf>,
    },
    /// List the workspaces and their changes.
    List,
    /// Remove a workspace, keeping its work as a revision of its change.
    Rm {
        /// The workspace.
        path: PathBuf,
    },
}

impl Cli {
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
            Self::New { description, on } => match on {
                Some(target) => workspace.new_on(&target, &description)?,
                None => workspace.new_change(&description)?,
            },
            Self::Describe { description } => workspace.describe(&description)?,
            Self::Edit { change } => workspace.edit(&change)?,
            Self::Abandon { change, reason } => workspace.abandon(change.as_deref(), &reason)?,
            Self::Converge { change } => workspace.converge(&change)?,
            Self::Log { target, limit } => workspace.log(target, limit)?,
            Self::Evolog { change } => workspace.evolog(&change)?,
            Self::Index(IndexCommand::Rebuild) => workspace.rebuild_index()?,
            Self::Annotate(args) => workspace.annotate(
                &args.rev,
                args.kind.clone(),
                args.verdict.verdict(),
                args.inputs(),
                args.payload.as_deref(),
            )?,
            Self::Statements { rev } => workspace.statements(&rev)?,
            Self::Promote { rev } => workspace.promote(&rev)?,
            Self::Gc => workspace.gc()?,
            Self::Land(args) => match (args.from, args.onto) {
                _ if args.status => workspace.land_status()?,
                (Some(from), Some(onto)) => workspace.land_target(&from, &onto)?,
                (_, onto) => workspace.land(args.change.as_deref(), onto)?,
            },
            Self::Target(TargetCommand::New { name, from }) => {
                workspace.target_new(&name, &from)?;
            }
            Self::Targets => workspace.targets()?,
            Self::Workspace(WorkspacesCommand::New { change, path }) => {
                workspace.add_workspace(change.as_deref(), path.as_deref())?;
            }
            Self::Workspace(WorkspacesCommand::List) => workspace.list_workspaces()?,
            Self::Workspace(WorkspacesCommand::Rm { path }) => workspace.remove_workspace(&path)?,
        }
        Ok(())
    }
}
