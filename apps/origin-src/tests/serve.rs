//! End to end: `origin repo create` and `origin serve`, running the real
//! binary and talking to it through the SDK.

mod common;

use common::Server;
use origin_client::Client;
use origin_shared::api::RepoName;

#[tokio::test]
async fn serves_the_repositories_it_created() {
    let server = Server::start();
    assert!(server.dir().join("node.key").is_file(), "the server's key");

    let client = Client::new(&server.base);
    let repos = client.repos().await.unwrap();
    assert_eq!(repos.len(), 1);
    let targets = client
        .repo(RepoName::new("demo").unwrap())
        .targets()
        .await
        .unwrap();
    assert!(
        targets
            .iter()
            .all(|target| target.authority == Some("hosted".parse().unwrap())),
        "the server orders the targets: {targets:?}"
    );
}
