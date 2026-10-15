//! The `origin` command line: argument parsing. The commands themselves are
//! methods of [`Workspace`], [`Repository`] and [`ServerConfig`], which print
//! their output.

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use origin_shared::BranchName;

use crate::http::ServerConfig;
use crate::repository::{RepoConfig, Repository};
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
    /// Create a repository with a working copy.
    Init {
        /// Where to create it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Check out a branch of a remote repository. Only the latest commit is
    /// downloaded; history is fetched when needed.
    Clone {
        /// The repository: `http://host[:port]/<repository>`, or a path.
        url: String,
        /// Where to check it out. Defaults to the repository's name.
        path: Option<PathBuf>,
        /// The branch to check out.
        #[arg(short, long)]
        branch: Option<BranchName>,
    },
    /// Serve repositories over HTTP.
    Serve {
        /// The server configuration file.
        #[arg(short, long, default_value = "origin-server.toml")]
        config: PathBuf,
    },
    /// Manage repositories hosted by a server.
    #[command(subcommand)]
    Repo(RepoCommand),
    #[command(flatten)]
    Workspace(WorkspaceCommand),
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Create an empty repository for a server to host.
    Create {
        /// Where to create it.
        path: PathBuf,
        /// What the repository holds.
        #[arg(short, long, default_value = "")]
        description: String,
    },
}

/// Commands run in the repository containing the current directory.
#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// List changes in the working copy.
    Status,
    /// Show changes in the working copy, line by line.
    Diff,
    /// Commit every change in the working copy. There is no staging area.
    Commit {
        /// The commit message.
        #[arg(short, long)]
        message: String,
    },
    /// Show the history of the current branch.
    Log {
        /// Show at most this many commits.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// Show the remote, or set it.
    Remote {
        /// The new remote: `http://host[:port]/<repository>`, or a path.
        url: Option<String>,
    },
    /// Fast-forward the current branch to the remote's.
    Pull,
    /// Send the current branch's new commits to the remote.
    Push,
}

impl Cli {
    /// Runs the command, printing its output.
    ///
    /// # Errors
    ///
    /// Fails if the command fails; the error is meant for the user.
    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Init { path } => Workspace::init(&path)?,
            Command::Clone { url, path, branch } => {
                Workspace::clone(&url, path.as_deref(), branch).await?;
            }
            Command::Serve { config } => ServerConfig::load(&config)?.serve().await?,
            Command::Repo(RepoCommand::Create { path, description }) => {
                Repository::create(&path, &RepoConfig { description })?;
            }
            Command::Workspace(command) => command.run().await?,
        }
        Ok(())
    }
}

impl WorkspaceCommand {
    async fn run(self) -> Result<()> {
        let cwd = env::current_dir().context("cannot read the current directory")?;
        let mut workspace = Workspace::discover(&cwd)?;
        match self {
            Self::Status => workspace.status()?,
            Self::Diff => workspace.diff()?,
            Self::Commit { message } => workspace.commit(message)?,
            Self::Log { limit } => workspace.log(limit).await?,
            Self::Remote { url: None } => workspace.show_remote(),
            Self::Remote { url: Some(url) } => workspace.set_remote(&url)?,
            Self::Pull => workspace.pull().await?,
            Self::Push => workspace.push().await?,
        }
        Ok(())
    }
}
