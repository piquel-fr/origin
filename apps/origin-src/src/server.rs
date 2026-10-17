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
use origin_shared::access::{Capability, Token};
use origin_shared::api::{RepoName, RepoSummary};
use origin_shared::{Principal, PrincipalName, RepoPath};
use serde::Deserialize;

use crate::auth::{Credentials, StaticTokens};
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
///
/// [tokens]                  # static tokens, by principal
/// "user:alice" = "a long random secret"
/// ```
///
/// Paths are relative to the file. Callers without a token are anonymous,
/// and only read; the server also takes the scoped tokens it signed (see
/// [`ServerConfig::issue_token`]).
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
    /// Static tokens.
    #[serde(default)]
    pub tokens: StaticTokens,
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

    /// `origin token issue --config <file> --as <principal>`: signs a
    /// token acting for `principal` with `scope`, valid for `ttl`, with the
    /// server's key, and prints it. The token acts with the principal's
    /// rights in each repository, narrowed to the scope, so an operator
    /// can hand agents identities of their own.
    ///
    /// # Errors
    ///
    /// Fails if the key cannot be read or generated.
    pub fn issue_token(
        &self,
        principal: Principal,
        scope: Vec<Capability>,
        ttl: Duration,
    ) -> anyhow::Result<()> {
        let node = self.node()?;
        println!("{}", Token::new(principal, scope, ttl).sign(&node.key));
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
        let credentials = Credentials::new(node.clone(), Box::new(self.tokens));
        let repositories = Repositories::open(self.repos, &node, credentials)?;
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
/// kept open while it runs, and how it tells its callers apart.
#[derive(Debug)]
pub struct Repositories {
    repos: BTreeMap<RepoName, OriginService>,
    credentials: Credentials,
}

impl Repositories {
    /// How often [`Repositories::compact_in_background`] checks.
    const COMPACT_EVERY: Duration = Duration::from_mins(1);

    /// No repositories yet, for callers `credentials` tells apart.
    #[must_use]
    pub fn new(credentials: Credentials) -> Self {
        Self {
            repos: BTreeMap::new(),
            credentials,
        }
    }

    /// Opens the repositories at the given paths, served by `node`.
    ///
    /// # Errors
    ///
    /// Fails if one has no repository.
    pub fn open(
        repos: BTreeMap<RepoName, PathBuf>,
        node: &Node,
        credentials: Credentials,
    ) -> anyhow::Result<Self> {
        let mut repositories = Self::new(credentials);
        for (name, path) in repos {
            let service = OriginService::open(path, node.clone())
                .with_context(|| format!("cannot serve repository `{name}`"))?;
            repositories.insert(name, service);
        }
        Ok(repositories)
    }

    /// How the server tells its callers apart.
    #[must_use]
    pub fn credentials(&self) -> &Credentials {
        &self.credentials
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

    /// Lists the repositories the caller sees, by name.
    ///
    /// # Errors
    ///
    /// Fails if a repository's policy cannot be read.
    pub fn list(&self, caller: &Caller) -> Result<Vec<RepoSummary>, ServiceError> {
        let mut seen = Vec::new();
        for (name, service) in &self.repos {
            if Self::sees(service, caller)? {
                seen.push(RepoSummary { name: name.clone() });
            }
        }
        Ok(seen)
    }

    /// The repository `name`, if the caller sees it: invisible repositories
    /// don't exist.
    ///
    /// # Errors
    ///
    /// Fails if the server hosts none by that name the caller sees, or its
    /// policy cannot be read.
    pub fn get(&self, caller: &Caller, name: &RepoName) -> Result<&OriginService, ServiceError> {
        match self.repos.get(name) {
            Some(service) if Self::sees(service, caller)? => Ok(service),
            _ => Err(ServiceError::UnknownRepository(name.clone())),
        }
    }

    /// Whether the caller reads some path of the repository `service`
    /// serves.
    fn sees(service: &OriginService, caller: &Caller) -> Result<bool, ServiceError> {
        Ok(service.rights(caller)?.paths.reads_under(&RepoPath::root()))
    }
}
