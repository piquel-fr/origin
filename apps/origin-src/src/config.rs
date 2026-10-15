//! The user's configuration, kept outside every repository in
//! `$ORIGIN_CONFIG_DIR`, or else `origin` in `$XDG_CONFIG_HOME` or
//! `~/.config`:
//!
//! - `config.toml`: `user = "alice"`, who authors revisions (by default
//!   `$USER`), and `node = "laptop"`, this node's name (by default the
//!   user's).
//! - `node.key`: this node's secret ed25519 key, created by the first
//!   `origin init`, readable only by its owner.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process;

use origin_platform::{Durability, Native};
use origin_shared::{InvalidName, NodeKey, Principal, PrincipalName};
use serde::Deserialize;

/// The user's configuration.
#[derive(Clone, Debug)]
pub struct UserConfig {
    dir: PathBuf,
    user: PrincipalName,
    node: PrincipalName,
}

/// `config.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    user: Option<String>,
    node: Option<String>,
}

impl UserConfig {
    /// The length of `node.key`: the raw secret key.
    const KEY_LEN: usize = 32;

    /// Loads the configuration from its directory.
    ///
    /// # Errors
    ///
    /// Fails if there's no configuration directory, `config.toml` cannot be
    /// read or is invalid, or no user name is set.
    pub fn load() -> Result<Self, ConfigError> {
        let dir = env::var_os("ORIGIN_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| env::var_os("XDG_CONFIG_HOME").map(|dir| Path::new(&dir).join("origin")))
            .or_else(|| env::var_os("HOME").map(|home| Path::new(&home).join(".config/origin")))
            .ok_or(ConfigError::NoDir)?;
        let path = dir.join("config.toml");
        let file: ConfigFile = match fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).map_err(|source| ConfigError::Toml {
                path: path.clone(),
                source,
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => ConfigFile::default(),
            Err(source) => return Err(ConfigError::Io { path, source }),
        };
        let name = |name: String| {
            PrincipalName::new(name).map_err(|source| ConfigError::InvalidName {
                path: path.clone(),
                source,
            })
        };
        let user = file
            .user
            .or_else(|| env::var("USER").ok())
            .ok_or_else(|| ConfigError::NoUser(path.clone()))
            .and_then(name)?;
        let node = file
            .node
            .map(name)
            .transpose()?
            .unwrap_or_else(|| user.clone());
        Ok(Self { dir, user, node })
    }

    /// The user, who authors revisions and issues statements.
    #[must_use]
    pub fn user(&self) -> Principal {
        Principal::User(self.user.clone())
    }

    /// This node's name.
    #[must_use]
    pub fn node(&self) -> &PrincipalName {
        &self.node
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join("node.key")
    }

    /// This node's key, generated and saved durably the first time.
    ///
    /// # Errors
    ///
    /// Fails if the key cannot be read, is malformed, or cannot be
    /// generated and saved.
    pub fn node_key(&self) -> Result<NodeKey, ConfigError> {
        match self.read_key() {
            Err(ConfigError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                self.create_key()
            }
            result => result,
        }
    }

    fn read_key(&self) -> Result<NodeKey, ConfigError> {
        let path = self.key_path();
        let bytes = fs::read(&path).map_err(|source| ConfigError::Io {
            path: path.clone(),
            source,
        })?;
        let secret: [u8; Self::KEY_LEN] = bytes
            .try_into()
            .map_err(|_| ConfigError::MalformedKey(path))?;
        Ok(NodeKey::from_bytes(&secret))
    }

    /// Generates a key and saves it: written and synced under a temporary
    /// name, then linked into place, which fails if another process saved
    /// one first. That one is used instead.
    fn create_key(&self) -> Result<NodeKey, ConfigError> {
        let io_error = |path: &Path| {
            let path = path.to_owned();
            move |source| ConfigError::Io { path, source }
        };
        fs::create_dir_all(&self.dir).map_err(io_error(&self.dir))?;
        let key = NodeKey::generate().map_err(ConfigError::Random)?;
        let temp = self.dir.join(format!(".node.key-{}", process::id()));
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .and_then(|mut file| {
                file.write_all(&key.to_bytes())?;
                Native::default().sync_file(&file)
            })
            .and_then(|()| fs::hard_link(&temp, self.key_path()));
        // Best effort: a leftover is never read.
        let _ = fs::remove_file(&temp);
        match written {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return self.read_key(),
            Err(source) => return Err(io_error(&temp)(source)),
        }
        Native::default()
            .sync_dir(&self.dir)
            .map_err(io_error(&self.dir))?;
        Ok(key)
    }
}

/// A failure to load the user's configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Neither `ORIGIN_CONFIG_DIR`, `XDG_CONFIG_HOME` nor `HOME` is set.
    #[error("cannot find the configuration directory: set ORIGIN_CONFIG_DIR or HOME")]
    NoDir,
    /// No user name is configured.
    #[error("who are you? Add `user = \"<name>\"` to {}", .0.display())]
    NoUser(PathBuf),
    /// A file cannot be read or written.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// `config.toml` is invalid.
    #[error("invalid configuration in {}", path.display())]
    Toml {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        source: toml::de::Error,
    },
    /// A configured name is invalid.
    #[error("invalid configuration in {}", path.display())]
    InvalidName {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        source: InvalidName,
    },
    /// `node.key` isn't a key.
    #[error("{} is not a node key", .0.display())]
    MalformedKey(PathBuf),
    /// The operating system has no randomness to generate a key with.
    #[error("cannot generate a node key")]
    Random(#[source] getrandom::Error),
}
