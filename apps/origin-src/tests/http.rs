//! The HTTP API, driven through the SDK.

use std::collections::BTreeMap;

use origin_client::{Client, Error, Repo};
use origin_shared::api::{CommitsRequest, ErrorCode, Location, ObjectsRequest, RepoName};
use origin_shared::pack::{Pack, PushRequest};
use origin_shared::{
    BranchName, Commit, EncodedObject, EntryKind, EntryName, Object, ObjectId, RefUpdate, RepoPath,
    Signature, Tree, TreeEntry,
};
use origin_src::http::Api;
use origin_src::repository::{RepoConfig, Repository};
use origin_src::service::OriginService;

/// A server hosting one repository with two commits on `main`:
/// `README.md`, then `src/lib.rs` added.
struct Server {
    dir: tempfile::TempDir,
    client: Client,
    repo: Repo,
    commits: [ObjectId; 2],
}

impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repository = Repository::create(
            &dir.path().join("demo"),
            &RepoConfig {
                description: "A demo".into(),
            },
        )
        .unwrap();
        let write = |(id, objects): (ObjectId, Pack)| {
            for object in &objects.objects {
                repository.objects().write(object).unwrap();
            }
            id
        };
        let first = write(Self::commit(&[("README.md", "hello\n")], None));
        let second = write(Self::commit(
            &[("README.md", "hello\n"), ("src/lib.rs", "fn lib() {}\n")],
            Some(first),
        ));
        repository
            .branches()
            .update(&[RefUpdate {
                branch: BranchName::default(),
                expected: None,
                new: Some(second),
            }])
            .unwrap();

        let repo = RepoName::new("demo").unwrap();
        let service = OriginService::new(BTreeMap::from([(repo.clone(), dir.path().join("demo"))]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, Api::router(service)).await });
        let client = Client::new(&format!("http://{address}"));
        Self {
            dir,
            repo: client.repo(repo),
            client,
            commits: [first, second],
        }
    }

    /// Builds a commit of `files`, which are at most one directory deep,
    /// returning it with every object it contains.
    fn commit(files: &[(&str, &str)], parent: Option<ObjectId>) -> (ObjectId, Pack) {
        let objects = std::cell::RefCell::new(Pack::default());
        let write = |object: Object| {
            let encoded = object.encode();
            let id = encoded.id();
            objects.borrow_mut().objects.push(encoded);
            id
        };
        let mut dirs: BTreeMap<&str, Tree> = BTreeMap::new();
        for (path, content) in files {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            let entry = TreeEntry {
                kind: EntryKind::File,
                id: write(Object::Blob(content.as_bytes().to_vec())),
            };
            dirs.entry(dir)
                .or_default()
                .insert(EntryName::new(name.to_owned()).unwrap(), entry);
        }
        let mut root = dirs.remove("").unwrap_or_default();
        for (dir, tree) in dirs {
            let entry = TreeEntry {
                kind: EntryKind::Tree,
                id: write(Object::Tree(tree)),
            };
            root.insert(EntryName::new(dir).unwrap(), entry);
        }
        let commit = write(Object::Commit(Commit {
            tree: write(Object::Tree(root)),
            parents: parent.into_iter().collect(),
            author: Signature::now("Ada".into(), "ada@example.com".into()),
            message: "commit".into(),
        }));
        (commit, objects.into_inner())
    }

    async fn main(&self) -> ObjectId {
        self.repo.info().await.unwrap().branches[0].commit
    }

    async fn push(&self, expected: ObjectId, new: ObjectId, pack: Pack) -> Result<(), ErrorCode> {
        let push = PushRequest {
            updates: vec![RefUpdate {
                branch: BranchName::default(),
                expected: Some(expected),
                new: Some(new),
            }],
            pack,
        };
        self.repo.push(&push).await.map_err(code)
    }
}

fn path(path: &str) -> RepoPath {
    RepoPath::new(path).unwrap()
}

/// `file` on `main`.
fn on_main(file: &str) -> Location {
    Location {
        branch: BranchName::default(),
        path: path(file),
    }
}

/// The error code of a refused request.
fn code(error: Error) -> ErrorCode {
    match error {
        Error::Api(error) => error.code,
        error => panic!("expected an API error, got {error}"),
    }
}

#[tokio::test]
async fn lists_and_describes_repositories() {
    let server = Server::start().await;
    let repos = server.client.repos().await.unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(&repos[0].name, server.repo.name());
    assert_eq!(repos[0].description, "A demo");

    let info = server.repo.info().await.unwrap();
    assert_eq!(info.branches.len(), 1);
    assert_eq!(info.branches[0].name, BranchName::default());
    assert_eq!(info.branches[0].commit, server.commits[1]);

    let missing = server.client.repo(RepoName::new("missing").unwrap());
    assert_eq!(code(missing.info().await.unwrap_err()), ErrorCode::NotFound);
}

#[tokio::test]
async fn browses_directories_and_files() {
    let server = Server::start().await;

    let root = server.repo.list_tree(&on_main("")).await.unwrap();
    let names: Vec<_> = root.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["README.md", "src"]);
    assert_eq!(root[1].kind, EntryKind::Tree);

    let file = server.repo.read_file(&on_main("src/lib.rs")).await.unwrap();
    assert_eq!(file, b"fn lib() {}\n");

    let error = server.repo.read_file(&on_main("src")).await.unwrap_err();
    assert_eq!(code(error), ErrorCode::BadRequest);
    let error = server
        .repo
        .list_tree(&on_main("missing"))
        .await
        .unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);
}

#[tokio::test]
async fn fetches_verified_packs() {
    let server = Server::start().await;
    let [first, second] = server.commits;
    let ids = |objects: &[EncodedObject]| objects.iter().map(EncodedObject::id).collect::<Vec<_>>();

    let request = CommitsRequest {
        ids: vec![second],
        depth: 1,
    };
    let pack = server.repo.fetch_commits(&request).await.unwrap();
    assert_eq!(ids(&pack.objects), [second]);

    let request = CommitsRequest {
        ids: vec![second],
        depth: 10,
    };
    let pack = server.repo.fetch_commits(&request).await.unwrap();
    assert_eq!(ids(&pack.objects), [second, first]);

    let request = ObjectsRequest {
        commit: second,
        paths: vec![RepoPath::root(), path("src/lib.rs")],
    };
    let pack = server.repo.fetch_objects(&request).await.unwrap();
    assert!(pack.objects[0].decode_tree().unwrap().get("src").is_some());
    assert_eq!(pack.objects[1].blob_content().unwrap(), b"fn lib() {}\n");
}

#[tokio::test]
async fn serves_its_openapi_document() {
    let server = Server::start().await;
    let document = server.client.openapi().await.unwrap();
    assert_eq!(document, serde_json::to_value(Api::openapi()).unwrap());
}

/// `docs/openapi.json` documents the API for other languages. Run `just
/// openapi` to rewrite it after changing the API.
#[test]
fn checked_in_openapi_document_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/openapi.json");
    let document = format!("{}\n", Api::openapi().to_pretty_json().unwrap());
    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(path, document).unwrap();
    } else {
        let checked_in = std::fs::read_to_string(path).unwrap();
        assert!(
            checked_in == document,
            "docs/openapi.json is stale: run `just openapi`"
        );
    }
}

#[tokio::test]
async fn pushes_move_branches() {
    let server = Server::start().await;
    let [_, second] = server.commits;
    let files = [
        ("README.md", "hello\n"),
        ("src/lib.rs", "fn lib() { changed() }\n"),
    ];
    let (third, pack) = Server::commit(&files, Some(second));

    server.push(second, third, pack).await.unwrap();
    assert_eq!(server.main().await, third);
    let file = server.repo.read_file(&on_main("src/lib.rs")).await.unwrap();
    assert_eq!(file, b"fn lib() { changed() }\n");
}

#[tokio::test]
async fn pushes_from_stale_clients_conflict() {
    let server = Server::start().await;
    let [first, second] = server.commits;
    let (other, pack) = Server::commit(&[("other", "content")], Some(first));

    let error = server.push(first, other, pack).await.unwrap_err();
    assert_eq!(error, ErrorCode::Conflict);
    assert_eq!(server.main().await, second, "the branch did not move");
}

#[tokio::test]
async fn pushes_must_include_every_new_object() {
    let server = Server::start().await;
    let [_, second] = server.commits;
    let files = [("README.md", "hello\n"), ("new.txt", "new\n")];
    let (third, mut pack) = Server::commit(&files, Some(second));
    // Drop the new blob: the server cannot accept a commit it cannot serve.
    pack.objects
        .retain(|object| object.blob_content().ok() != Some(b"new\n".as_slice()));

    let error = server.push(second, third, pack).await.unwrap_err();
    assert_eq!(error, ErrorCode::BadRequest);
    assert_eq!(server.main().await, second, "the branch did not move");
}

#[tokio::test]
async fn pushes_store_only_checked_objects() {
    let server = Server::start().await;
    let [_, second] = server.commits;
    let files = [("README.md", "hello\n"), ("new.txt", "new\n")];
    let (third, mut pack) = Server::commit(&files, Some(second));
    let stray = EncodedObject::blob(b"not part of any commit");
    pack.objects.push(stray.clone());

    server.push(second, third, pack).await.unwrap();
    let store = origin_src::store::ObjectStore::new(server.dir.path().join("demo/objects"));
    assert!(store.contains(third));
    assert!(
        !store.contains(stray.id()),
        "objects no commit refers to are dropped"
    );
}
