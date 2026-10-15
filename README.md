# origin

A modern, git-compatible source control system, written in Rust.

**Status:** early prototype. Nothing here is stable.

## Usage

There are no commits or branches. The working copy is always a revision of
the current change, recorded automatically by every command that reads it.

```sh
origin init                         # a repository with the targets `policy` and `main`
origin describe "Add retries"       # say what the current change is for
origin new "Parse the config"       # finish it, and start the next change on top
origin status | diff | show [<change>]
origin log [<target>]               # your changes, then the target's acceptances
origin evolog <change>              # every revision of a change
origin edit <change>                # switch the working copy to another change
origin abandon --reason "Too slow"  # give a change up, keeping it for others to see
origin converge <change>            # end a change's divergence
origin index rebuild                # rebuild the record index from the store

origin annotate @ ci.test --pass --inputs src Cargo.toml   # a statement about a revision
origin statements [<rev>]           # the statements about a revision
origin promote [<rev>]              # carry results over from revisions with the same inputs
origin gc                           # drop expired payloads (policy.toml) and pack objects

origin land [<change>] [--onto <target>]   # land a change and the ones below it
origin land --from <target> --onto <target> # land a target's tip on another
origin land --status                # the landings waiting for evidence or a conflict fix
origin targets                      # every target, its tip, authority and requirements
origin target new <name> --from <target>
origin new [description] --on <target>     # start a change on a target's tip
```

Landing rebases a change onto the target's tip, file by file, checks the
statements `policy.toml` requires (promoting results whose inputs didn't
change), and appends an acceptance signed by your node, which must be the
target's authority. A landing that conflicts or lacks evidence waits: issue
the statements, or a new revision, and run `origin land` again. Policy is a
target too: edit it with `origin new --on policy`, then `origin land`.

Changes are named by a unique prefix of their id, or `@` for the working
copy's. A revision is `@`, a change (for its head), or a unique prefix of
the revision's id. Your name and node key live in `~/.config/origin` (or
`$ORIGIN_CONFIG_DIR`): set `user = "<name>"` in its `config.toml` if `$USER`
isn't the name you want. See [the design](docs/design.md) for the model.

`converge` merges the files of a change's heads. A text file two heads
changed holds both versions between conflict markers (`<<<<<<<`, `=======`,
`>>>>>>>`), and the change doesn't land until you edit them out: the next
snapshot without them resolves it.

## Development

```sh
just             # format, lint, test, and audit dependencies
just bench       # micro-benchmarks
just bench-git   # origin against git, with hyperfine (docs/benchmarks.md)
```

rustup installs the toolchain `rust-toolchain.toml` pins. The minimum
supported Rust version is the `rust-version` in `Cargo.toml`.
