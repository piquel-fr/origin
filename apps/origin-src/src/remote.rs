//! Remotes: where a workspace pushes and pulls.
//!
//! A remote is either a server over HTTP (`http://host:port/<repository>`)
//! or a repository on a local path. Local repositories are served by an
//! in-process [`OriginService`], so both behave the same, except that
//! whoever can open a repository's files owns it: permissions only apply
//! over HTTP, to the identity of the token the user configured.

use std::collections::BTreeMap;
use std::path::PathBuf;

use origin_client::Client;
use origin_shared::access::{Identity, Permissions};
use origin_shared::api::{ApiError, CommitsRequest, ErrorCode, ObjectsRequest, RepoInfo, RepoName};
use origin_shared::pack::{Pack, PushRequest};
use serde::{Deserialize, Serialize};

use crate::service::{Caller, OriginService, ServiceError};
use crate::user::UserConfig;

/// A repository to push to and pull from.
pub enum Remote {
    /// A repository on a server.
    Http(origin_client::Repo),
    /// A repository on a local path, served in-process.
    Local {
        /// The service, serving only `repo`.
        service: OriginService,
        /// The repository's name in `service`.
        repo: RepoName,
    },
}

impl Remote {
    /// Opens `url`: an `http://` URL whose last path segment is the
    /// repository, or the path of a repository. HTTP requests carry the
    /// user's token for the server, if they configured one.
    ///
    /// # Errors
    ///
    /// Fails if the URL has no repository name, or the user configuration
    /// is invalid.
    pub fn open(url: &str) -> Result<Self, RemoteError> {
        if url.starts_with("https://") {
            return Err(RemoteError::HttpsUnsupported);
        }
        if let Some(rest) = url.strip_prefix("http://") {
            let invalid = || RemoteError::InvalidUrl(url.to_owned());
            let (_, name) = rest
                .trim_end_matches('/')
                .rsplit_once('/')
                .ok_or_else(invalid)?;
            let repo = RepoName::new(name).map_err(|_| invalid())?;
            let base = &url[..url.trim_end_matches('/').len() - name.len() - 1];
            let mut client = Client::new(base);
            if let Some(token) = UserConfig::load()?.token(base) {
                client = client.with_token(token);
            }
            return Ok(Self::Http(client.repo(repo)));
        }
        let repo = RepoName::new("local").map_err(|_| RemoteError::InvalidUrl(url.to_owned()))?;
        let service = OriginService::new(BTreeMap::from([(repo.clone(), PathBuf::from(url))]));
        Ok(Self::Local { service, repo })
    }

    /// Describes the repository and its branches.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached or has no such repository.
    pub async fn info(&self) -> Result<RepoInfo, RemoteError> {
        match self {
            Self::Http(repo) => Ok(repo.info().await?),
            Self::Local { service, repo } => Ok(service.info(&Caller::Owner, repo)?),
        }
    }

    /// Fetches commits and their ancestors.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached or a commit does not exist.
    pub async fn commits(&self, request: &CommitsRequest) -> Result<Pack, RemoteError> {
        match self {
            Self::Http(repo) => Ok(repo.fetch_commits(request).await?),
            Self::Local { service, repo } => Ok(service.commits(&Caller::Owner, repo, request)?),
        }
    }

    /// Fetches the objects at some paths of a commit.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached or a path does not exist.
    pub async fn objects(&self, request: &ObjectsRequest) -> Result<Pack, RemoteError> {
        match self {
            Self::Http(repo) => Ok(repo.fetch_objects(request).await?),
            Self::Local { service, repo } => Ok(service.objects(&Caller::Owner, repo, request)?),
        }
    }

    /// Pushes, as one transaction.
    ///
    /// # Errors
    ///
    /// Fails with [`RemoteError::Conflict`] if a branch moved since it was
    /// last fetched, or if the remote rejects the push.
    pub async fn push(&self, push: PushRequest) -> Result<(), RemoteError> {
        match self {
            Self::Http(repo) => Ok(repo.push(&push).await?),
            Self::Local { service, repo } => Ok(service.push(&Caller::Owner, repo, push)?),
        }
    }
}

/// What the user may do on a remote: who it thinks they are, and its rules.
/// `None` stands for full access, like an owner's.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AccessInfo {
    /// The user, or `None` when anonymous.
    pub identity: Option<Identity>,
    /// The repository's rules.
    pub permissions: Permissions,
}

impl Remote {
    /// What the user may do, or `None` if they own the repository.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached.
    pub async fn access(&self) -> Result<Option<AccessInfo>, RemoteError> {
        let Self::Http(repo) = self else {
            return Ok(None);
        };
        Ok(Some(AccessInfo {
            identity: repo.client().whoami().await?.identity,
            permissions: repo.permissions().await?,
        }))
    }

    /// Replaces the repository's rules.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached or the user may not change
    /// them.
    pub async fn set_permissions(&self, permissions: &Permissions) -> Result<(), RemoteError> {
        match self {
            Self::Http(repo) => Ok(repo.set_permissions(permissions).await?),
            Self::Local { service, repo } => {
                Ok(service.set_permissions(&Caller::Owner, repo, permissions.clone())?)
            }
        }
    }

    /// The repository's rules.
    ///
    /// # Errors
    ///
    /// Fails if the remote cannot be reached.
    pub async fn permissions(&self) -> Result<Permissions, RemoteError> {
        match self {
            Self::Http(repo) => Ok(repo.permissions().await?),
            Self::Local { service, repo } => Ok(service.permissions(&Caller::Owner, repo)?),
        }
    }
}

/// A failure to talk to a remote.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The remote URL is not usable.
    #[error("invalid remote `{0}`: expected http://host[:port]/<repository> or a path")]
    InvalidUrl(String),
    /// The user configuration is invalid.
    #[error(transparent)]
    UserConfig(#[from] crate::user::UserConfigError),
    /// The client has no TLS support yet.
    #[error("HTTPS remotes are not supported yet")]
    HttpsUnsupported,
    /// A branch moved on the remote since it was last fetched.
    #[error("{0}")]
    Conflict(String),
    /// The remote rejected a request.
    #[error("the remote refused: {0}")]
    Refused(ApiError),
    /// The remote could not be reached, or answered unexpectedly.
    #[error(transparent)]
    Http(origin_client::Error),
    /// A local remote refused.
    #[error(transparent)]
    Local(ServiceError),
}

impl From<origin_client::Error> for RemoteError {
    fn from(error: origin_client::Error) -> Self {
        match error {
            origin_client::Error::Api(error) if error.code == ErrorCode::Conflict => {
                Self::Conflict(error.message)
            }
            origin_client::Error::Api(error) => Self::Refused(error),
            error => Self::Http(error),
        }
    }
}

impl From<ServiceError> for RemoteError {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::Conflict(_) => Self::Conflict(error.to_string()),
            error => Self::Local(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let Remote::Http(repo) = Remote::open("http://localhost:7878/demo").unwrap() else {
            panic!("expected an HTTP remote");
        };
        assert_eq!(repo.name().as_str(), "demo");
        assert!(Remote::open("http://localhost:7878/prefix/demo/").is_ok());
        assert!(Remote::open("http://localhost").is_err());
        assert!(matches!(
            Remote::open("../server/demo").unwrap(),
            Remote::Local { .. }
        ));
    }
}
