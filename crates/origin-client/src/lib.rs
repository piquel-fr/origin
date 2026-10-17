//! The origin SDK: a typed client for origin servers.
//!
//! ```no_run
//! use origin_client::Client;
//! use origin_shared::api::RepoName;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::new("http://127.0.0.1:7878").with_token("a long random secret");
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

use origin_shared::access::Rights;
use origin_shared::api::{
    Annotation, ApiError, BrowseQuery, EventsQuery, FetchRecords, FetchSnapshot, Inventory,
    InventoryRequest, IssuedToken, LandRequest, Landings, Listing, LogEntry, LogQuery, NewTarget,
    PromoteRequest, RepoName, RepoSummary, Revisions, StatementsQuery, Stored, TargetInfo,
    TokenRequest,
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
        /// Fetches the records the caller sees by id, in the order asked,
        /// each followed by the attachment it carries if the server has
        /// it: a revision's description, a statement's payload.
        fn fetch_records(request: &FetchRecords) -> Pack =
            POST "records/fetch", json(request) => pack;
        /// Fetches the trees and blobs of a record's snapshot that none of
        /// the request's snapshots holds at the same path, parents first,
        /// but the content the caller may not read.
        fn fetch_snapshot(request: &FetchSnapshot) -> Pack =
            POST "snapshots/fetch", json(request) => pack;
        /// Puts a batch of objects: snapshots, revisions and statements,
        /// each needing only objects the batch or the repository has.
        fn put_records(pack: &Pack) -> Stored = PUT "records", body(pack.encode()) => json;
        /// The statements about a record, in the order the repository
        /// received them.
        fn statements(query: &StatementsQuery) -> Vec<Annotation> =
            GET "statements", query(query) => json;
        /// Promotes results to a record from the records with the same
        /// input digest, as statements the server issues, returning them,
        /// and retries the landings waiting for them.
        fn promote(request: &PromoteRequest) -> Vec<Annotation> =
            POST "promote", json(request) => json;
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
        /// Who the server thinks the caller is, and what they may do.
        fn whoami() -> Rights = GET "whoami" => json;
        /// Issues a token acting for the caller, with a narrower scope.
        fn issue_token(request: &TokenRequest) -> IssuedToken =
            POST "tokens", json(request) => json;
        /// Lists a directory of a target's tip, or of a revision's or an
        /// acceptance's snapshot.
        fn tree(query: &BrowseQuery) -> Listing = GET "tree", query(query) => json;
        /// Reads a file of a snapshot, as `tree` finds it.
        fn file(query: &BrowseQuery) -> Vec<u8> = GET "file", query(query) => bytes;
    }
}

/// A connection to an origin server. Cloning it is cheap, and clones share
/// their connections.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    /// The API's base URL, ending with a `/`.
    api: String,
    /// The bearer token every request carries, if any.
    token: Option<String>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token.
        f.debug_struct("Client")
            .field("api", &self.api)
            .field("authenticated", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for the server at `url`, such as `http://127.0.0.1:7878`.
    #[must_use]
    pub fn new(url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api: format!("{}/api/v1/", url.trim_end_matches('/')),
            token: None,
        }
    }

    /// The client, authenticating every request with `token`: a static
    /// one from the server's configuration, or one it issued. Without one,
    /// the server treats requests as anonymous, which only read.
    #[must_use]
    pub fn with_token(self, token: impl Into<String>) -> Self {
        Self {
            token: Some(token.into()),
            ..self
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
        let request = self.http.request(method, format!("{}{path}", self.api));
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
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

    async fn bytes(self) -> Result<Vec<u8>, Error> {
        Ok(self.0.bytes().await?.to_vec())
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
