//! The HTTP API: a thin layer translating requests into [`OriginService`]
//! calls. Every route is documented with `utoipa`, which generates the
//! OpenAPI document; `origin-client` is the Rust SDK for it.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path as UrlPath, Query, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use origin_shared::access::{Permissions, Whoami};
use origin_shared::api::{
    ApiError, CommitsRequest, ErrorCode, ListedEntry, Location, ObjectsRequest, RepoInfo, RepoName,
    RepoSummary,
};
use origin_shared::pack::PushRequest;
use serde::Deserialize;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::{Authenticator, Users};
use crate::service::{Caller, OriginService, PushError, ServiceError};

/// Server configuration, read from a TOML file:
///
/// ```toml
/// listen = "127.0.0.1:7878"
///
/// [repos]
/// origin = "/srv/origin/origin" # name = path, relative to this file
///
/// [users.alice] # see `Users`
/// token = "a long random secret"
/// ```
#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    /// The address to listen on. Port 0 picks a free port.
    pub listen: SocketAddr,
    /// The repositories to serve, by name.
    #[serde(default)]
    pub repos: BTreeMap<RepoName, PathBuf>,
    /// Who can authenticate. Everyone else is anonymous.
    #[serde(default)]
    pub users: Users,
}

impl ServerConfig {
    /// Reads the configuration, resolving repository paths relative to it.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is invalid.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut config: Self = toml::from_str(&content)
            .with_context(|| format!("invalid configuration in {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        for repo in config.repos.values_mut() {
            *repo = base.join(&*repo);
        }
        Ok(config)
    }

    /// `origin serve`: serves the API until interrupted, logging requests to
    /// stderr. Prints the address it listens on, which tells callers that
    /// chose port 0 where to connect.
    ///
    /// # Errors
    ///
    /// Fails if the address cannot be bound.
    pub async fn serve(self) -> anyhow::Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_writer(std::io::stderr)
            .init();
        let listener = tokio::net::TcpListener::bind(self.listen)
            .await
            .with_context(|| format!("cannot listen on {}", self.listen))?;
        println!("Listening on http://{}", listener.local_addr()?);
        let router = Api::router(OriginService::new(self.repos), Arc::new(self.users));
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                // If the signal handler cannot be installed, run until killed.
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .context("server failed")
    }
}

#[derive(OpenApi)]
#[openapi(info(
    title = "origin API",
    version = "v1",
    description = "Repository management and object transfer for origin servers.",
    license(name = "Apache-2.0")
))]
struct ApiDoc;

/// What every request can reach.
struct AppState {
    service: OriginService,
    authenticator: Arc<dyn Authenticator>,
}

type Service = State<Arc<AppState>>;

/// The HTTP API.
pub struct Api;

impl Api {
    fn documented() -> OpenApiRouter<Arc<AppState>> {
        OpenApiRouter::with_openapi(ApiDoc::openapi())
            .routes(routes!(whoami))
            .routes(routes!(list_repos))
            .routes(routes!(permissions, set_permissions))
            .routes(routes!(repo_info))
            .routes(routes!(list_tree))
            .routes(routes!(read_file))
            .routes(routes!(fetch_commits))
            .routes(routes!(fetch_objects))
            .routes(routes!(push))
            .routes(routes!(openapi_document))
    }

    /// The largest request accepted, which bounds pushes.
    const MAX_REQUEST: usize = 1 << 30;

    /// The router serving `service`, authenticating bearer tokens with
    /// `authenticator`.
    pub fn router(service: OriginService, authenticator: Arc<dyn Authenticator>) -> axum::Router {
        let state = AppState {
            service,
            authenticator,
        };
        axum::Router::from(Self::documented().with_state(Arc::new(state)))
            .layer(DefaultBodyLimit::max(Self::MAX_REQUEST))
    }

    /// The OpenAPI document, which `docs/openapi.json` keeps a copy of.
    #[must_use]
    pub fn openapi() -> utoipa::openapi::OpenApi {
        Self::documented().into_openapi()
    }
}

/// Callers are anonymous without an `Authorization: Bearer <token>`
/// header, and rejected with an invalid one.
impl FromRequestParts<Arc<AppState>> for Caller {
    type Rejection = HttpError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> impl Future<Output = Result<Self, HttpError>> + Send {
        let caller = match parts.headers.get(header::AUTHORIZATION) {
            None => Ok(Self::Client(None)),
            Some(header) => header
                .to_str()
                .ok()
                .and_then(|header| header.strip_prefix("Bearer "))
                .and_then(|token| state.authenticator.authenticate(token))
                .map(|identity| Self::Client(Some(identity)))
                .ok_or(HttpError::Unauthorized),
        };
        std::future::ready(caller)
    }
}

/// Runs a service call on the blocking thread pool: service calls do
/// synchronous filesystem work.
async fn call<T: Send + 'static>(
    State(state): Service,
    caller: Caller,
    call: impl FnOnce(&OriginService, &Caller) -> Result<T, ServiceError> + Send + 'static,
) -> Result<T, HttpError> {
    tokio::task::spawn_blocking(move || call(&state.service, &caller))
        .await
        .map_err(|error| HttpError::Internal(error.to_string()))?
        .map_err(HttpError::Service)
}

/// A failed request, rendered as an [`ApiError`].
enum HttpError {
    Service(ServiceError),
    Unauthorized,
    Internal(String),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Service(error) => {
                let (status, code) = match &error {
                    ServiceError::RepoNotFound(_)
                    | ServiceError::BranchNotFound(_)
                    | ServiceError::PathNotFound(..)
                    | ServiceError::ObjectNotFound(_)
                    | ServiceError::NotInCommit { .. } => {
                        (StatusCode::NOT_FOUND, ErrorCode::NotFound)
                    }
                    ServiceError::NotADirectory(_)
                    | ServiceError::NotAFile(_)
                    | ServiceError::InvalidPush(_) => {
                        (StatusCode::BAD_REQUEST, ErrorCode::BadRequest)
                    }
                    ServiceError::Conflict(_) => (StatusCode::CONFLICT, ErrorCode::Conflict),
                    ServiceError::Forbidden(_) => (StatusCode::FORBIDDEN, ErrorCode::Forbidden),
                    ServiceError::Repository(_)
                    | ServiceError::Store(_)
                    | ServiceError::Refs(_) => {
                        tracing::error!(error = %anyhow::Error::from(error), "request failed");
                        return Self::Internal("internal server error".into()).into_response();
                    }
                };
                (status, code, error.to_string())
            }
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                ErrorCode::Unauthorized,
                "invalid credentials".into(),
            ),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                message,
            ),
        };
        (status, Json(ApiError { code, message })).into_response()
    }
}

/// Binary content, such as a file or a pack, in the OpenAPI document:
/// utoipa would describe `Vec<u8>` as an array of numbers.
struct Binary;

impl utoipa::PartialSchema for Binary {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::Type::String)
            .content_media_type("application/octet-stream")
            .into()
    }
}

impl utoipa::ToSchema for Binary {}

fn pack_response(pack: &origin_shared::pack::Pack) -> Response {
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        pack.encode(),
    )
        .into_response()
}

/// Lists the repositories on the server.
#[utoipa::path(
    get,
    path = "/api/v1/repos",
    operation_id = "list_repos",
    responses((status = 200, description = "The repositories", body = Vec<RepoSummary>), (status = 500, description = "The server failed", body = ApiError))
)]
async fn list_repos(service: Service, caller: Caller) -> Result<Json<Vec<RepoSummary>>, HttpError> {
    call(service, caller, OriginService::list).await.map(Json)
}

/// Describes a repository and its branches.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}",
    operation_id = "repo_info",
    params(("repo" = RepoName, Path, description = "The repository")),
    responses((status = 200, description = "The repository", body = RepoInfo), (status = 404, description = "Something does not exist", body = ApiError))
)]
async fn repo_info(
    service: Service,
    caller: Caller,
    UrlPath(repo): UrlPath<RepoName>,
) -> Result<Json<RepoInfo>, HttpError> {
    call(service, caller, move |service, caller| {
        service.info(caller, &repo)
    })
    .await
    .map(Json)
}

/// Lists a directory on a branch.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/tree",
    operation_id = "list_tree",
    params(("repo" = RepoName, Path, description = "The repository"), Location),
    responses(
        (status = 200, description = "The directory entries, sorted by name", body = Vec<ListedEntry>),
        (status = 400, description = "The request is invalid", body = ApiError),
        (status = 404, description = "Something does not exist", body = ApiError)
    )
)]
async fn list_tree(
    service: Service,
    caller: Caller,
    UrlPath(repo): UrlPath<RepoName>,
    Query(location): Query<Location>,
) -> Result<Json<Vec<ListedEntry>>, HttpError> {
    call(service, caller, move |service, caller| {
        service.tree(caller, &repo, &location)
    })
    .await
    .map(Json)
}

/// Reads a file on a branch.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/file",
    operation_id = "read_file",
    params(("repo" = RepoName, Path, description = "The repository"), Location),
    responses(
        (status = 200, description = "The file content", content_type = "application/octet-stream", body = Binary),
        (status = 400, description = "The request is invalid", body = ApiError),
        (status = 404, description = "Something does not exist", body = ApiError)
    )
)]
async fn read_file(
    service: Service,
    caller: Caller,
    UrlPath(repo): UrlPath<RepoName>,
    Query(location): Query<Location>,
) -> Result<Response, HttpError> {
    let content = call(service, caller, move |service, caller| {
        service.file(caller, &repo, &location)
    })
    .await?;
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        content,
    )
        .into_response())
}

/// Fetches commits and their ancestors, as a pack.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/commits",
    operation_id = "fetch_commits",
    params(("repo" = RepoName, Path, description = "The repository")),
    request_body = CommitsRequest,
    responses(
        (status = 200, description = "The objects, as a pack", content_type = "application/octet-stream", body = Binary),
        (status = 404, description = "Something does not exist", body = ApiError)
    )
)]
async fn fetch_commits(
    service: Service,
    caller: Caller,
    UrlPath(repo): UrlPath<RepoName>,
    Json(request): Json<CommitsRequest>,
) -> Result<Response, HttpError> {
    let pack = call(service, caller, move |service, caller| {
        service.commits(caller, &repo, &request)
    })
    .await?;
    Ok(pack_response(&pack))
}

/// Fetches the objects at some paths of a commit, as a pack.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/objects",
    operation_id = "fetch_objects",
    params(("repo" = RepoName, Path, description = "The repository")),
    request_body = ObjectsRequest,
    responses(
        (status = 200, description = "The objects, as a pack", content_type = "application/octet-stream", body = Binary),
        (status = 404, description = "Something does not exist", body = ApiError)
    )
)]
async fn fetch_objects(
    service: Service,
    caller: Caller,
    UrlPath(repo): UrlPath<RepoName>,
    Json(request): Json<ObjectsRequest>,
) -> Result<Response, HttpError> {
    let pack = call(service, caller, move |service, caller| {
        service.objects(caller, &repo, &request)
    })
    .await?;
    Ok(pack_response(&pack))
}

/// Moves branches, with the objects their new commits add, as one
/// transaction. The body is a push (see `origin_shared::pack`).
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/push",
    operation_id = "push",
    params(("repo" = RepoName, Path, description = "The repository")),
    request_body(content = Binary, content_type = "application/octet-stream"),
    responses(
        (status = 204, description = "Every branch moved"),
        (status = 400, description = "The push is incomplete or malformed", body = ApiError),
        (status = 404, description = "Something does not exist", body = ApiError),
        (status = 403, description = "The caller may not write a changed path", body = ApiError),
        (status = 409, description = "A branch moved since the pusher looked; nothing changed", body = ApiError)
    )
)]
async fn push(
    service: Service,
    caller: Caller,
    UrlPath(repo): UrlPath<RepoName>,
    body: Bytes,
) -> Result<StatusCode, HttpError> {
    let push = PushRequest::decode(&body)
        .map_err(|error| HttpError::Service(PushError::from(error).into()))?;
    call(service, caller, move |service, caller| {
        service.push(caller, &repo, push)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Who the server thinks the caller is.
#[utoipa::path(
    get,
    path = "/api/v1/whoami",
    operation_id = "whoami",
    responses(
        (status = 200, description = "The caller", body = Whoami),
        (status = 401, description = "The credentials are invalid", body = ApiError)
    )
)]
async fn whoami(caller: Caller) -> Json<Whoami> {
    let identity = match caller {
        Caller::Client(identity) => identity,
        Caller::Owner => None,
    };
    Json(Whoami { identity })
}

/// The repository's permissions.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/permissions",
    operation_id = "permissions",
    params(("repo" = RepoName, Path, description = "The repository")),
    responses(
        (status = 200, description = "The rules, by path", body = Permissions),
        (status = 404, description = "Something does not exist", body = ApiError)
    )
)]
async fn permissions(
    service: Service,
    caller: Caller,
    UrlPath(repo): UrlPath<RepoName>,
) -> Result<Json<Permissions>, HttpError> {
    call(service, caller, move |service, caller| {
        service.permissions(caller, &repo)
    })
    .await
    .map(Json)
}

/// Replaces the repository's permissions. Takes write access to the root.
#[utoipa::path(
    put,
    path = "/api/v1/repos/{repo}/permissions",
    operation_id = "set_permissions",
    params(("repo" = RepoName, Path, description = "The repository")),
    request_body = Permissions,
    responses(
        (status = 204, description = "The permissions were replaced"),
        (status = 403, description = "The caller may not write the root", body = ApiError),
        (status = 404, description = "Something does not exist", body = ApiError)
    )
)]
async fn set_permissions(
    service: Service,
    caller: Caller,
    UrlPath(repo): UrlPath<RepoName>,
    Json(permissions): Json<Permissions>,
) -> Result<StatusCode, HttpError> {
    call(service, caller, move |service, caller| {
        service.set_permissions(caller, &repo, permissions)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The OpenAPI document of this API.
#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    operation_id = "openapi_document",
    responses((status = 200, description = "The OpenAPI document", body = Object))
)]
async fn openapi_document() -> Json<utoipa::openapi::OpenApi> {
    Json(Api::openapi())
}
