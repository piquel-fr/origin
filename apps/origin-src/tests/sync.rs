//! End-to-end tests of cloning, pushing, and pulling, over HTTP and through
//! a local path.

mod common;

use common::{Remote, Repo};

/// Two people sharing a repository, and a third arriving late.
fn round_trip(remote: &Remote) {
    let alice = Repo::init();
    alice.write("README.md", "hello\n");
    alice.ok(&["commit", "-m", "First"]);
    alice.ok(&["remote", &remote.url]);
    assert!(alice.ok(&["push"]).starts_with("Pushed 1 commits"));

    let bob = Repo::clone(&remote.url);
    assert_eq!(bob.read("README.md"), "hello\n");
    assert_eq!(bob.ok(&["status"]), "On branch main\nNo changes.\n");
    bob.write("README.md", "hello, world\n");
    bob.write("src/lib.rs", "fn lib() {}\n");
    bob.ok(&["commit", "-m", "Second"]);
    bob.ok(&["push"]);

    assert!(alice.ok(&["pull"]).starts_with("Fast-forwarded"));
    assert_eq!(alice.read("README.md"), "hello, world\n");
    assert_eq!(alice.read("src/lib.rs"), "fn lib() {}\n");
    assert_eq!(alice.ok(&["status"]), "On branch main\nNo changes.\n");
    assert_eq!(alice.ok(&["push"]), "Everything up to date.\n");
    assert_eq!(alice.ok(&["pull"]), "Already up to date.\n");

    // Carol's clone only has the latest commit: the log fetches the rest.
    let carol = Repo::clone(&remote.url);
    assert_eq!(carol.log(), ["Second", "First"]);
}

#[test]
fn round_trip_over_http() {
    round_trip(&Remote::http());
}

#[test]
fn round_trip_through_a_path() {
    round_trip(&Remote::path());
}

#[test]
fn pushes_from_behind_are_rejected() {
    let remote = Remote::http();
    let alice = Repo::init();
    alice.write("file", "1\n");
    alice.ok(&["commit", "-m", "First"]);
    alice.ok(&["remote", &remote.url]);
    alice.ok(&["push"]);
    let bob = Repo::clone(&remote.url);

    alice.write("file", "2\n");
    alice.ok(&["commit", "-m", "Alice's"]);
    alice.ok(&["push"]);
    bob.write("other", "bob\n");
    bob.ok(&["commit", "-m", "Bob's"]);

    assert!(bob.fails(&["push"]).contains("moved"));
    assert!(bob.fails(&["pull"]).contains("local commits"));
    let carol = Repo::clone(&remote.url);
    assert_eq!(
        carol.log(),
        ["Alice's", "First"],
        "Bob's push changed nothing"
    );
}

#[test]
fn pulls_keep_uncommitted_changes() {
    let remote = Remote::path();
    let alice = Repo::init();
    alice.write("file", "1\n");
    alice.ok(&["commit", "-m", "First"]);
    alice.ok(&["remote", &remote.url]);
    alice.ok(&["push"]);
    let bob = Repo::clone(&remote.url);
    alice.write("file", "2\n");
    alice.ok(&["commit", "-m", "Second"]);
    alice.ok(&["push"]);

    bob.write("file", "bob's work\n");
    assert!(bob.fails(&["pull"]).contains("commit your changes first"));
    assert_eq!(bob.read("file"), "bob's work\n");
}

#[test]
fn cloning_an_empty_repository() {
    let remote = Remote::http();
    let alice = Repo::clone(&remote.url);
    assert_eq!(alice.log(), Vec::<String>::new());
    alice.write("file", "content\n");
    alice.ok(&["commit", "-m", "First"]);
    alice.ok(&["push"]);
    assert_eq!(Repo::clone(&remote.url).read("file"), "content\n");
}

#[test]
fn pulls_refuse_unrelated_history() {
    let (first, second) = (Remote::path(), Remote::path());
    for (remote, file) in [(&first, "mine"), (&second, "theirs")] {
        let repo = Repo::init();
        repo.write(file, "content\n");
        repo.ok(&["commit", "-m", file]);
        repo.ok(&["remote", &remote.url]);
        repo.ok(&["push"]);
    }
    let bob = Repo::clone(&first.url);

    // Replace the first repository's history with the second's.
    let (from, to) = (
        std::path::Path::new(&second.url),
        std::path::Path::new(&first.url),
    );
    let copied = std::process::Command::new("cp")
        .arg("-r")
        .arg(from.join("objects/."))
        .arg(to.join("objects"))
        .status()
        .unwrap();
    assert!(copied.success());
    std::fs::copy(from.join("branches"), to.join("branches")).unwrap();

    assert!(bob.fails(&["pull"]).contains("diverged"));
    assert_eq!(bob.read("mine"), "content\n");
}
