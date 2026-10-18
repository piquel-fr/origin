//! Remotes: the repositories a node shares its work with.
//!
//! A remote is a server over HTTP, `http://host[:port]/<repository>`, or a
//! repository on a local path: a server's repository, or a working copy's.
//! A local one is served by an in-process [`OriginService`] run by this
//! node, so both answer the same calls, and [`sync`](crate::sync) never
//! tells them apart. Calls block: an HTTP remote runs the SDK on a runtime
//! of its own.

use std::fmt;
use std::path::{self, Path, PathBuf};

use origin_client::Client;
use origin_shared::access::Rights;
use origin_shared::api::{
    Annotation, BrowseQuery, ErrorCode, EventsQuery, FetchRecords, FetchSnapshot, Inventory,
    InventoryRequest, IssuedToken, LandRequest, Landings, Listing, LogEntry, LogQuery, NewTarget,
    PromoteRequest, PutQuery, RepoName, Revisions, StatementsQuery, Stored, TargetInfo,
    TokenRequest,
};
use origin_shared::event::Page;
use origin_shared::landing::Pending;
use origin_shared::pack::{Pack, Part};
use origin_shared::{ChangeId, Head};

use crate::config::{ConfigError, UserConfig};
use crate::service::{Caller, Node, OriginService, ServiceError};
use crate::workspace::Workspace;

/// A repository to share work with.
#[derive(Debug)]
pub enum Remote {
    /// A repository on a server.
    Http {
        /// The runtime the SDK's requests run on.
        runtime: tokio::runtime::Runtime,
        /// The repository.
        repo: origin_client::Repo,
    },
    /// A repository on a local path, served in-process.
    Local(Box<OriginService>),
}

/// Defines the remote's calls, each answered by an SDK method over HTTP, or
/// by a service method in-process, called as the node's owner.
macro_rules! calls {
    ($(
        $(#[doc = $doc:literal])*
        fn $name:ident($($arg:ident: $type:ty),*) -> $output:ty =
            $http:ident($($http_arg:expr),*), $local:ident($($local_arg:expr),*);
    )*) => {
        impl Remote {$(
            $(#[doc = $doc])*
            ///
            /// # Errors
            ///
            /// Fails if the remote cannot be reached or refuses.
            pub fn $name(&self, $($arg: $type),*) -> Result<$output, RemoteError> {
                match self {
                    Self::Http { runtime, repo } => {
                        Ok(runtime.block_on(repo.$http($($http_arg),*))?)
                    }
                    Self::Local(service) => Ok(service.$local(&Caller::Owner, $($local_arg),*)?),
                }
            }
        )*}
    };
}

calls! {
    /// Every target, with its tip, authority and requirements.
    fn targets() -> Vec<TargetInfo> = targets(), targets();
    /// The acceptances of a target, oldest first.
    fn log(query: &LogQuery) -> Vec<LogEntry> = log(query), log(query);
    /// Records by id, each followed by the attachment it carries: those up
    /// to the last record of the part, if it stops short.
    fn records(request: &FetchRecords) -> Part = fetch_records(request), records(&request.ids);
    /// The trees and blobs of a snapshot that the request's snapshots lack,
    /// in parts.
    fn snapshot(request: &FetchSnapshot) -> Part = fetch_snapshot(request), snapshot(request);
    /// The revisions of a change, and its heads.
    fn revisions(change: ChangeId) -> Revisions = revisions(change), revisions(change);
    /// What the remote holds of some changes.
    fn inventory(request: &InventoryRequest) -> Vec<Inventory> =
        inventory(request), inventory(request);
    /// Puts a batch of objects.
    fn put_records(pack: Pack) -> Stored = put_records(&pack), put_records(pack);
    /// Puts a batch of objects, if the changes the query names have the
    /// heads it says.
    fn push(pack: Pack, query: &PutQuery) -> Stored =
        push_records(&pack, query), push(pack, &query.expect.0);
    /// The statements about a record.
    fn statements(query: &StatementsQuery) -> Vec<Annotation> =
        statements(query), statements(query.subject);
    /// Promotes results to a record, as statements the remote issues.
    fn promote(request: &PromoteRequest) -> Vec<Annotation> =
        promote(request), promote(request.subject);
    /// Submits a landing to the remote's queue.
    fn land(request: &LandRequest) -> Landings = land(request), land(request);
    /// The landings that wait on the remote.
    fn landings() -> Vec<Pending> = landings(), landings();
    /// Creates a target the remote orders.
    fn create_target(request: &NewTarget) -> Head = create_target(request), create_target(request);
    /// The events after a cursor, waiting for some.
    fn events(query: &EventsQuery) -> Page = events(query), events(query);
    /// Who the remote thinks the user is, and what they may do there.
    fn whoami() -> Rights = whoami(), whoami();
    /// Issues a token acting for the user, with a narrower scope.
    fn issue_token(request: &TokenRequest) -> IssuedToken =
        issue_token(request), issue_token(request);
    /// Lists a directory of a snapshot.
    fn tree(query: &BrowseQuery) -> Listing = tree(query), tree(query);
    /// Reads a file of a snapshot.
    fn file(query: &BrowseQuery) -> Vec<u8> = file(query), file(query);
}

impl Remote {
    /// Opens the remote at `url` for the user of `config`: over HTTP with
    /// their token for its server, if they have one, or in-process by
    /// their node.
    ///
    /// # Errors
    ///
    /// Fails like [`Remote::open`], or if the node key cannot be loaded.
    pub fn open_for(url: &RemoteUrl, config: &UserConfig) -> Result<Self, RemoteError> {
        Self::open(url, Node::of(config)?, url.token(config))
    }

    /// Opens the remote at `url`: an `http://` URL whose last path segment
    /// names the repository, called with `token` if there's one, or the
    /// path of a repository, which `node` serves in-process, to its owner.
    ///
    /// # Errors
    ///
    /// Fails if the URL names no repository, or the path holds none.
    pub fn open(url: &RemoteUrl, node: Node, token: Option<String>) -> Result<Self, RemoteError> {
        match url {
            RemoteUrl::Http { base, repo } => {
                let client = Client::new(base);
                let client = match token {
                    Some(token) => client.with_token(token),
                    None => client,
                };
                Ok(Self::Http {
                    runtime: tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(RemoteError::Runtime)?,
                    repo: client.repo(repo.clone()),
                })
            }
            RemoteUrl::Path(path) => {
                // A working copy's repository is in its `.origin`.
                let meta = path.join(Workspace::META_DIR);
                let root = if meta.is_dir() { meta } else { path.clone() };
                let service = OriginService::open(root, node)
                    .map_err(|source| RemoteError::Open(Box::new(source)))?;
                Ok(Self::Local(Box::new(service)))
            }
        }
    }
}

/// Where a remote is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteUrl {
    /// A repository on a server.
    Http {
        /// The server, such as `http://127.0.0.1:7878`.
        base: String,
        /// The repository.
        repo: RepoName,
    },
    /// A repository on a local path, absolute.
    Path(PathBuf),
}

impl RemoteUrl {
    /// The token the user of `config` sends this remote, if any: none to a
    /// local path, which serves them as its owner.
    #[must_use]
    pub fn token(&self, config: &UserConfig) -> Option<String> {
        match self {
            Self::Http { base, .. } => config.token(base),
            Self::Path(_) => None,
        }
    }

    /// Parses a remote URL: `http://host[:port]/<repository>`, or a path,
    /// made absolute, so the URL works from anywhere.
    ///
    /// # Errors
    ///
    /// Fails if an HTTP URL names no repository, or is HTTPS.
    pub fn parse(url: &str) -> Result<Self, RemoteError> {
        if url.starts_with("https://") {
            return Err(RemoteError::HttpsUnsupported);
        }
        let Some(rest) = url.strip_prefix("http://") else {
            let path = path::absolute(Path::new(url))
                .map_err(|_| RemoteError::InvalidUrl(url.to_owned()))?;
            return Ok(Self::Path(path));
        };
        let invalid = || RemoteError::InvalidUrl(url.to_owned());
        let (host, name) = rest
            .trim_end_matches('/')
            .rsplit_once('/')
            .ok_or_else(invalid)?;
        Ok(Self::Http {
            base: format!("http://{host}"),
            repo: RepoName::new(name).map_err(|_| invalid())?,
        })
    }

    /// The name a clone's directory takes by default: the repository's, or
    /// the path's last component.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        match self {
            Self::Http { repo, .. } => Some(repo.to_string()),
            Self::Path(path) => Some(path.file_name()?.to_string_lossy().into_owned()),
        }
    }
}

impl fmt::Display for RemoteUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http { base, repo } => write!(f, "{base}/{repo}"),
            Self::Path(path) => write!(f, "{}", path.display()),
        }
    }
}

/// A failure to talk to a remote.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The remote URL is not usable.
    #[error("invalid remote `{0}`: expected http://host[:port]/<repository> or a path")]
    InvalidUrl(String),
    /// The client has no TLS support yet.
    #[error("HTTPS remotes are not supported yet")]
    HttpsUnsupported,
    /// The user's node key cannot be loaded.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The runtime for HTTP requests cannot start.
    #[error("cannot start the runtime for HTTP requests")]
    Runtime(#[source] std::io::Error),
    /// A local path holds no repository to serve.
    #[error("cannot open the remote repository")]
    Open(#[source] Box<ServiceError>),
    /// The server cannot be reached or refused.
    #[error(transparent)]
    Http(#[from] origin_client::Error),
    /// The repository on a local path refused.
    #[error(transparent)]
    Local(#[from] ServiceError),
}

impl RemoteError {
    /// Whether the remote cannot be reached for now, or failed on its side,
    /// as a server does while it restarts: asking again later may work.
    #[must_use]
    pub fn is_unreachable(&self) -> bool {
        match self {
            Self::Http(origin_client::Error::Transport(_)) => true,
            Self::Http(origin_client::Error::Status(status)) => *status >= 500,
            _ => false,
        }
    }

    /// What kind of refusal this is, if the remote refused.
    #[must_use]
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Http(origin_client::Error::Api(error)) => Some(error.code),
            Self::Local(error) => Some(error.code()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let url = RemoteUrl::parse("http://localhost:7878/demo/").unwrap();
        assert_eq!(
            url,
            RemoteUrl::Http {
                base: "http://localhost:7878".to_owned(),
                repo: RepoName::new("demo").unwrap(),
            }
        );
        assert_eq!(url.to_string(), "http://localhost:7878/demo");
        assert_eq!(url.name().as_deref(), Some("demo"));
        assert!(RemoteUrl::parse("http://localhost").is_err());
        assert!(RemoteUrl::parse("http://localhost/.hidden").is_err());
        assert!(RemoteUrl::parse("https://localhost/demo").is_err());
        let RemoteUrl::Path(path) = RemoteUrl::parse("server/demo").unwrap() else {
            panic!("expected a path");
        };
        assert!(path.is_absolute() && path.ends_with("server/demo"));
    }
}
