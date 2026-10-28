//! End-to-end tests of sharing work: cloning, pushing, fetching and
//! pulling, and landing and watching against a remote, over HTTP and
//! through a local path.

mod common;

use std::fs;
use std::time::Duration;

use common::{Repo, Server, User};

#[test]
fn work_lands_through_the_server_and_reaches_other_clones() {
    let server = Server::start();
    let (ada, bob) = (User::new("ada"), User::new("bob"));
    let alice = Repo::clone(&ada, &server.url(), &[]);
    assert!(
        alice
            .ok(&["status"])
            .ends_with("On top of: main #0\nNo changes.\n")
    );
    alice.write("README.md", "hello\n");
    alice.write("src/lib.rs", "fn lib() {}\n");
    alice.ok(&["describe", "Add the library"]);
    let change = alice.change();
    assert!(alice.ok(&["push"]).starts_with("Pushed "));
    assert_eq!(alice.ok(&["push"]), "Everything up to date.\n");
    // The server orders `main`, so the landing goes to its queue.
    let landed = alice.ok(&["land"]);
    assert!(
        landed.contains(&format!("Landed change {change}")),
        "{landed}"
    );
    assert!(landed.contains("on main #1"), "{landed}");

    let bobs = Repo::clone(&bob, &server.url(), &[]);
    assert_eq!(bobs.read("README.md"), "hello\n");
    assert_eq!(bobs.read("src/lib.rs"), "fn lib() {}\n");
    let log = bobs.ok(&["log"]);
    assert!(log.contains(&format!("{change} Add the library")), "{log}");

    // Bob works on top while Alice lands more: pulling moves his work onto
    // the new tip.
    bobs.write("notes.md", "bob\n");
    alice.write("README.md", "hello, world\n");
    alice.ok(&["land"]);
    let pulled = bobs.ok(&["pull"]);
    assert!(pulled.starts_with("`main`: #1 -> #2\n"), "{pulled}");
    assert_eq!(bobs.read("README.md"), "hello, world\n");
    assert_eq!(bobs.read("notes.md"), "bob\n");
    let status = bobs.ok(&["status"]);
    assert!(
        status.contains("On top of: main #2\nA notes.md\n"),
        "{status}"
    );
    assert!(bobs.ok(&["pull"]).ends_with("on the tip of `main`.\n"));
}

#[test]
fn pushing_a_change_that_moved_on_the_server_is_refused() {
    let server = Server::start();
    let (ada, bob) = (User::new("ada"), User::new("bob"));
    let alice = Repo::clone(&ada, &server.url(), &[]);
    alice.write("file", "alice\n");
    alice.ok(&["describe", "Shared work"]);
    alice.ok(&["push"]);

    // Bob takes the change over, and pushes a revision of it.
    let bobs = Repo::clone(&bob, &server.url(), &[]);
    let change = alice.full_change();
    bobs.ok(&["fetch", "--change", &change]);
    bobs.ok(&["edit", &change]);
    assert_eq!(bobs.read("file"), "alice\n");
    bobs.write("file", "bob\n");
    bobs.ok(&["push", &change]);

    // Alice's next revision is stale: she learns before sending anything.
    alice.write("file", "alice again\n");
    let error = alice.fails(&["push"]);
    assert!(error.contains("is stale"), "{error}");
    alice.ok(&["fetch"]);
    assert!(alice.ok(&["status"]).contains("Divergent"));
    alice.ok(&["converge", &change]);
    assert!(alice.ok(&["push"]).starts_with("Pushed "));
}

#[test]
fn watch_follows_the_remote_events() {
    let server = Server::start();
    let ada = User::new("ada");
    let alice = Repo::clone(&ada, &server.url(), &[]);
    let watcher = Repo::clone(&ada, &server.url(), &[]);
    let (mut watch, lines) = watcher.spawn(&["watch", "--kind", "target.advanced"]);
    // Let the long-poll start, so the landing comes after its cursor.
    std::thread::sleep(Duration::from_millis(500));
    alice.write("file", "content\n");
    alice.ok(&["land"]);
    let line = lines.recv_timeout(Duration::from_secs(20)).unwrap();
    watch.kill().unwrap();
    watch.wait().unwrap();
    assert!(line.contains("target.advanced target=main seq=1"), "{line}");
}

#[test]
fn local_paths_are_remotes_served_in_process() {
    let ada = User::new("ada");
    let origin = Repo::init(&ada);
    origin.write("README.md", "hello\n");
    origin.ok(&["land"]);

    // Ada's node orders the repository, so her clone's queue, running in
    // process, lands in it.
    let url = origin.root().to_str().unwrap();
    let clone = Repo::clone(&ada, url, &[]);
    assert_eq!(clone.read("README.md"), "hello\n");
    clone.write("src/lib.rs", "fn lib() {}\n");
    let landed = clone.ok(&["land"]);
    assert!(landed.contains("on main #2"), "{landed}");
    let log = origin.ok(&["log", "main"]);
    assert!(log.contains("◆ main #2"), "{log}");

    // Bob's node doesn't: the queue refuses him.
    let bobs = Repo::clone(&User::new("bob"), url, &[]);
    assert_eq!(bobs.read("src/lib.rs"), "fn lib() {}\n");
    bobs.write("bob.txt", "bob\n");
    let error = bobs.fails(&["land"]);
    assert!(error.contains("authority"), "{error}");
}

#[test]
fn clones_follow_one_target_and_fetch_others_on_demand() {
    let ada = User::new("ada");
    let origin = Repo::init(&ada);
    origin.write("README.md", "main\n");
    origin.ok(&["land"]);
    origin.ok(&["target", "new", "release", "--from", "main"]);
    origin.ok(&["new", "--on", "release"]);
    origin.write("RELEASE.md", "v1\n");
    origin.ok(&["land"]);

    let url = origin.root().to_str().unwrap();
    let clone = Repo::clone(&User::new("bob"), url, &["--target", "release"]);
    assert_eq!(clone.read("RELEASE.md"), "v1\n");
    assert!(clone.ok(&["log"]).contains("◆ release #1"));
    let error = clone.fails(&["log", "main"]);
    assert!(error.contains("main"), "not followed: {error}");
    let fetched = clone.ok(&["fetch", "--target", "main"]);
    assert!(fetched.contains("`main`: now followed, at #1"), "{fetched}");
    assert!(clone.ok(&["log", "main"]).starts_with("◆ main #1"));

    let missing = clone.fails(&["clone", url, "elsewhere", "--target", "nope"]);
    assert!(missing.contains("no target `nope`"), "{missing}");
    assert!(
        !clone.path("elsewhere").exists(),
        "a failed clone leaves nothing"
    );
}

#[test]
fn a_move_that_cannot_fetch_moves_nothing() {
    let ada = User::new("ada");
    let origin = Repo::init(&ada);
    origin.write("x", "x\n");
    origin.ok(&["describe", "X"]);
    let change = origin.full_change();
    let url = origin.root().to_str().unwrap();
    let clone = Repo::clone(&ada, url, &[]);
    clone.ok(&["fetch", "--change", &change]);

    // The remote is gone while `edit` needs the change's files from it.
    let away = origin.root().with_extension("away");
    fs::rename(origin.root(), &away).unwrap();
    let error = clone.fails(&["edit", &change]);
    assert!(error.contains("remote"), "{error}");
    assert!(!clone.path(".origin/moving").exists());
    clone.write("notes", "mine\n");
    let status = clone.ok(&["status"]);
    assert!(status.contains("A notes\n"), "{status}");
    fs::rename(&away, origin.root()).unwrap();
    clone.ok(&["edit", &change]);
    assert_eq!(clone.read("x"), "x\n");
}
