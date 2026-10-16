//! The HTTP API: a thin layer translating requests into [`OriginService`]
//! calls on the repository they name. Every route is documented with
//! `utoipa`, which generates the OpenAPI document; `origin-client` is the
//! Rust SDK for it.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use origin_shared::ChangeId;
use origin_shared::Head;
use origin_shared::api::{
    Annotation, ApiError, ErrorCode, EventsQuery, FetchRecords, FetchSnapshot, Inventory,
    InventoryRequest, LandRequest, Landings, LogEntry, LogQuery, NewTarget, RepoName, RepoSummary,
    Revisions, StatementsQuery, Stored, TargetInfo,
};
use origin_shared::event::Page;
use origin_shared::landing::Pending;
use origin_shared::pack::{Pack, PackError};
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
    /// The largest request accepted, which bounds a batch of records put.
    const MAX_REQUEST: usize = 1 << 30;

    fn documented() -> OpenApiRouter<Arc<Repositories>> {
        OpenApiRouter::with_openapi(ApiDoc::openapi())
            .routes(routes!(list_repos))
            .routes(routes!(fetch_records))
            .routes(routes!(fetch_snapshot))
            .routes(routes!(put_records))
            .routes(routes!(inventory))
            .routes(routes!(statements))
            .routes(routes!(revisions))
            .routes(routes!(targets, create_target))
            .routes(routes!(log))
            .routes(routes!(land))
            .routes(routes!(landings))
            .routes(routes!(events))
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

/// Every request is anonymous: nothing authenticates callers yet.
impl FromRequestParts<Arc<Repositories>> for Caller {
    type Rejection = Infallible;

    fn from_request_parts(
        _parts: &mut Parts,
        _repositories: &Arc<Repositories>,
    ) -> impl Future<Output = Result<Self, Infallible>> + Send {
        std::future::ready(Ok(Self::Anonymous))
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
enum HttpError {
    Service(ServiceError),
    Pack(PackError),
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

/// Lists the repositories on the server.
#[utoipa::path(
    get,
    path = "/api/v1/repos",
    operation_id = "list_repos",
    responses((status = 200, description = "The repositories, by name", body = Vec<RepoSummary>))
)]
async fn list_repos(State(repositories): Repos, caller: Caller) -> Json<Vec<RepoSummary>> {
    Json(repositories.list(&caller))
}

/// A pack, as a response.
struct PackResponse(Pack);

impl IntoResponse for PackResponse {
    fn into_response(self) -> Response {
        (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            self.0.encode(),
        )
            .into_response()
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
        (status = 200, description = "The records, in the order asked, each followed by its description or payload if the repository has it, as a pack: each object a 32-byte id, a little-endian u64 length and the object's encoding", content_type = "application/octet-stream", body = Binary),
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
        (status = 200, description = "The trees and blobs, parents before children, as a pack", content_type = "application/octet-stream", body = Binary),
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
/// statements, each needing only objects the batch or the repository has.
#[utoipa::path(
    put,
    path = "/api/v1/repos/{repo}/records",
    operation_id = "put_records",
    params(("repo" = String, Path, description = "The repository")),
    request_body(content = Binary, content_type = "application/octet-stream", description = "The objects, as a pack"),
    responses(
        (status = 200, description = "The objects are durable", body = Stored),
        (status = 400, description = "The pack is invalid, holds an acceptance, or needs objects the repository doesn't have", body = ApiError),
        (status = 404, description = "The repository does not exist", body = ApiError)
    )
)]
async fn put_records(
    repos: Repos,
    Path(repo): Path<RepoName>,
    caller: Caller,
    body: Bytes,
) -> Result<Json<Stored>, HttpError> {
    let pack = Pack::decode(&body).map_err(HttpError::Pack)?;
    call(repos, repo, caller, move |service, caller| {
        service.put_records(caller, pack)
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
