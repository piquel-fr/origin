//! The `origin-ci` binary, which `origin ci` runs.

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use origin_ci::runner::{Outcome, Runner};
use origin_ci::state::State;
use origin_ci::worker::Worker;
use origin_shared::{ChangeId, RecordId, RevisionId};
use origin_src::config::UserConfig;
use origin_src::output;
use origin_src::plugin::Context;
use origin_src::remote::{Remote, RemoteUrl};

/// Run the checks of `checks.toml`, and issue their results.
#[derive(Debug, Parser)]
#[command(name = "origin ci", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a revision's checks here, and issue their results as you, in
    /// this repository. Checks a result of yours already covers, ran or
    /// promoted, are skipped. Fails if a check fails.
    Run {
        /// The revision: `@` for the working copy, a change's full id for
        /// its head, or a revision's full id.
        #[arg(default_value = "@")]
        rev: String,
    },
    /// Run the checks a remote's landings wait for, until stopped. Its
    /// token, in `$ORIGIN_TOKEN` or the configuration, must read the
    /// repository and annotate `ci.*`, such as one issued with `--scope
    /// "read /" --scope "annotate ci.*"`. Resumes where it stopped.
    Worker {
        /// The repository: `http://host[:port]/<repository>`, or a path.
        /// By default, this repository's remote.
        #[arg(long)]
        remote: Option<String>,
    },
}

fn main() -> ExitCode {
    output::exit_quietly_on_broken_pipe();
    match Cli::parse().run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

impl Cli {
    fn run(self) -> Result<ExitCode> {
        let config = UserConfig::load()?;
        let context = Context::from_env()?;
        match self.command {
            Command::Run { rev } => {
                let Some(context) = context else {
                    bail!("run `origin ci run` in a working copy");
                };
                let url = RemoteUrl::Path(context.repository.clone());
                let runner = Self::runner(&url, &config)?;
                let subject = Self::subject(&runner, &context, &rev)?;
                let outcomes = runner.check(subject, None)?;
                let mut failed = false;
                for outcome in &outcomes {
                    println!("{outcome}");
                    if let Outcome::Ran { output, .. } = outcome
                        && outcome.failed()
                    {
                        print!("{}", String::from_utf8_lossy(output));
                    }
                    failed |= outcome.failed();
                }
                Ok(if failed {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                })
            }
            Command::Worker { remote } => {
                let url = match (remote, context.and_then(|context| context.remote)) {
                    (Some(url), _) => RemoteUrl::parse(&url)?,
                    (None, Some(url)) => url,
                    (None, None) => bail!("give the worker a --remote"),
                };
                let runner = Self::runner(&url, &config)?;
                let mut worker = Worker::new(runner, State::open(&url)?)?;
                println!("Answering the check requests of {url}");
                worker.run()?;
                Ok(ExitCode::SUCCESS)
            }
        }
    }

    /// A runner of the checks of the repository at `url`, as the user.
    fn runner(url: &RemoteUrl, config: &UserConfig) -> Result<Runner> {
        let remote = Remote::open_for(url, config).with_context(|| format!("cannot open {url}"))?;
        Ok(Runner::new(remote, config.user(), State::open(url)?)?)
    }

    /// The record `rev` names: the working copy, a change's head, or a
    /// revision.
    fn subject(runner: &Runner, context: &Context, rev: &str) -> Result<RecordId> {
        if rev == "@" {
            return Ok(RecordId::Revision(context.revision));
        }
        if let Ok(revision) = rev.parse::<RevisionId>() {
            return Ok(RecordId::Revision(revision));
        }
        let change: ChangeId = rev
            .parse()
            .with_context(|| format!("`{rev}` is neither `@`, a change's id nor a revision's"))?;
        match runner.remote().revisions(change)?.heads[..] {
            [head] => Ok(RecordId::Revision(head)),
            _ => bail!(
                "change {} is divergent: name one of its heads",
                change.short()
            ),
        }
    }
}
