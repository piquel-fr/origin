//! The `origin` command line: argument parsing. The commands themselves are
//! methods of [`Workspace`] and [`ServerConfig`], which print their output.

use std::convert::Infallible;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use origin_shared::access::Capability;
use origin_shared::policy::KindPattern;
use origin_shared::{
    ChangeId, Digest, Inputs, Kind, Period, Principal, RepoPath, TargetName, Verdict,
};

use crate::config::UserConfig;
use crate::oplog::Actor;
use crate::server::ServerConfig;
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
    /// Copy a remote repository: the chains of `policy` and a target, and
    /// the target's tip, on which a new change starts. Other snapshots are
    /// fetched when something reads them.
    Clone {
        /// The repository: `http://host[:port]/<repository>`, or a path.
        url: String,
        /// Where to create it: by default, named after the repository.
        path: Option<PathBuf>,
        /// The target to follow and start on.
        #[arg(short, long, default_value = "main")]
        target: TargetName,
    },
    /// Serve repositories over HTTP, as their authority.
    Serve {
        /// The server configuration file.
        #[arg(short, long, default_value = "origin-server.toml")]
        config: PathBuf,
    },
    /// Manage the repositories a server hosts.
    #[command(subcommand)]
    Repo(RepoCommand),
    /// Manage tokens.
    #[command(subcommand)]
    Token(TokenCommand),
    #[command(flatten)]
    Workspace(WorkspaceCommand),
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Create a repository without a working copy for a server to host,
    /// with the server's node the authority of `policy` and `main`.
    Create {
        /// Where to create it.
        path: PathBuf,
        /// The server configuration file.
        #[arg(short, long, default_value = "origin-server.toml")]
        config: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Issue a short-lived token with a narrower scope, for an agent or a
    /// CI runner: the remote signs one acting for you, or, with
    /// `--config`, a server signs one for anyone.
    Issue {
        /// A capability: `read <path>`, `write <path>`, `annotate <kind>`,
        /// `land <target>`, `land *` or `admin`. Repeat it for several.
        #[arg(long, required = true)]
        scope: Vec<Capability>,
        /// How long it lasts, such as `1h` or `7d`.
        #[arg(long, default_value = "1h")]
        ttl: Period,
        /// Sign it with the key of the server this configuration file
        /// describes, instead of asking the remote.
        #[arg(short, long, requires = "principal")]
        config: Option<PathBuf>,
        /// Who it acts for, with `--config`, such as `user:agent-1`.
        #[arg(long = "as", requires = "config")]
        principal: Option<Principal>,
    },
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
    /// Show the op log: what this node did, in every workspace.
    #[command(subcommand)]
    Op(OpCommand),
    /// Move this workspace's working copy back to where it was before its
    /// latest operation. Records and targets stay as they are.
    Undo,
    /// Show the remote, or set it.
    Remote {
        /// The new remote: `http://host[:port]/<repository>`, or a path.
        url: Option<String>,
    },
    /// Fetch the remote's new acceptances on the targets followed, and the
    /// records of the changes this repository knows. Changes no files.
    Fetch {
        /// Follow this target too.
        #[arg(long)]
        target: Vec<TargetName>,
        /// Fetch this change too, by its full id.
        #[arg(long)]
        change: Vec<ChangeId>,
    },
    /// Fetch, then move the working copy onto its target's new tip.
    Pull,
    /// Show who the remote thinks you are.
    Whoami,
    /// Show what the remote lets you do: the paths you read and write, the
    /// statements you issue, and the targets you land on.
    Permissions,
    /// Share changes with the remote, with the changes below them: their
    /// revisions, files and statements. Lands nothing.
    Push {
        /// The changes: the working copy's by default.
        changes: Vec<String>,
    },
    /// Print events as they happen, one a line, starting with the cursor to
    /// resume from: the remote's, if the repository has one. Runs until
    /// interrupted.
    Watch {
        /// Only events of this name, such as `check.requested`, or about
        /// statements of this kind, such as `core.conflict` or `ci.*`.
        #[arg(long)]
        kind: Vec<KindPattern>,
        /// Only events about this: a revision or change id prefix, or a
        /// target name.
        #[arg(long)]
        subject: Vec<String>,
        /// Start after this cursor instead of now: `0` for every event kept.
        #[arg(long)]
        since: Option<u64>,
        /// This node's events, rather than the remote's.
        #[arg(long)]
        local: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OpCommand {
    /// List the latest operations, newest first.
    Log {
        /// Show at most this many operations.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
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
        match self.command {
            Command::Serve { config } => return ServerConfig::load(&config)?.serve(),
            Command::Repo(RepoCommand::Create { path, config }) => {
                return ServerConfig::load(&config)?.create_repo(&path);
            }
            Command::Token(TokenCommand::Issue {
                scope,
                ttl,
                config: Some(config),
                principal: Some(principal),
            }) => {
                return ServerConfig::load(&config)?.issue_token(principal, scope, ttl.0);
            }
            Command::Init { .. }
            | Command::Clone { .. }
            | Command::Token(_)
            | Command::Workspace(_) => {}
        }
        let config = UserConfig::load()?;
        // The op log records the command line, without the program's name.
        let actor = Actor::new(env::args().skip(1).collect::<Vec<_>>().join(" "));
        match self.command {
            Command::Init { path } => {
                Workspace::init(&path, config, &actor)?;
            }
            Command::Clone { url, path, target } => {
                Workspace::clone(&url, path.as_deref(), &target, config, &actor)?;
            }
            Command::Workspace(command) => command.run(config, &actor)?,
            Command::Token(TokenCommand::Issue { scope, ttl, .. }) => {
                let cwd = env::current_dir().context("cannot read the current directory")?;
                Workspace::discover(&cwd, config, &actor)?.issue_token(scope, ttl)?;
            }
            Command::Serve { .. } | Command::Repo(_) => unreachable!("run above"),
        }
        Ok(())
    }
}

impl WorkspaceCommand {
    fn run(self, config: UserConfig, actor: &Actor) -> Result<()> {
        let cwd = env::current_dir().context("cannot read the current directory")?;
        let mut workspace = Workspace::discover(&cwd, config, actor)?;
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
            Self::Op(OpCommand::Log { limit }) => workspace.op_log(limit)?,
            Self::Undo => workspace.undo()?,
            Self::Remote { url } => workspace.set_remote(url.as_deref())?,
            Self::Fetch { target, change } => workspace.fetch(&target, &change)?,
            Self::Pull => workspace.pull()?,
            Self::Whoami => workspace.whoami()?,
            Self::Permissions => workspace.permissions()?,
            Self::Push { changes } => workspace.push(&changes)?,
            Self::Watch {
                kind,
                subject,
                since,
                local,
            } => workspace.watch(kind, subject, since, local)?,
        }
        Ok(())
    }
}
