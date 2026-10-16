//! The origin SDK: a typed client for origin servers.
//!
//! ```no_run
//! use origin_client::Client;
//! use origin_shared::api::RepoName;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::new("http://127.0.0.1:7878");
//! let repo = client.repo(RepoName::new("demo")?);
//! for target in repo.targets().await? {
//!     println!("{} is at #{}", target.name, target.head.seq);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Requests and responses are the `origin-shared` types the server uses, so
//! the two cannot disagree on a contract. The server also describes its API
//! in `docs/openapi.json`, for other languages.

use origin_shared::api::{
    Annotation, ApiError, EventsQuery, FetchRecords, FetchSnapshot, Inventory, InventoryRequest,
    LandRequest, Landings, LogEntry, LogQuery, NewTarget, RepoName, RepoSummary, Revisions,
    StatementsQuery, Stored, TargetInfo,
};
use origin_shared::event::Page;
use origin_shared::landing::Pending;
use origin_shared::pack::{Pack, PackError};
use origin_shared::{ChangeId, Head};
use reqwest::{Method, RequestBuilder};
use serde::de::DeserializeOwned;

/// Defines endpoints as methods. Each sends one request, built from an HTTP
/// method, a path relative to its owner's, which can name the arguments
/// like `format!` does, and optionally a body applied by a
/// [`RequestBuilder`] method (`query`, `json` or `body`), then reads the
/// response with a [`Response`] method.
macro_rules! endpoints {
    ($(
        impl $owner:ident {$(
            $(#[doc = $doc:literal])*
            fn $name:ident($($arg:ident: $type:ty),*) -> $output:ty =
                $method:ident $path:literal $(, $send:ident($body:expr))? => $read:ident;
        )*}
    )*) => {$(
        impl $owner {$(
            $(#[doc = $doc])*
            ///
            /// # Errors
            ///
            /// Fails if the server cannot be reached or refuses the request.
            pub async fn $name(&self, $($arg: $type),*) -> Result<$output, Error> {
                let request = self.request(Method::$method, &format!($path))$(.$send($body))?;
                Response::send(request).await?.$read().await
            }
        )*}
    )*};
}

endpoints! {
    impl Client {
        /// Lists the repositories on the server, by name.
        fn repos() -> Vec<RepoSummary> = GET "repos" => json;
        /// The server's OpenAPI document.
        fn openapi() -> serde_json::Value = GET "openapi.json" => json;
    }

    impl Repo {
        /// Fetches records by id, in the order asked, each followed by the
        /// blob it carries if the server has it: a revision's description,
        /// a statement's payload.
        fn fetch_records(request: &FetchRecords) -> Pack =
            POST "records/fetch", json(request) => pack;
        /// Fetches the trees and blobs of a snapshot that none of the
        /// request's snapshots holds at the same path, parents first.
        fn fetch_snapshot(request: &FetchSnapshot) -> Pack =
            POST "snapshots/fetch", json(request) => pack;
        /// Puts a batch of objects: snapshots, revisions and statements,
        /// each needing only objects the batch or the repository has.
        fn put_records(pack: &Pack) -> Stored = PUT "records", body(pack.encode()) => json;
        /// The statements about a record, in the order the repository
        /// received them.
        fn statements(query: &StatementsQuery) -> Vec<Annotation> =
            GET "statements", query(query) => json;
        /// What the server holds of some changes: their revisions, heads
        /// and statements, by id.
        fn inventory(request: &InventoryRequest) -> Vec<Inventory> =
            POST "changes/inventory", json(request) => json;
        /// The revisions of a change, and its heads.
        fn revisions(change: ChangeId) -> Revisions = GET "changes/{change}/revisions" => json;
        /// Every target, with its tip, authority and requirements.
        fn targets() -> Vec<TargetInfo> = GET "targets" => json;
        /// Creates a target at the tip of another, ordered by the server.
        fn create_target(request: &NewTarget) -> Head = POST "targets", json(request) => json;
        /// The acceptances of a target, oldest first.
        fn log(query: &LogQuery) -> Vec<LogEntry> = GET "log", query(query) => json;
        /// Submits a change, or a target's tip, to a target's landing
        /// queue, which the server runs.
        fn land(request: &LandRequest) -> Landings = POST "land", json(request) => json;
        /// The landings that wait, and why.
        fn landings() -> Vec<Pending> = GET "landings" => json;
        /// The events after a cursor, waiting up to the query's `wait` for
        /// some: pass the page's `next` as the next query's cursor.
        fn events(query: &EventsQuery) -> Page = GET "events", query(query) => json;
    }
}

/// A connection to an origin server. Cloning it is cheap, and clones share
/// their connections.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    /// The API's base URL, ending with a `/`.
    api: String,
}

impl Client {
    /// A client for the server at `url`, such as `http://127.0.0.1:7878`.
    #[must_use]
    pub fn new(url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api: format!("{}/api/v1/", url.trim_end_matches('/')),
        }
    }

    /// A repository on the server. Sends no request.
    #[must_use]
    pub fn repo(&self, name: RepoName) -> Repo {
        Repo {
            client: self.clone(),
            name,
        }
    }

    /// Starts a request to `path`, relative to the API's base URL.
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http.request(method, format!("{}{path}", self.api))
    }
}

/// A repository on a server.
#[derive(Clone, Debug)]
pub struct Repo {
    client: Client,
    name: RepoName,
}

impl Repo {
    /// The repository's name.
    #[must_use]
    pub fn name(&self) -> &RepoName {
        &self.name
    }

    /// Starts a request to `path`, relative to the repository's URL.
    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let path = format!("repos/{}/{path}", self.name.as_str());
        self.client.request(method, &path)
    }
}

/// A successful response, read in the form its endpoint answers with.
struct Response(reqwest::Response);

impl Response {
    /// Sends a request, turning error statuses into errors.
    async fn send(request: RequestBuilder) -> Result<Self, Error> {
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(Self(response));
        }
        Err(match response.json().await {
            Ok(error) => Error::Api(error),
            Err(_) => Error::Status(status.as_u16()),
        })
    }

    async fn json<T: DeserializeOwned>(self) -> Result<T, Error> {
        Ok(self.0.json().await?)
    }

    async fn pack(self) -> Result<Pack, Error> {
        Ok(Pack::decode(&self.0.bytes().await?)?)
    }
}

/// A failed request.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server refused the request, and said why.
    #[error("the server refused: {0}")]
    Api(ApiError),
    /// The server answered with an error status, without saying why.
    #[error("the server answered with status {0}")]
    Status(u16),
    /// The server cannot be reached, or its response cannot be read.
    #[error("cannot reach the server")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The server sent invalid objects.
    #[error("the server sent invalid objects")]
    Pack(#[from] PackError),
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(Box::new(error))
    }
}
