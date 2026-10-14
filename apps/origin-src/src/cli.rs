//! The `origin` command line: argument parsing. The commands themselves are
//! methods of [`Workspace`], which print their output.

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

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
    /// Create a repository.
    Init {
        /// Where to create it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    #[command(flatten)]
    Workspace(WorkspaceCommand),
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
}

impl Cli {
    /// Runs the command, printing its output.
    ///
    /// # Errors
    ///
    /// Fails if the command fails; the error is meant for the user.
    pub fn run(self) -> Result<()> {
        match self.command {
            Command::Init { path } => Workspace::init(&path)?,
            Command::Workspace(command) => command.run()?,
        }
        Ok(())
    }
}

impl WorkspaceCommand {
    fn run(self) -> Result<()> {
        let cwd = env::current_dir().context("cannot read the current directory")?;
        let workspace = Workspace::discover(&cwd)?;
        match self {
            Self::Status => workspace.status()?,
            Self::Diff => workspace.diff()?,
            Self::Commit { message } => workspace.commit(message)?,
            Self::Log { limit } => workspace.log(limit)?,
        }
        Ok(())
    }
}
