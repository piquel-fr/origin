//! Development tasks, run with `cargo xtask <task>`:
//!
//! - `bench-git [--files N] [--tree PATH]`: times origin against git on the
//!   same working tree with hyperfine, locally and syncing with a server,
//!   and writes `docs/benchmarks.md`. The tree is a synthetic fixture of N
//!   files, or a copy of PATH.
//! - `edit DIR COUNT`: appends a line to COUNT files spread across DIR. The
//!   benchmarks use it to change files between runs.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, iter};

use anyhow::{Context, Result, bail, ensure};
use origin_src::fixture::FixtureTree;

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("bench-git") => BenchGit::parse(args)?.run(),
        Some("edit") => {
            let (Some(dir), Some(count)) = (args.next(), args.next()) else {
                bail!("usage: cargo xtask edit DIR COUNT");
            };
            edit(
                Path::new(&dir),
                count.parse().context("COUNT must be a number")?,
            )
        }
        _ => bail!("usage: cargo xtask <bench-git [--files N] [--tree PATH] | edit DIR COUNT>"),
    }
}

/// Appends a line to `count` files spread evenly across `dir`, so that both
/// tools see real content changes.
fn edit(dir: &Path, count: usize) -> Result<()> {
    fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            if name == ".git" || name == ".origin" {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                walk(&entry.path(), files)?;
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(dir, &mut files)?;
    let step = (files.len() / count.max(1)).max(1);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for file in files.iter().step_by(step).take(count) {
        let mut file = OpenOptions::new().append(true).open(file)?;
        writeln!(file, "// edited {stamp}")?;
    }
    Ok(())
}

/// Times origin against git with hyperfine.
struct BenchGit {
    files: usize,
    tree: Option<PathBuf>,
}

impl BenchGit {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut task = Self {
            files: FixtureTree::LARGE.files,
            tree: None,
        };
        while let Some(arg) = args.next() {
            let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--files" => task.files = value()?.parse().context("--files must be a number")?,
                "--tree" => task.tree = Some(value()?.into()),
                _ => bail!("unknown option {arg}, expected --files N or --tree PATH"),
            }
        }
        Ok(task)
    }

    fn run(&self) -> Result<()> {
        // Fail before the slow build and setup if hyperfine is missing.
        run(Command::new("hyperfine")
            .arg("--version")
            .stdout(Stdio::null()))?;
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        run(Command::new(env!("CARGO"))
            .args([
                "build",
                "--release",
                "--package",
                "origin-src",
                "--bin",
                "origin",
            ])
            .current_dir(&workspace))?;

        let base = workspace.join("target/bench-git");
        if base.exists() {
            fs::remove_dir_all(&base)?;
        }
        let config = base.join("config");
        fs::create_dir_all(&config)?;
        fs::write(config.join("config.toml"), "user = \"bench\"\n")?;
        for dir in ["origin", "git"] {
            self.populate(&base.join(dir))?;
        }
        let mut bench = Bench {
            origin: workspace.join("target/release/origin").canonicalize()?,
            xtask: env::current_exe()?,
            origin_dir: base.join("origin").canonicalize()?,
            git_dir: base.join("git").canonicalize()?,
            config,
            base,
            report: self.report_header()?,
        };
        bench.local()?;
        bench.sync(self)?;

        let path = workspace.join("docs/benchmarks.md");
        fs::create_dir_all(path.parent().context("docs has a parent")?)?;
        fs::write(&path, bench.report)?;
        println!("Wrote {}", path.canonicalize()?.display());
        Ok(())
    }

    /// Fills `dir` with the working tree under test.
    fn populate(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        match &self.tree {
            Some(tree) => {
                run(Command::new("cp").arg("-a").arg(tree.join(".")).arg(dir))?;
                for metadata in [".git", ".origin"] {
                    let path = dir.join(metadata);
                    if path.exists() {
                        fs::remove_dir_all(path)?;
                    }
                }
            }
            None => FixtureTree {
                files: self.files,
                ..FixtureTree::LARGE
            }
            .write(dir)?,
        }
        Ok(())
    }

    /// A git identity, and no user git configuration (hooks, fsmonitor)
    /// skewing results. Origin's identity is in its configuration directory.
    fn environment() -> impl Iterator<Item = (&'static str, &'static str)> {
        iter::empty()
            .chain([
                ("GIT_AUTHOR_NAME", "Bench"),
                ("GIT_AUTHOR_EMAIL", "bench@example.com"),
            ])
            .chain([
                ("GIT_COMMITTER_NAME", "Bench"),
                ("GIT_COMMITTER_EMAIL", "bench@example.com"),
            ])
            .chain([
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ("GIT_CONFIG_NOSYSTEM", "1"),
            ])
    }

    fn report_header(&self) -> Result<String> {
        let cpu = fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|info| {
                info.lines()
                    .find_map(|line| line.strip_prefix("model name"))
                    .map(|name| name.trim_start_matches([' ', '\t', ':']).to_owned())
            })
            .unwrap_or_else(|| "unknown".to_owned());
        let git = String::from_utf8(Command::new("git").arg("--version").output()?.stdout)?;
        let tree = match &self.tree {
            Some(tree) => format!("a copy of `{}`", tree.display()),
            None => format!("a synthetic tree of {} files", self.files),
        };
        Ok(format!(
            "# Benchmarks: origin vs git\n\n\
             Generated by `just bench-git` on {tree}.\n\n\
             - CPU: {cpu}\n- {}\n",
            git.trim()
        ))
    }
}

/// One side of a scenario: a command preparing each run, if any, and the
/// timed command.
type Side = (Option<String>, String);

/// A benchmark run: the two working copies under test, and the report.
struct Bench {
    origin: PathBuf,
    xtask: PathBuf,
    base: PathBuf,
    config: PathBuf,
    origin_dir: PathBuf,
    git_dir: PathBuf,
    report: String,
}

impl Bench {
    fn shell(dir: &Path, command: &str) -> String {
        format!("cd {} && {command}", dir.display())
    }

    /// An origin command in `dir`.
    fn origin_in(&self, dir: &Path, command: &str) -> String {
        Self::shell(dir, &format!("{} {command}", self.origin.display()))
    }

    /// An origin command in origin's working copy.
    fn origin(&self, command: &str) -> String {
        self.origin_in(&self.origin_dir, command)
    }

    /// A git command in git's working copy.
    fn git(&self, command: &str) -> String {
        Self::shell(&self.git_dir, command)
    }

    /// Edits 100 files of a working copy.
    fn edit(&self, dir: &Path) -> String {
        format!("{} edit {} 100", self.xtask.display(), dir.display())
    }

    /// Runs origin with `args` in `dir`, outside of hyperfine.
    fn run_origin(&self, dir: &Path, args: &[&str]) -> Result<()> {
        run(Command::new(&self.origin)
            .args(args)
            .current_dir(dir)
            .env("ORIGIN_CONFIG_DIR", &self.config)
            .stdout(Stdio::null()))
    }

    /// Runs git with `args` in `dir`, outside of hyperfine.
    fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
        run(Command::new("git")
            .args(args)
            .current_dir(dir)
            .envs(BenchGit::environment())
            .stdout(Stdio::null()))
    }

    /// Times both sides with hyperfine and appends the results to the report.
    fn scenario(&mut self, title: &str, runs: &[&str], origin: Side, git: Side) -> Result<()> {
        let markdown = self.base.join("scenario.md");
        let mut hyperfine = Command::new("hyperfine");
        // The commands are shell syntax, which hyperfine 2 makes explicit.
        hyperfine
            .args(runs)
            .arg("--shell=sh")
            .arg("--export-markdown")
            .arg(&markdown);
        for (name, (prepare, command)) in [("origin", origin), ("git", git)] {
            if let Some(prepare) = prepare {
                hyperfine.arg("--prepare").arg(prepare);
            }
            hyperfine.arg("--command-name").arg(name).arg(command);
        }
        run(hyperfine
            .envs(BenchGit::environment())
            .env("ORIGIN_CONFIG_DIR", &self.config))?;
        writeln!(
            self.report,
            "\n## {title}\n\n{}",
            fs::read_to_string(&markdown)?
        )?;
        Ok(())
    }

    /// Recording and inspecting the working copy. Origin has no commit:
    /// `new` records the working copy into the current change and starts the
    /// next one, the closest operation.
    fn local(&mut self) -> Result<()> {
        let init = Self::shell(
            &self.origin_dir,
            &format!("rm -rf .origin && {} init", self.origin.display()),
        );
        self.scenario(
            "First commit of the whole tree (`origin new`)",
            &["--runs", "5"],
            (Some(init), self.origin("new bench")),
            (
                Some(self.git("rm -rf .git && git init -q -b main")),
                self.git("git add -A && git commit -qm bench"),
            ),
        )?;
        self.scenario(
            "Status, nothing changed",
            &["--warmup", "3"],
            (None, self.origin("status")),
            (None, self.git("git status --porcelain")),
        )?;
        self.scenario(
            "Status, 100 files edited",
            &["--warmup", "1"],
            (Some(self.edit(&self.origin_dir)), self.origin("status")),
            (
                Some(self.edit(&self.git_dir)),
                self.git("git status --porcelain"),
            ),
        )?;
        self.scenario(
            "Commit, 100 files edited (`origin new`)",
            &["--warmup", "1"],
            (Some(self.edit(&self.origin_dir)), self.origin("new bench")),
            (
                Some(self.edit(&self.git_dir)),
                self.git("git add -A && git commit -qm bench"),
            ),
        )
    }

    /// Cloning, pushing and pulling. Origin serves over HTTP on localhost;
    /// git uses its pack protocol on a local bare repository, which spares
    /// it the network stack. Origin's side starts from a clone with the
    /// tree landed, and git's from its working copy, pushed.
    fn sync(&mut self, task: &BenchGit) -> Result<()> {
        let server = Server::start(&self.origin, &self.base, &self.config)?;
        let (seed, clone) = (
            self.base.join("origin-seed"),
            self.base.join("origin-clone"),
        );
        self.run_origin(&self.base, &["clone", &server.url, &seed.to_string_lossy()])?;
        task.populate(&seed)?;
        self.run_origin(&seed, &["land"])?;
        let (remote, git_clone) = (self.base.join("remote.git"), self.base.join("git-clone"));
        let remote_path = remote.to_string_lossy();
        Self::run_git(
            &self.base,
            &["init", "-q", "--bare", "-b", "main", &remote_path],
        )?;
        Self::run_git(&self.git_dir, &["push", "-q", &remote_path, "HEAD:main"])?;
        let push = format!("git push -q {} HEAD:main", remote.display());

        self.scenario(
            "Clone",
            &["--runs", "5"],
            (
                Some(format!("rm -rf {}", clone.display())),
                format!(
                    "{} clone {} {}",
                    self.origin.display(),
                    server.url,
                    clone.display()
                ),
            ),
            (
                Some(format!("rm -rf {}", git_clone.display())),
                format!(
                    "git clone -q --no-local {} {}",
                    remote.display(),
                    git_clone.display()
                ),
            ),
        )?;
        // `describe` records the edits, as `commit` does for git. The push
        // still snapshots the working copy, as every origin command does.
        self.scenario(
            "Push, 100 files edited",
            &["--warmup", "1"],
            (
                Some(format!(
                    "{} && {}",
                    self.edit(&seed),
                    self.origin_in(&seed, "describe bench")
                )),
                self.origin_in(&seed, "push"),
            ),
            (
                Some(format!(
                    "{} && {}",
                    self.edit(&self.git_dir),
                    self.git("git add -A && git commit -qm bench")
                )),
                self.git(&push),
            ),
        )?;
        self.scenario(
            "Pull, 100 files landed",
            &["--warmup", "1"],
            (
                Some(format!(
                    "{} && {}",
                    self.edit(&seed),
                    self.origin_in(&seed, "land")
                )),
                self.origin_in(&clone, "pull"),
            ),
            (
                Some(format!(
                    "{} && {}",
                    self.edit(&self.git_dir),
                    self.git(&format!("git add -A && git commit -qm bench && {push}"))
                )),
                Self::shell(&git_clone, "git pull -q"),
            ),
        )
    }
}

/// An `origin serve` hosting one empty repository, which the bench user,
/// signed in with [`Server::TOKEN`], writes to. Stopped when dropped.
struct Server {
    child: Child,
    url: String,
}

impl Server {
    /// The bench user's token.
    const TOKEN: &str = "bench-secret";

    /// Starts the server, and adds its token to the bench user's
    /// configuration in `config`.
    fn start(origin: &Path, base: &Path, config: &Path) -> Result<Self> {
        let dir = base.join("server");
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("origin-server.toml"),
            format!(
                "listen = \"127.0.0.1:0\"\nnode = \"hosted\"\n[repos]\ndemo = \"demo\"\n\
                 [tokens]\n\"user:bench\" = \"{}\"\n",
                Self::TOKEN
            ),
        )?;
        run(Command::new(origin)
            .args(["repo", "create", "demo"])
            .current_dir(&dir)
            .stdout(Stdio::null()))?;
        let mut child = Command::new(origin)
            .arg("serve")
            .current_dir(&dir)
            .env("ORIGIN_CONFIG_DIR", config)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take().context("the server has no stdout")?)
            .read_line(&mut line)?;
        let base = line
            .trim()
            .strip_prefix("Listening on ")
            .with_context(|| format!("unexpected server output: {line}"))?;
        let user = config.join("config.toml");
        let toml = fs::read_to_string(&user)?;
        fs::write(
            &user,
            format!("{toml}[tokens]\n\"{base}\" = \"{}\"\n", Self::TOKEN),
        )?;
        Ok(Self {
            url: format!("{base}/demo"),
            child,
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best effort: the benchmark is over either way.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(command: &mut Command) -> Result<()> {
    let status = match command.status() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("{} is not installed", command.get_program().display())
        }
        status => status.with_context(|| format!("cannot run {command:?}"))?,
    };
    ensure!(status.success(), "{command:?} failed with {status}");
    Ok(())
}
