//! The user's configuration, in `~/.config/origin/config.toml`:
//!
//! ```toml
//! [user]
//! name = "Ada Lovelace"
//! email = "ada@example.com"
//!
//! [tokens] # by server URL
//! "http://localhost:7878" = "a long random secret"
//! ```
//!
//! `ORIGIN_USER_NAME`, `ORIGIN_USER_EMAIL`, and `ORIGIN_TOKEN` override it.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use origin_shared::Signature;
use serde::Deserialize;

/// The user's configuration.
#[derive(Debug, Default, Deserialize)]
pub struct UserConfig {
    user: Option<Author>,
    #[serde(default)]
    tokens: BTreeMap<String, String>,
}

impl UserConfig {
    /// Reads the configuration. A missing file is an empty configuration.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is invalid.
    pub fn load() -> Result<Self, UserConfigError> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(UserConfigError::Io { path, source }),
        };
        toml::from_str(&content).map_err(|source| UserConfigError::Invalid { path, source })
    }

    fn path() -> Option<PathBuf> {
        let base = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| Path::new(&home).join(".config")))?;
        Some(base.join("origin/config.toml"))
    }

    /// Who is making changes.
    ///
    /// # Errors
    ///
    /// Fails if it is not configured.
    pub fn author(self) -> Result<Author, UserConfigError> {
        if let (Ok(name), Ok(email)) = (env::var("ORIGIN_USER_NAME"), env::var("ORIGIN_USER_EMAIL"))
        {
            return Ok(Author { name, email });
        }
        self.user.ok_or(UserConfigError::NoAuthor)
    }

    /// The token to authenticate to a server with, by its URL.
    #[must_use]
    pub fn token(&self, server: &str) -> Option<String> {
        env::var("ORIGIN_TOKEN")
            .ok()
            .or_else(|| self.tokens.get(server.trim_end_matches('/')).cloned())
    }
}

/// Who is making changes.
#[derive(Debug, Deserialize)]
pub struct Author {
    name: String,
    email: String,
}

impl Author {
    /// Signs as this author, now.
    #[must_use]
    pub fn sign(self) -> Signature {
        Signature::now(self.name, self.email)
    }
}

/// A failure to read the user's configuration.
#[derive(Debug, thiserror::Error)]
pub enum UserConfigError {
    /// The author is not configured.
    #[error(
        "who are you? Set ORIGIN_USER_NAME and ORIGIN_USER_EMAIL, or add \
         `[user]` with `name` and `email` to ~/.config/origin/config.toml"
    )]
    NoAuthor,
    /// The file is invalid.
    #[error("invalid configuration in {}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        source: toml::de::Error,
    },
    /// The file cannot be read.
    #[error("cannot read {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}
