//! Development tasks, run with `cargo xtask <task>`:
//!
//! - `bench-git [--files N] [--tree PATH]`: times origin against git on the
//!   same working tree with hyperfine, and writes `docs/benchmarks.md`. The
//!   tree is a synthetic fixture of N files, or a copy of PATH.
//! - `edit DIR COUNT`: appends a line to COUNT files spread across DIR. The
//!   benchmarks use it to change files between runs.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
        let origin = workspace.join("target/release/origin").canonicalize()?;
        let xtask = env::current_exe()?;

        let base = workspace.join("target/bench-git");
        if base.exists() {
            fs::remove_dir_all(&base)?;
        }
        let config = base.join("config");
        fs::create_dir_all(&config)?;
        fs::write(config.join("config.toml"), "user = \"bench\"\n")?;
        let (origin_dir, git_dir) = (base.join("origin"), base.join("git"));
        for dir in [&origin_dir, &git_dir] {
            self.populate(dir)?;
        }
        let (origin_dir, git_dir) = (origin_dir.canonicalize()?, git_dir.canonicalize()?);
        let shell = |dir: &Path, command: &str| format!("cd {} && {command}", dir.display());
        let origin_cmd =
            |command: &str| shell(&origin_dir, &format!("{} {command}", origin.display()));
        let git_cmd = |command: &str| shell(&git_dir, command);
        let edit_cmd = |dir: &Path| format!("{} edit {} 100", xtask.display(), dir.display());

        let mut report = self.report_header()?;
        let mut scenario = |title: &str,
                            runs: &[&str],
                            origin: (Option<String>, String),
                            git: (Option<String>, String)| {
            let markdown = base.join("scenario.md");
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
                .envs(Self::environment())
                .env("ORIGIN_CONFIG_DIR", &config))?;
            writeln!(report, "\n## {title}\n\n{}", fs::read_to_string(&markdown)?)?;
            anyhow::Ok(())
        };

        // Origin has no commit: `new` records the working copy into the
        // current change and starts the next one, the closest operation.
        scenario(
            "First commit of the whole tree (`origin new`)",
            &["--runs", "5"],
            (
                Some(shell(
                    &origin_dir,
                    &format!("rm -rf .origin && {} init", origin.display()),
                )),
                origin_cmd("new bench"),
            ),
            (
                Some(git_cmd("rm -rf .git && git init -q")),
                git_cmd("git add -A && git commit -qm bench"),
            ),
        )?;
        scenario(
            "Status, nothing changed",
            &["--warmup", "3"],
            (None, origin_cmd("status")),
            (None, git_cmd("git status --porcelain")),
        )?;
        scenario(
            "Status, 100 files edited",
            &["--warmup", "1"],
            (Some(edit_cmd(&origin_dir)), origin_cmd("status")),
            (Some(edit_cmd(&git_dir)), git_cmd("git status --porcelain")),
        )?;
        scenario(
            "Commit, 100 files edited (`origin new`)",
            &["--warmup", "1"],
            (Some(edit_cmd(&origin_dir)), origin_cmd("new bench")),
            (
                Some(edit_cmd(&git_dir)),
                git_cmd("git add -A && git commit -qm bench"),
            ),
        )?;

        let path = workspace.join("docs/benchmarks.md");
        fs::create_dir_all(path.parent().context("docs has a parent")?)?;
        fs::write(&path, report)?;
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
