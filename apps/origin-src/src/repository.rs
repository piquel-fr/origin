//! Repositories served by an origin server.
//!
//! A repository is a self-contained directory; the server only knows where
//! it is.
//!
//! ```text
//! <repository>/
//!   repo.toml   the description and permissions
//!   branches    branches (see `RefStore`)
//!   objects/    objects (see `ObjectStore`)
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use origin_shared::access::Permissions;
use serde::{Deserialize, Serialize};

use crate::refs::RefStore;
use crate::store::ObjectStore;

/// Distinguishes temporary configuration files written concurrently.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A repository on disk.
#[derive(Debug)]
pub struct Repository {
    root: PathBuf,
    objects: ObjectStore,
    branches: RefStore,
}

/// `repo.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RepoConfig {
    /// What the repository holds.
    #[serde(default)]
    pub description: String,
    /// Who may read and write what (see [`Permissions`]).
    #[serde(default, skip_serializing_if = "Permissions::is_empty")]
    pub permissions: Permissions,
}

impl Repository {
    const CONFIG: &str = "repo.toml";

    /// `origin repo create`: creates an empty repository in `root`, which
    /// must not exist or be empty.
    ///
    /// # Errors
    ///
    /// Fails if `root` is not empty or cannot be written.
    ///
    /// # Panics
    ///
    /// Panics if the configuration cannot be represented as TOML, which its
    /// fields rule out.
    pub fn create(root: &Path, config: &RepoConfig) -> Result<Self, RepositoryError> {
        let io_error = |source| RepositoryError::Io {
            path: root.to_owned(),
            source,
        };
        fs::create_dir_all(root).map_err(io_error)?;
        if fs::read_dir(root).map_err(io_error)?.next().is_some() {
            return Err(RepositoryError::NotEmpty(root.to_owned()));
        }
        Self::write_config(root, config)?;
        println!("Created a repository in {}", root.display());
        Self::open(root)
    }

    /// Replaces `repo.toml` atomically.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be written.
    ///
    /// # Panics
    ///
    /// Panics if the configuration cannot be represented as TOML, which its
    /// fields rule out.
    pub fn save_config(&self, config: &RepoConfig) -> Result<(), RepositoryError> {
        Self::write_config(&self.root, config)
    }

    fn write_config(root: &Path, config: &RepoConfig) -> Result<(), RepositoryError> {
        let content = toml::to_string(config).expect("the config serializes to TOML");
        let path = root.join(Self::CONFIG);
        // Unique per writer, so concurrent saves never share a file.
        let temp = root.join(format!(
            ".repo.toml.{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&temp, content)
            .and_then(|()| fs::rename(&temp, &path))
            .map_err(|source| RepositoryError::Io { path, source })
    }

    /// Opens the repository in `root`.
    ///
    /// # Errors
    ///
    /// Fails if `root` has no repository.
    pub fn open(root: &Path) -> Result<Self, RepositoryError> {
        if !root.join(Self::CONFIG).is_file() {
            return Err(RepositoryError::NotARepository(root.to_owned()));
        }
        Ok(Self {
            root: root.to_owned(),
            objects: ObjectStore::new(root.join("objects")),
            branches: RefStore::new(root.join("branches")),
        })
    }

    /// Reads `repo.toml`. It is read on every call, so edits apply without
    /// restarting the server.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is invalid.
    pub fn config(&self) -> Result<RepoConfig, RepositoryError> {
        let path = self.root.join(Self::CONFIG);
        let content = fs::read_to_string(&path).map_err(|source| RepositoryError::Io {
            path: path.clone(),
            source,
        })?;
        toml::from_str(&content).map_err(|source| RepositoryError::Config { path, source })
    }

    /// The repository's objects.
    #[must_use]
    pub fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    /// The repository's branches.
    #[must_use]
    pub fn branches(&self) -> &RefStore {
        &self.branches
    }
}

/// A failure to create or open a repository.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    /// The directory has no repository.
    #[error("{} is not an origin repository", .0.display())]
    NotARepository(PathBuf),
    /// `create` was pointed at a directory with files in it.
    #[error("{} is not empty", .0.display())]
    NotEmpty(PathBuf),
    /// `repo.toml` is invalid.
    #[error("invalid repository configuration in {}", path.display())]
    Config {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        source: toml::de::Error,
    },
    /// A file cannot be accessed.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}
