//! Plugins: `origin <name>` runs `origin-<name>` from `PATH` when `<name>`
//! isn't a built-in command, as cargo and git do with theirs. The plugin
//! replaces the `origin` process, so its standard streams, exit code and
//! signals are the command's.
//!
//! Run in a working copy, the plugin gets the repository's [`Context`] in
//! its environment, after its draft is recorded:
//!
//! - `ORIGIN_REPOSITORY`: the directory holding the repository's store, its
//!   main working copy's `.origin`, which a remote on a local path accepts;
//! - `ORIGIN_REVISION`: the revision the working copy is, in full;
//! - `ORIGIN_REMOTE`: the repository's remote, if it has one.
//!
//! Outside a working copy, none of them is set.

use std::convert::Infallible;
use std::env;
use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use origin_shared::RevisionId;

use crate::remote::{RemoteError, RemoteUrl};

/// What a plugin learns of the repository it runs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    /// The directory holding the repository's store.
    pub repository: PathBuf,
    /// The revision the working copy is.
    pub revision: RevisionId,
    /// The repository's remote, if it has one.
    pub remote: Option<RemoteUrl>,
}

impl Context {
    /// The variable naming [`Context::repository`].
    pub const REPOSITORY: &str = "ORIGIN_REPOSITORY";
    /// The variable naming [`Context::revision`].
    pub const REVISION: &str = "ORIGIN_REVISION";
    /// The variable naming [`Context::remote`].
    pub const REMOTE: &str = "ORIGIN_REMOTE";

    /// The context `origin` gave this process, if it ran it in a working
    /// copy.
    ///
    /// # Errors
    ///
    /// Fails if a variable is set but malformed.
    pub fn from_env() -> Result<Option<Self>, ContextError> {
        let Some(repository) = env::var_os(Self::REPOSITORY) else {
            return Ok(None);
        };
        let read = |name: &'static str| {
            env::var(name).map_err(|source| ContextError::Variable { name, source })
        };
        let revision = read(Self::REVISION)?;
        let revision = revision
            .parse()
            .map_err(|_| ContextError::Revision(revision))?;
        let remote = match env::var_os(Self::REMOTE) {
            Some(_) => Some(RemoteUrl::parse(&read(Self::REMOTE)?)?),
            None => None,
        };
        Ok(Some(Self {
            repository: repository.into(),
            revision,
            remote,
        }))
    }

    /// Sets the context in the environment of `command`.
    fn apply(&self, command: &mut Command) {
        command
            .env(Self::REPOSITORY, &self.repository)
            .env(Self::REVISION, self.revision.to_string());
        match &self.remote {
            Some(remote) => command.env(Self::REMOTE, remote.to_string()),
            None => command.env_remove(Self::REMOTE),
        };
    }
}

/// A command that isn't built in: the plugin's name, then its arguments.
#[derive(Debug)]
pub struct Plugin(pub Vec<OsString>);

impl Plugin {
    /// Replaces this process with the plugin, giving it `context`.
    ///
    /// # Errors
    ///
    /// Fails if no `origin-<name>` is on `PATH`, or it cannot be run.
    pub fn exec(self, context: Option<&Context>) -> Result<Infallible, PluginError> {
        let mut args = self.0.into_iter();
        let name = args.next().unwrap_or_default();
        let mut program = OsString::from("origin-");
        program.push(&name);
        let mut command = Command::new(&program);
        command.args(args);
        // A plugin run from another's working copy gets only its own.
        for variable in [Context::REPOSITORY, Context::REVISION, Context::REMOTE] {
            command.env_remove(variable);
        }
        if let Some(context) = context {
            context.apply(&mut command);
        }
        let name = name.to_string_lossy().into_owned();
        Err(match command.exec() {
            error if error.kind() == io::ErrorKind::NotFound => PluginError::NotFound(name),
            source => PluginError::Exec { name, source },
        })
    }
}

/// A plugin that cannot run.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// Neither a built-in command nor a plugin has the name.
    #[error("`{0}` is not an origin command, and no `origin-{0}` is on PATH")]
    NotFound(String),
    /// The plugin cannot be run.
    #[error("cannot run `origin-{name}`")]
    Exec {
        /// The plugin's name.
        name: String,
        /// Why.
        source: io::Error,
    },
}

/// A context in the environment that cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// A variable isn't UTF-8.
    #[error("${name} is not valid")]
    Variable {
        /// Its name.
        name: &'static str,
        /// What is wrong.
        source: env::VarError,
    },
    /// `ORIGIN_REVISION` isn't a revision id.
    #[error("$ORIGIN_REVISION is not a revision id: {0:?}")]
    Revision(String),
    /// `ORIGIN_REMOTE` isn't a remote.
    #[error(transparent)]
    Remote(#[from] RemoteError),
}
