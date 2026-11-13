//! The `origin-ci` binary, which `origin ci` runs.

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use origin_ci::checks::Check;
use origin_ci::runner::{Outcome, Runner};
use origin_ci::state::State;
use origin_ci::worker::Worker;
use origin_shared::api::At;
use origin_shared::{ChangeId, RecordId, RepoPath, RevisionId};
use origin_src::config::UserConfig;
use origin_src::output;
use origin_src::plugin::Context;
use origin_src::remote::{Remote, RemoteUrl};

/// Run the checks of `checks.toml`, or a command as one, and issue their
/// results.
#[derive(Debug, Parser)]
#[command(name = "origin ci", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a revision's checks here, as its own `checks.toml` defines them,
    /// and issue their results as you, in this repository. Checks a result
    /// of yours already covers, ran or promoted, are skipped. Fails if a
    /// check fails.
    Run {
        /// The revision: `@` for the working copy, a change's full id for
        /// its head, or a revision's full id.
        #[arg(default_value = "@")]
        rev: String,
    },
    /// Run a command as a check of a revision, as `run` runs those of
    /// `checks.toml`: in a checkout of it, issuing a `ci.<name>` result as
    /// you, which supersedes your previous one. Runs even if a result
    /// already covers it. Fails if the check fails.
    Exec {
        /// The check's name: its results are of the kind `ci.<name>`.
        name: String,
        /// The revision, as for `run`.
        #[arg(long, default_value = "@")]
        rev: String,
        /// The files and directories it reads: the whole snapshot by
        /// default. Its result is promoted where they're the same.
        #[arg(long, num_args = 1..)]
        inputs: Vec<RepoPath>,
        /// A shell command whose output identifies the tools it runs with.
        #[arg(long)]
        toolchain: Option<String>,
        /// It also reads what isn't in the snapshot, such as the network, so
        /// its result is never promoted.
        #[arg(long)]
        external: bool,
        /// The command, after `--`: its words are quoted for `sh -c`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Run the checks a remote's landings wait for, until stopped. Its
    /// token, in `$ORIGIN_TOKEN` or the configuration, must read the
    /// repository and annotate `ci.*`, such as one issued with `--scope
    /// "read /" --scope "annotate ci.*"`. Checks don't inherit
    /// `$ORIGIN_TOKEN`, so they don't print it by accident, but they run as
    /// the worker's user and can read whatever it can, the token included:
    /// run the worker as a user of its own, with a token scoped as above.
    /// Runs each check as the
    /// `checks.toml` at the tip of the target landed on defines it, never
    /// the candidate's. Answers the landings that wait first, and outlives
    /// the remote going away.
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
                let (runner, subject) = Self::here(context.as_ref(), &config, "run", &rev)?;
                let outcomes = runner.check(subject, &At::Record(subject), None)?;
                Ok(Self::report(&outcomes))
            }
            Command::Exec {
                name,
                rev,
                inputs,
                toolchain,
                external,
                command,
            } => {
                let (runner, subject) = Self::here(context.as_ref(), &config, "exec", &rev)?;
                let check = Check::adhoc(&name, &command, inputs, toolchain, external)?;
                Ok(Self::report(&[runner.exec(subject, &check)?]))
            }
            Command::Worker { remote } => {
                let url = match (remote, context.and_then(|context| context.remote)) {
                    (Some(url), _) => RemoteUrl::parse(&url)?,
                    (None, Some(url)) => url,
                    (None, None) => bail!("give the worker a --remote"),
                };
                let runner = Self::runner(&url, &config)?;
                let mut worker = Worker::new(runner, &State::open(&url)?)?;
                println!("Answering the check requests of {url}");
                worker.run()?;
                Ok(ExitCode::SUCCESS)
            }
        }
    }

    /// A runner of the checks of the repository `origin ci <command>` runs
    /// in, as the user, and the record `rev` names there.
    fn here(
        context: Option<&Context>,
        config: &UserConfig,
        command: &str,
        rev: &str,
    ) -> Result<(Runner, RecordId)> {
        let Some(context) = context else {
            bail!("run `origin ci {command}` in a working copy");
        };
        let url = RemoteUrl::Path(context.repository.clone());
        let runner = Self::runner(&url, config)?;
        let subject = Self::subject(&runner, context, rev)?;
        Ok((runner, subject))
    }

    /// Prints each outcome, and a failed check's output: fails if one did.
    fn report(outcomes: &[Outcome]) -> ExitCode {
        let mut failed = false;
        for outcome in outcomes {
            println!("{outcome}");
            if let Outcome::Ran { output, .. } = outcome
                && outcome.failed()
            {
                print!("{}", String::from_utf8_lossy(output));
            }
            failed |= outcome.failed();
        }
        if failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
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
