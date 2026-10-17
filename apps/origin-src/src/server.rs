//! `origin serve`: one node hosting repositories over HTTP.
//!
//! A server has a node key of its own, and is the authority of the targets
//! its repositories' policies name it for: `origin repo create` makes it the
//! authority of `policy` and `main`. The repositories have no working copy:
//! each is a store, in the directory the configuration names.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use origin_shared::PrincipalName;
use origin_shared::api::{RepoName, RepoSummary};
use serde::Deserialize;

use crate::config::KeyFile;
use crate::http::Api;
use crate::policy::Policy;
use crate::service::{Caller, Node, OriginService, ServiceError};
use crate::store::Store;

/// Server configuration, read from a TOML file:
///
/// ```toml
/// listen = "127.0.0.1:7878"
/// node = "hosted"           # the server's node name
/// key = "node.key"          # its secret key, created if missing
///
/// [repos]
/// origin = "repos/origin"   # name = path
/// ```
///
/// Paths are relative to the file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The address to listen on. Port 0 picks a free port.
    pub listen: SocketAddr,
    /// The server's node name.
    pub node: PrincipalName,
    /// The file holding the server's node key.
    #[serde(default = "ServerConfig::default_key")]
    pub key: PathBuf,
    /// The repositories to serve, by name.
    #[serde(default)]
    pub repos: BTreeMap<RepoName, PathBuf>,
}

impl ServerConfig {
    fn default_key() -> PathBuf {
        PathBuf::from("node.key")
    }

    /// Reads the configuration, resolving its paths relative to it.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is invalid.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut config: Self = toml::from_str(&content)
            .with_context(|| format!("invalid configuration in {}", path.display()))?;
        // A file in the current directory has an empty parent.
        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        config.key = base.join(&config.key);
        for repo in config.repos.values_mut() {
            *repo = base.join(&*repo);
        }
        Ok(config)
    }

    /// The server's node, with its key, generated the first time.
    ///
    /// # Errors
    ///
    /// Fails if the key cannot be read, or generated and saved.
    pub fn node(&self) -> anyhow::Result<Node> {
        Ok(Node {
            name: self.node.clone(),
            key: Arc::new(KeyFile(self.key.clone()).load()?),
        })
    }

    /// `origin repo create`: creates an empty repository for this server
    /// to host in `root`, with the server's node the authority of `policy`
    /// and `main`.
    ///
    /// # Errors
    ///
    /// Fails if `root` isn't empty, or the key or repository cannot be
    /// written.
    pub fn create_repo(&self, root: &Path) -> anyhow::Result<()> {
        let node = self.node()?;
        let policy = Policy::initial(&node.name, &node.key.public());
        OriginService::create(root.to_owned(), node, &policy)?;
        println!(
            "Created a repository in {}: node:{} is the authority of `policy` and `main`",
            root.display(),
            self.node
        );
        println!("Name it in the [repos] of the configuration to serve it");
        Ok(())
    }

    /// `origin serve`: serves the repositories until interrupted, logging
    /// requests to stderr. Prints the address it listens on, which tells
    /// callers that chose port 0 where to connect.
    ///
    /// # Errors
    ///
    /// Fails if a repository cannot be opened or the address bound.
    pub fn serve(self) -> anyhow::Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_writer(std::io::stderr)
            .init();
        let node = self.node()?;
        let repositories = Repositories::open(self.repos, &node)?;
        repositories.compact_in_background();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("cannot start the server's runtime")?;
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind(self.listen)
                .await
                .with_context(|| format!("cannot listen on {}", self.listen))?;
            println!("Listening on http://{}", listener.local_addr()?);
            axum::serve(listener, Api::router(repositories))
                .with_graceful_shutdown(async {
                    // If the signal handler cannot be installed, run until
                    // killed.
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await
                .context("server failed")
        })
    }
}

/// The repositories a server hosts, each served by the server's node and
/// kept open while it runs.
#[derive(Debug, Default)]
pub struct Repositories {
    repos: BTreeMap<RepoName, OriginService>,
}

impl Repositories {
    /// How often [`Repositories::compact_in_background`] checks.
    const COMPACT_EVERY: Duration = Duration::from_mins(1);

    /// Opens the repositories at the given paths, served by `node`.
    ///
    /// # Errors
    ///
    /// Fails if one has no repository.
    pub fn open(repos: BTreeMap<RepoName, PathBuf>, node: &Node) -> anyhow::Result<Self> {
        let repos = repos
            .into_iter()
            .map(|(name, path)| {
                let service = OriginService::open(path, node.clone())
                    .with_context(|| format!("cannot serve repository `{name}`"))?;
                Ok((name, service))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { repos })
    }

    /// Compacts each repository once enough loose objects pile up in it
    /// (see [`Store::compact_if_due`]), checking every
    /// [`Repositories::COMPACT_EVERY`] on a thread of its own, for as long
    /// as the process runs. A failed compaction loses nothing, so it is
    /// logged, and the next check tries again.
    pub fn compact_in_background(&self) {
        let stores: Vec<(RepoName, Store)> = self
            .repos
            .iter()
            .map(|(name, service)| (name.clone(), service.store().clone()))
            .collect();
        thread::spawn(move || {
            loop {
                thread::sleep(Self::COMPACT_EVERY);
                for (repo, store) in &stores {
                    match store.compact_if_due() {
                        Ok(Some(compaction)) => tracing::info!(
                            %repo,
                            packed = compaction.packed,
                            merged = compaction.merged,
                            "compacted"
                        ),
                        Ok(None) => {}
                        Err(error) => tracing::error!(
                            %repo,
                            error = %anyhow::Error::from(error),
                            "compaction failed"
                        ),
                    }
                }
            }
        });
    }

    /// Hosts `service` as `name`.
    pub fn insert(&mut self, name: RepoName, service: OriginService) {
        self.repos.insert(name, service);
    }

    /// Lists the repositories, by name.
    #[must_use]
    pub fn list(&self, _caller: &Caller) -> Vec<RepoSummary> {
        self.repos
            .keys()
            .map(|name| RepoSummary { name: name.clone() })
            .collect()
    }

    /// The repository `name`.
    ///
    /// # Errors
    ///
    /// Fails if the server hosts none by that name.
    pub fn get(&self, _caller: &Caller, name: &RepoName) -> Result<&OriginService, ServiceError> {
        self.repos
            .get(name)
            .ok_or_else(|| ServiceError::UnknownRepository(name.clone()))
    }
}
