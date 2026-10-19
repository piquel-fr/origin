# origin

Version control for repositories that many people, agents, CI runners and
review bots read and write at the same time, written in Rust. See
[`docs/design.md`](docs/design.md) for the model.

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
```

Changes are named by a unique prefix of their id, or `@` for the working
copy's. Your name and node key live in `~/.config/origin` (or
`$ORIGIN_CONFIG_DIR`): set `user = "<name>"` in its `config.toml` if `$USER`
isn't the name you want. See [the design](docs/design.md) for the model.

## Development

```sh
just             # format, lint, test, and audit dependencies
just bench       # micro-benchmarks
just bench-git   # origin against git, with hyperfine (docs/benchmarks.md)
```

rustup installs the toolchain `rust-toolchain.toml` pins. The minimum
supported Rust version is the `rust-version` in `Cargo.toml`.
