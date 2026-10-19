//! End to end: `origin repo create` and `origin serve`, running the real
//! binary and talking to it through the SDK.

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use origin_client::Client;
use origin_shared::api::RepoName;

/// A running `origin serve`, killed when dropped.
struct Serve(Child);

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn serves_the_repositories_it_created() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("origin-server.toml");
    fs::write(
        &config,
        "listen = \"127.0.0.1:0\"\nnode = \"hosted\"\n[repos]\ndemo = \"repos/demo\"\n",
    )
    .unwrap();
    let origin = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_origin"));
        command.args(args).arg("--config").arg(&config);
        command
    };

    let created = origin(&["repo", "create"])
        .arg(dir.path().join("repos/demo"))
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(dir.path().join("node.key").is_file(), "the server's key");

    let mut child = origin(&["serve"]).stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _serve = Serve(child);
    let url = line.trim().strip_prefix("Listening on ").unwrap();

    let client = Client::new(url);
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
