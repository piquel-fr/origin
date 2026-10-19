//! Helpers for the end-to-end tests, which run the real `origin` binary.
//! The tests of plugins, in other packages, include them too.

// Each test binary uses its own subset of the helpers, and the others would
// warn as unused in it.
#![allow(dead_code)]

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

/// The `origin` binary: this package's, or for a plugin's tests, the one
/// built beside the plugin's, in the target directory holding the tests.
pub fn origin() -> PathBuf {
    if let Some(origin) = option_env!("CARGO_BIN_EXE_origin") {
        return PathBuf::from(origin);
    }
    let exe = env::current_exe().unwrap();
    let origin = exe.parent().unwrap().parent().unwrap().join("origin");
    assert!(
        origin.exists(),
        "build `origin` first, such as with `cargo build --bin origin`"
    );
    origin
}

/// `$PATH`, starting with the directory holding `origin` and the plugins
/// built with it.
pub fn path_with_plugins() -> OsString {
    let bin = origin().parent().unwrap().to_owned();
    let paths = env::var_os("PATH").unwrap_or_default();
    env::join_paths([bin].into_iter().chain(env::split_paths(&paths))).unwrap()
}

/// A user, with a configuration directory, and so a node key, of their
/// own, and the token they send servers, if any. Clones share it.
#[derive(Clone)]
pub struct User {
    config: Arc<tempfile::TempDir>,
    token: Option<String>,
}

impl User {
    /// A user named `name`, whose node is named after them, with the token
    /// [`Server`] knows them by.
    pub fn new(name: &str) -> Self {
        Self {
            token: Some(Server::token(name)),
            ..Self::anonymous(name)
        }
    }

    /// A user named `name` without a token: servers don't know them.
    pub fn anonymous(name: &str) -> Self {
        let config = tempfile::tempdir().unwrap();
        fs::write(
            config.path().join("config.toml"),
            format!("user = \"{name}\"\n"),
        )
        .unwrap();
        Self {
            config: Arc::new(config),
            token: None,
        }
    }

    /// The user, sending `token` instead.
    pub fn with_token(&self, token: &str) -> Self {
        Self {
            token: Some(token.to_owned()),
            ..self.clone()
        }
    }

    /// The configuration directory.
    pub fn config(&self) -> &Path {
        self.config.path()
    }

    /// Starts a long-running command in `cwd`, returning it and its output
    /// lines as they come.
    pub fn spawn(&self, cwd: &Path, args: &[&str]) -> (Child, mpsc::Receiver<String>) {
        let mut child = self
            .command(cwd, args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            for line in stdout.lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        (child, receive)
    }

    /// `origin` with `args`, run in `cwd` as this user, with the plugins
    /// built with it on `$PATH`, keeping their state in the configuration
    /// directory.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(origin());
        command
            .args(args)
            .current_dir(cwd)
            .env("ORIGIN_CONFIG_DIR", self.config())
            .env("XDG_STATE_HOME", self.config().join("state"))
            .env("PATH", path_with_plugins())
            .env_remove("ORIGIN_TOKEN");
        if let Some(token) = &self.token {
            command.env("ORIGIN_TOKEN", token);
        }
        command
    }
}

/// A temporary working copy, and its user.
pub struct Repo {
    dir: tempfile::TempDir,
    user: User,
}

impl Repo {
    /// A new repository of a user of its own, `ada`.
    pub fn new() -> Self {
        Self::init(&User::new("ada"))
    }

    /// A new repository of `user`.
    pub fn init(user: &User) -> Self {
        let repo = Self::empty(user);
        repo.ok(&["init"]);
        repo
    }

    /// A clone of `url` by `user`, with more arguments for `clone`.
    pub fn clone(user: &User, url: &str, args: &[&str]) -> Self {
        let repo = Self::empty(user);
        let root = repo.root().to_str().unwrap().to_owned();
        repo.ok(&[&["clone", url, &root], args].concat());
        repo
    }

    fn empty(user: &User) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            user: user.clone(),
        }
    }

    /// The root of the working copy.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The user's configuration directory.
    pub fn config(&self) -> &Path {
        self.user.config()
    }

    /// The repository's user.
    pub fn user(&self) -> &User {
        &self.user
    }

    /// Runs `origin` in `cwd` as the repository's user.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.user.command(cwd, args).output().unwrap()
    }

    /// Starts a long-running command, such as `watch`, returning it and its
    /// output lines as they come.
    pub fn spawn(&self, args: &[&str]) -> (Child, mpsc::Receiver<String>) {
        self.user.spawn(self.root(), args)
    }

    /// The revision the working copy is, in full.
    pub fn revision(&self) -> String {
        self.read(".origin/workspace").trim_end().to_owned()
    }

    /// Runs a command that must succeed, returning its output.
    pub fn ok(&self, args: &[&str]) -> String {
        self.ok_in(self.root(), args)
    }

    /// Runs a command in `cwd` that must succeed, returning its output.
    pub fn ok_in(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "`origin {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Runs a command that must fail, returning its error.
    pub fn fails(&self, args: &[&str]) -> String {
        self.fails_in(self.root(), args)
    }

    /// Runs `origin` in `cwd`, which must fail, returning its errors.
    pub fn fails_in(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, args);
        assert!(
            !output.status.success(),
            "`origin {}` succeeded",
            args.join(" ")
        );
        String::from_utf8(output.stderr).unwrap()
    }

    pub fn path(&self, path: &str) -> PathBuf {
        self.root().join(path)
    }

    pub fn write(&self, path: &str, content: &str) {
        let path = self.path(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    pub fn read(&self, path: &str) -> String {
        fs::read_to_string(self.path(path)).unwrap()
    }

    /// The short id of the working copy's change.
    pub fn change(&self) -> String {
        let status = self.ok(&["status"]);
        let line = status.lines().next().unwrap();
        let headline = line.strip_prefix("Working copy: ").unwrap();
        headline.split(' ').next().unwrap().to_owned()
    }

    /// The full id of the working copy's change.
    pub fn full_change(&self) -> String {
        let show = self.ok(&["show"]);
        let line = show.lines().next().unwrap();
        let change = line.strip_prefix("Change ").unwrap();
        change.split(' ').next().unwrap().to_owned()
    }

    /// Creates a workspace at `path`, returning its change.
    pub fn workspace(&self, path: &Path, args: &[&str]) -> String {
        let path = path.to_str().unwrap();
        let output = self.ok(&[&["workspace", "new", path], args].concat());
        let line = output.lines().nth(1).unwrap();
        let headline = line.strip_prefix("Working copy: ").unwrap();
        headline.split(' ').next().unwrap().to_owned()
    }
}

/// A running `origin serve` hosting one repository, `demo`, which its node,
/// `hosted`, orders. Each of [`Server::USERS`] signs in with
/// [`Server::token`]. Killed when dropped.
pub struct Server {
    dir: tempfile::TempDir,
    child: Child,
    /// The server's base URL.
    pub base: String,
}

impl Server {
    /// The users the server knows.
    pub const USERS: [&str; 4] = ["ada", "bob", "carol", "alice"];

    /// The static token of `user`.
    pub fn token(user: &str) -> String {
        format!("token-{user}")
    }

    pub fn start() -> Self {
        Self::start_with(&[])
    }

    /// Starts a server whose repository `origin repo create` made with
    /// `args`, such as `--admin user:alice`.
    pub fn start_with(args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("origin-server.toml");
        let tokens: Vec<_> = Self::USERS
            .iter()
            .map(|user| format!("\"user:{user}\" = \"{}\"", Self::token(user)))
            .collect();
        let tokens = tokens.join("\n");
        fs::write(
            &config,
            format!(
                "listen = \"127.0.0.1:0\"\nnode = \"hosted\"\n[repos]\ndemo = \"repos/demo\"\n\
                 [tokens]\n{tokens}"
            ),
        )
        .unwrap();
        let server = |args: &[&str]| {
            let mut command = Command::new(origin());
            command.args(args).arg("--config").arg(&config);
            command
        };
        let created = server(&[&["repo", "create"], args].concat())
            .arg(dir.path().join("repos/demo"))
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let (child, base) = Self::serve(&config);
        Self { dir, child, base }
    }

    /// Runs `origin serve` with `config`, returning it and its base URL
    /// once it listens.
    fn serve(config: &Path) -> (Child, String) {
        let mut child = Command::new(origin())
            .args(["serve", "--config"])
            .arg(config)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let base = line
            .trim()
            .strip_prefix("Listening on ")
            .unwrap()
            .to_owned();
        (child, base)
    }

    /// Kills the server, runs `meanwhile`, then serves the repository again
    /// at the same address.
    pub fn restart(&mut self, meanwhile: impl FnOnce()) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        meanwhile();
        let config = self.dir().join("origin-server.toml");
        let address = self.base.strip_prefix("http://").unwrap();
        let toml = fs::read_to_string(&config).unwrap();
        let toml = toml.replace("127.0.0.1:0", address);
        fs::write(&config, toml).unwrap();
        let (child, base) = Self::serve(&config);
        assert_eq!(base, self.base);
        self.child = child;
    }

    /// The directory holding the server's configuration, key and
    /// repositories.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// The URL of `demo`.
    pub fn url(&self) -> String {
        format!("{}/demo", self.base)
    }

    /// Adds `rules` to `demo`'s `policy.toml`, landing them on `policy` as
    /// the server's own user, who runs its node, through the repository's
    /// path.
    pub fn add_policy(&self, rules: &str) {
        let operator = User::anonymous("operator");
        fs::write(
            operator.config().join("config.toml"),
            "user = \"operator\"\nnode = \"hosted\"\n",
        )
        .unwrap();
        fs::copy(
            self.dir().join("node.key"),
            operator.config().join("node.key"),
        )
        .unwrap();
        let path = self.dir().join("repos/demo");
        let policy = Repo::clone(&operator, path.to_str().unwrap(), &["--target", "policy"]);
        let mut toml = policy.read("policy.toml");
        toml.push_str(rules);
        policy.write("policy.toml", &toml);
        policy.ok(&["describe", "Change the rules"]);
        let landed = policy.ok(&["land"]);
        assert!(landed.contains("Landed change"), "{landed}");
    }

    /// Signs a token for `principal` with `scope` with the server's key.
    pub fn issue_token(&self, principal: &str, scope: &[&str]) -> String {
        let mut command = Command::new(origin());
        command
            .args(["token", "issue", "--as", principal, "--config"])
            .arg(self.dir().join("origin-server.toml"));
        for capability in scope {
            command.args(["--scope", capability]);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
