//! The HTTP API: a thin layer translating requests into [`OriginService`]
//! calls on the repository they name. Every route is documented with
//! `utoipa`, which generates the OpenAPI document; `origin-client` is the
//! Rust SDK for it.
//!
//! Callers authenticate with `Authorization: Bearer <token>` (see
//! [`Credentials`](crate::auth::Credentials)); without it, they are anonymous.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use origin_shared::ChangeId;
use origin_shared::Head;
use origin_shared::access::{InvalidToken, Rights};
use origin_shared::api::{
    Annotation, ApiError, BrowseQuery, ErrorCode, EventsQuery, FetchRecords, FetchSnapshot,
    Inventory, InventoryRequest, IssuedToken, LandRequest, Landings, Listing, LogEntry, LogQuery,
    MORE, NewTarget, PromoteRequest, PutQuery, RepoName, RepoSummary, Revisions, StatementsQuery,
    Stored, TargetInfo, TokenRequest,
};
use origin_shared::event::Page;
use origin_shared::landing::Pending;
use origin_shared::pack::{Pack, PackError, Part};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::server::Repositories;
use crate::service::{Caller, OriginService, ServiceError};

#[derive(OpenApi)]
#[openapi(info(
    title = "origin API",
    version = "v1",
    description = "The repositories an origin server hosts: their records, targets, landing \
                   queues and events.",
    license(name = "Apache-2.0")
))]
struct ApiDoc;

type Repos = State<Arc<Repositories>>;

/// The HTTP API.
pub struct Api;

impl Api {
    /// The largest request accepted, which bounds an object put: clients
    /// split larger pushes into requests of at most
    /// [`Pack::LIMIT`](origin_shared::pack::Pack::LIMIT), so only a single
    /// object larger than that takes a request of its own this large.
    const MAX_REQUEST: usize = 1 << 30;

    fn documented() -> OpenApiRouter<Arc<Repositories>> {
        OpenApiRouter::with_openapi(ApiDoc::openapi())
            .routes(routes!(list_repos))
            .routes(routes!(fetch_records))
            .routes(routes!(fetch_snapshot))
            .routes(routes!(put_records))
            .routes(routes!(inventory))
            .routes(routes!(statements))
            .routes(routes!(promote))
            .routes(routes!(revisions))
            .routes(routes!(targets, create_target))
            .routes(routes!(log))
            .routes(routes!(land))
            .routes(routes!(landings))
            .routes(routes!(events))
            .routes(routes!(whoami))
            .routes(routes!(issue_token))
            .routes(routes!(tree))
            .routes(routes!(file))
            .routes(routes!(openapi_document))
    }

    /// The router serving `repositories`.
    pub fn router(repositories: Repositories) -> axum::Router {
        axum::Router::from(Self::documented().with_state(Arc::new(repositories)))
            .layer(DefaultBodyLimit::max(Self::MAX_REQUEST))
    }

    /// The OpenAPI document, which `docs/openapi.json` keeps a copy of.
    #[must_use]
    pub fn openapi() -> utoipa::openapi::OpenApi {
        Self::documented().into_openapi()
    }
}

/// The caller of a request, by its bearer token.
impl FromRequestParts<Arc<Repositories>> for Caller {
    type Rejection = HttpError;

    fn from_request_parts(
        parts: &mut Parts,
        repositories: &Arc<Repositories>,
    ) -> impl Future<Output = Result<Self, HttpError>> + Send {
        let authorization = parts
            .headers
            .get(header::AUTHORIZATION)
            .map(|value| value.to_str().map_err(|_| InvalidToken::Malformed))
            .transpose();
        let caller = authorization
            .and_then(|authorization| repositories.credentials().caller(authorization))
            .map_err(HttpError::Unauthorized);
        std::future::ready(caller)
    }
}

/// Runs a call on the service of repository `repo`, on the blocking thread
/// pool: service calls do synchronous file system work, and event
/// long-polls wait.
async fn call<T: Send + 'static>(
    State(repositories): Repos,
    repo: RepoName,
    caller: Caller,
    call: impl FnOnce(&OriginService, &Caller) -> Result<T, ServiceError> + Send + 'static,
) -> Result<T, HttpError> {
    tokio::task::spawn_blocking(move || call(repositories.get(&caller, &repo)?, &caller))
        .await
        .map_err(|error| HttpError::Panicked(error.to_string()))?
        .map_err(HttpError::Service)
}

/// A failed request, rendered as an [`ApiError`].
pub enum HttpError {
    /// The service refused or failed.
    Service(ServiceError),
    /// The request's pack is invalid.
    Pack(PackError),
    /// The caller's credentials are invalid.
    Unauthorized(InvalidToken),
    /// The service panicked.
    Panicked(String),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (code, message) = match self {
            Self::Service(error) if error.code() == ErrorCode::Internal => {
                tracing::error!(error = %anyhow::Error::from(error), "request failed");
                (ErrorCode::Internal, "internal server error".to_owned())
            }
            Self::Service(error) => (error.code(), error.to_string()),
            Self::Pack(error) => (ErrorCode::BadRequest, format!("invalid pack: {error}")),
            Self::Unauthorized(error) => (ErrorCode::Unauthorized, error.to_string()),
            Self::Panicked(message) => {
                tracing::error!(%message, "request panicked");
                (ErrorCode::Internal, "internal server error".to_owned())
            }
        };
        let status = match code {
            ErrorCode::BadRequest => StatusCode::BAD_REQUEST,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Conflict => StatusCode::CONFLICT,
            ErrorCode::Expired => StatusCode::GONE,
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(ApiError { code, message })).into_response()
    }
}

/// Binary content, a pack, in the OpenAPI document: utoipa would describe
/// `Vec<u8>` as an array of numbers.
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

/// Lists the repositories on the server that the caller sees.
#[utoipa::path(
    get,
    path = "/api/v1/repos",
    operation_id = "list_repos",
    responses(
        (status = 200, description = "The repositories, by name", body = Vec<RepoSummary>),
        (status = 401, description = "The token is invalid or expired", body = ApiError)
    )
)]
async fn list_repos(
    State(repositories): Repos,
    caller: Caller,
) -> Result<Json<Vec<RepoSummary>>, HttpError> {
    tokio::task::spawn_blocking(move || repositories.list(&caller))
        .await
        .map_err(|error| HttpError::Panicked(error.to_string()))?
        .map(Json)
        .map_err(HttpError::Service)
}

/// A part of a fetch, as a response: its pack, with the [`MORE`] header if
/// it stopped short.
struct PackResponse(Part);

impl IntoResponse for PackResponse {
    fn into_response(self) -> Response {
        let Part { pack, more } = self.0;
        let mut response = (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            pack.encode(),
        )
            .into_response();
        if more {
            response
                .headers_mut()
                .insert(MORE, header::HeaderValue::from_static("true"));
        }
        response
    }
}

/// Fetches records by id, each with the blob it carries, as a pack.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/records/fetch",
    operation_id = "fetch_records",
    params(("repo" = String, Path, description = "The repository")),
    request_body = FetchRecords,
    responses(
        (status = 200, description = "The records, in the order asked, each followed by its description or payload if the repository has it, as a pack: each object a 32-byte id, a little-endian u64 length and the object's encoding. At most 64 MiB, but for a record alone", content_type = "application/octet-stream", body = Binary, headers(("origin-more" = bool, description = "Present if the pack stopped short: ask again for the records after the last it holds"))),
        (status = 400, description = "An object is a blob or a tree, which only leave by walking a snapshot", body = ApiError),
        (status = 404, description = "The repository or a record does not exist", body = ApiError)
    )
)]
async fn fetch_records(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Json(request): Json<FetchRecords>,
) -> Result<PackResponse, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.records(caller, &request.ids)
    })
    .await
    .map(PackResponse)
}

/// Fetches the objects of a snapshot the caller lacks, as a pack: the only
/// way trees and blobs leave a repository.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/snapshots/fetch",
    operation_id = "fetch_snapshot",
    params(("repo" = String, Path, description = "The repository")),
    request_body = FetchSnapshot,
    responses(
        (status = 200, description = "The trees and blobs, parents before children, as a pack, of at most 64 MiB, but for an object alone", content_type = "application/octet-stream", body = Binary, headers(("origin-more" = bool, description = "Present if the pack stopped short: ask again, skipping the objects it holds"))),
        (status = 404, description = "The repository or the snapshot does not exist", body = ApiError)
    )
)]
async fn fetch_snapshot(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Json(request): Json<FetchSnapshot>,
) -> Result<PackResponse, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.snapshot(caller, &request)
    })
    .await
    .map(PackResponse)
}

/// Puts a batch of objects, as a pack: snapshots, revisions and
/// statements, each needing only objects the batch or the repository has,
/// of the kinds it names. With `expect`, only if the changes it names have
/// the heads it says when the records are published.
#[utoipa::path(
    put,
    path = "/api/v1/repos/{repo}/records",
    operation_id = "put_records",
    params(("repo" = String, Path, description = "The repository"), PutQuery),
    request_body(content = Binary, content_type = "application/octet-stream", description = "The objects, as a pack"),
    responses(
        (status = 200, description = "The objects are durable", body = Stored),
        (status = 400, description = "The pack is invalid, holds an acceptance, or needs objects the repository doesn't have, or has as another kind", body = ApiError),
        (status = 404, description = "The repository does not exist", body = ApiError),
        (status = 409, description = "A change of `expect` has other heads now", body = ApiError)
    )
)]
async fn put_records(
    repos: Repos,
    Path(repo): Path<RepoName>,
    Query(query): Query<PutQuery>,
    caller: Caller,
    body: Bytes,
) -> Result<Json<Stored>, HttpError> {
    let pack = Pack::decode(&body).map_err(HttpError::Pack)?;
    call(repos, repo, caller, move |service, caller| {
        service.push(caller, pack, &query.expect.0)
    })
    .await
    .map(Json)
}

/// Lists what the repository holds of some changes: their revisions,
/// heads and statements, by id.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/changes/inventory",
    operation_id = "inventory",
    params(("repo" = String, Path, description = "The repository")),
    request_body = InventoryRequest,
    responses(
        (status = 200, description = "An inventory for each change, in the order asked: empty for a change the repository never heard of", body = Vec<Inventory>),
        (status = 404, description = "The repository does not exist", body = ApiError)
    )
)]
async fn inventory(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Json(request): Json<InventoryRequest>,
) -> Result<Json<Vec<Inventory>>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.inventory(caller, &request)
    })
    .await
    .map(Json)
}

/// Lists the statements about a record, in the order the repository
/// received them.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/statements",
    operation_id = "statements",
    params(("repo" = String, Path, description = "The repository"), StatementsQuery),
    responses(
        (status = 200, description = "The statements", body = Vec<Annotation>),
        (status = 404, description = "The repository does not exist", body = ApiError)
    )
)]
async fn statements(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Query(query): Query<StatementsQuery>,
) -> Result<Json<Vec<Annotation>>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.statements(caller, query.subject)
    })
    .await
    .map(Json)
}

/// Promotes results to a record from the records with the same input
/// digest, as statements this server issues, and retries the landings
/// waiting for them.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/promote",
    operation_id = "promote",
    params(("repo" = String, Path, description = "The repository")),
    request_body = PromoteRequest,
    responses(
        (status = 200, description = "The statements promoted: none if nothing matched", body = Vec<Annotation>),
        (status = 403, description = "The caller isn't signed in", body = ApiError),
        (status = 404, description = "The repository or the record does not exist", body = ApiError)
    )
)]
async fn promote(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Json(request): Json<PromoteRequest>,
) -> Result<Json<Vec<Annotation>>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.promote(caller, request.subject)
    })
    .await
    .map(Json)
}

/// Lists the revisions of a change, and its heads.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/changes/{change}/revisions",
    operation_id = "revisions",
    params(
        ("repo" = String, Path, description = "The repository"),
        ("change" = String, Path, description = "The change's id")
    ),
    responses(
        (status = 200, description = "The revisions, in the order the repository received them", body = Revisions),
        (status = 404, description = "The repository or the change does not exist", body = ApiError)
    )
)]
async fn revisions(
    repos: Repos,
    Path((repo, change)): Path<(RepoName, ChangeId)>,
    caller: Caller,
) -> Result<Json<Revisions>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.revisions(caller, change)
    })
    .await
    .map(Json)
}

/// Lists the targets, with their tips, authorities and requirements.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/targets",
    operation_id = "targets",
    params(("repo" = String, Path, description = "The repository")),
    responses(
        (status = 200, description = "The targets, by name", body = Vec<TargetInfo>),
        (status = 404, description = "The repository does not exist", body = ApiError)
    )
)]
async fn targets(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
) -> Result<Json<Vec<TargetInfo>>, HttpError> {
    call(repos, repo, caller, OriginService::targets)
        .await
        .map(Json)
}

/// Creates a target at the tip of another, ordered by this server.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/targets",
    operation_id = "create_target",
    params(("repo" = String, Path, description = "The repository")),
    request_body = NewTarget,
    responses(
        (status = 200, description = "The new target's first acceptance", body = Head),
        (status = 404, description = "The repository or the target to start from does not exist", body = ApiError),
        (status = 409, description = "The name is taken, or this server wouldn't order the target", body = ApiError)
    )
)]
async fn create_target(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Json(request): Json<NewTarget>,
) -> Result<Json<Head>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.create_target(caller, &request)
    })
    .await
    .map(Json)
}

/// Lists the acceptances of a target, oldest first.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/log",
    operation_id = "log",
    params(("repo" = String, Path, description = "The repository"), LogQuery),
    responses(
        (status = 200, description = "The acceptances from `since` on", body = Vec<LogEntry>),
        (status = 404, description = "The repository or the target does not exist", body = ApiError)
    )
)]
async fn log(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Query(query): Query<LogQuery>,
) -> Result<Json<Vec<LogEntry>>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.log(caller, &query)
    })
    .await
    .map(Json)
}

/// Submits a change, or a target's tip, to a target's landing queue, run
/// by this server.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/land",
    operation_id = "land",
    params(("repo" = String, Path, description = "The repository")),
    request_body = LandRequest,
    responses(
        (status = 200, description = "How each landing went: landed, unchanged, or waiting for evidence or a conflict's resolution", body = Landings),
        (status = 400, description = "A target was proposed without a target to land on", body = ApiError),
        (status = 404, description = "The repository, the change or a target does not exist", body = ApiError),
        (status = 409, description = "The change diverged or was abandoned, or this server doesn't order the target", body = ApiError)
    )
)]
async fn land(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Json(request): Json<LandRequest>,
) -> Result<Json<Landings>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.land(caller, &request)
    })
    .await
    .map(Json)
}

/// Lists the landings that wait, and why.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/landings",
    operation_id = "landings",
    params(("repo" = String, Path, description = "The repository")),
    responses(
        (status = 200, description = "The waiting landings, by target", body = Vec<Pending>),
        (status = 404, description = "The repository does not exist", body = ApiError)
    )
)]
async fn landings(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
) -> Result<Json<Vec<Pending>>, HttpError> {
    call(repos, repo, caller, OriginService::landings)
        .await
        .map(Json)
}

/// Reads the events after a cursor, waiting up to `wait` for some: a
/// long-poll. Pass the page's `next` as the next request's cursor.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/events",
    operation_id = "events",
    params(("repo" = String, Path, description = "The repository"), EventsQuery),
    responses(
        (status = 200, description = "The events, oldest first, and the next cursor: empty if none came in time", body = Page),
        (status = 404, description = "The repository does not exist", body = ApiError),
        (status = 410, description = "The server no longer keeps the events after the cursor", body = ApiError)
    )
)]
async fn events(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Page>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.events(caller, &query)
    })
    .await
    .map(Json)
}

/// Tells the caller who they are and what they may do in the repository.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/whoami",
    operation_id = "whoami",
    params(("repo" = String, Path, description = "The repository")),
    responses(
        (status = 200, description = "The caller's rights", body = Rights),
        (status = 401, description = "The token is invalid or expired", body = ApiError),
        (status = 404, description = "The repository does not exist", body = ApiError)
    )
)]
async fn whoami(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
) -> Result<Json<Rights>, HttpError> {
    call(repos, repo, caller, OriginService::whoami)
        .await
        .map(Json)
}

/// Issues a token acting for the caller, with a scope narrowing their
/// rights, signed by the server, which verifies it without storing it.
#[utoipa::path(
    post,
    path = "/api/v1/repos/{repo}/tokens",
    operation_id = "issue_token",
    params(("repo" = String, Path, description = "The repository whose policy the scope is checked against")),
    request_body = TokenRequest,
    responses(
        (status = 200, description = "The token", body = IssuedToken),
        (status = 403, description = "The caller isn't signed in, or the scope reaches beyond their rights", body = ApiError),
        (status = 404, description = "The repository does not exist", body = ApiError)
    )
)]
async fn issue_token(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Json(request): Json<TokenRequest>,
) -> Result<Json<IssuedToken>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.issue_token(caller, &request)
    })
    .await
    .map(Json)
}

/// Lists a directory of a snapshot: a target's tip, or a revision's or an
/// acceptance's. Withheld entries keep their place.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/tree",
    operation_id = "tree",
    params(("repo" = String, Path, description = "The repository"), BrowseQuery),
    responses(
        (status = 200, description = "The directory's entries", body = Listing),
        (status = 400, description = "The path is a file, or the record has no snapshot", body = ApiError),
        (status = 403, description = "The caller reads nothing under the path", body = ApiError),
        (status = 404, description = "The repository, the target, the record or the path does not exist", body = ApiError)
    )
)]
async fn tree(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Query(query): Query<BrowseQuery>,
) -> Result<Json<Listing>, HttpError> {
    call(repos, repo, caller, move |service, caller| {
        service.tree(caller, &query)
    })
    .await
    .map(Json)
}

/// Reads a file of a snapshot, as `tree` finds it.
#[utoipa::path(
    get,
    path = "/api/v1/repos/{repo}/file",
    operation_id = "file",
    params(("repo" = String, Path, description = "The repository"), BrowseQuery),
    responses(
        (status = 200, description = "The file's content", content_type = "application/octet-stream", body = Binary),
        (status = 400, description = "The path is a directory, or the record has no snapshot", body = ApiError),
        (status = 403, description = "The caller may not read the file", body = ApiError),
        (status = 404, description = "The repository, the target, the record or the path does not exist", body = ApiError)
    )
)]
async fn file(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    Query(query): Query<BrowseQuery>,
) -> Result<Response, HttpError> {
    let content = call(repos, repo, caller, move |service, caller| {
        service.file(caller, &query)
    })
    .await?;
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        content,
    )
        .into_response())
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
