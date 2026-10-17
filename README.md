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

origin workspace new [--change <change>] [<path>]  # another working copy, sharing the repository
origin workspace list | rm <path>

origin op log [-n <count>]          # what this node did, operation by operation, in every workspace
origin undo                         # move this workspace's working copy back one operation
origin watch [--kind <kind>] [--subject <id>] [--since <cursor>] [--local]  # events, as they happen

origin clone <url> [<path>] [--target <target>]  # follow a remote's target, starting a change on its tip
origin remote [<url>]               # show or set the remote: http://host:port/<repo>, or a path
origin push [<change>...]           # share changes and the ones below them; lands nothing
origin fetch [--target <t>] [--change <id>]  # new acceptances, and the records of known changes
origin pull                         # fetch, then move the working copy onto its target's tip
origin whoami | permissions         # who the remote thinks you are, and what you may do there
origin token issue --scope "write src" [--ttl 1h]  # a short-lived token for an agent
```

Landing rebases a change onto the target's tip, file by file, checks the
statements `policy.toml` requires (promoting results whose inputs didn't
change), and appends an acceptance signed by your node, which must be the
target's authority. A landing that conflicts or lacks evidence waits: issue
the statements, or a new revision, and run `origin land` again. Policy is a
target too: edit it with `origin new --on policy`, then `origin land`.

Workspaces work like git worktrees: each has its own current change and
shares the repository's records, so agents can work side by side. A new
workspace clones its files from the other working copies where the file
system can (Btrfs, XFS, APFS), and copies them otherwise. Landing moves
only the working copy of the workspace it runs in. A workspace whose change
landed elsewhere starts a new change on the target's tip at its next
command, keeping its edits.

Every node keeps an op log of what it did: records written, landings left
waiting and working copies moved, numbered in order. `origin undo` only
moves the working copy of the workspace it runs in back to where it was
before its latest operation; records and targets stay as they are, so an
undone landing still landed. `origin watch` prints events derived from the
op log, one a line starting with a cursor: `statement.issued`,
`revision.created`, `target.advanced` and `check.requested`. `--kind`
takes an event name or a statement kind (`core.conflict`, `ci.*`), and
`--since` resumes after a cursor.

A repository can have one remote, a server or another repository's path,
shared by its workspaces. A clone holds the chains of the targets it
follows, `policy` and one other, verified against their authorities' keys,
and the records of the changes it knows; it fetches snapshots only when a
command reads them, starting with its target's tip. `push` shares revisions,
their files and statements, and refuses a change the remote has a newer
revision of: `fetch`, then `converge` it. `land`, `land --from` and `target
new` go to the remote's queue for every target the remote has, after pushing
what lands, and `watch` long-polls the remote's events (`--local` for this
node's). Fetching never changes your files; `pull` then moves the working
copy onto its target's new tip, like a landing elsewhere does.

Changes are named by a unique prefix of their id, or `@` for the working
copy's. A revision is `@`, a change (for its head), or a unique prefix of
the revision's id. Your name and node key live in `~/.config/origin` (or
`$ORIGIN_CONFIG_DIR`): set `user = "<name>"` in its `config.toml` if `$USER`
isn't the name you want, and the token to send each server under
`[tokens]`, by its URL (`"http://127.0.0.1:7878" = "<token>"`), or in
`$ORIGIN_TOKEN`. See [the design](docs/design.md) for the model.

## Permissions

What each user may do lives in `policy.toml`, so changing it is a landing
on `policy`, which only `admin` may make:

```toml
[grants]                       # capabilities: annotate, land, admin
"group:maintainers" = ["admin", "land *", "annotate review"]
"group:runners"     = ["annotate ci.*"]

[[paths]]                      # path access: none, read or write
path   = "secrets"
grants = { all = "none", "group:maintainers" = "write" }
```

Under a path, the most specific rule that mentions you decides, and paths
no rule mentions you for are writable. Without `[grants]`, everyone lands
on every target but `policy` and issues every kind of statement. Content
you may not read never reaches your clone: those files are missing from
the working copy and stay as they were in your revisions, and fetching
restores them once you may read them. Revisions that only change what you
can't read, and the statements about them, are hidden from you. You may
only push revisions that change paths you write, and land what changes
only paths you write.

`origin token issue --scope <capability>... --ttl <period>` asks the remote
for a token acting for you with a narrower scope, such as `write src`,
`read /`, `annotate ci.*`, `land main` or `admin`; the server verifies it
without storing it. A server's operator signs one for any principal with
`origin token issue --config origin-server.toml --as user:agent-1 --scope
...`. A token never grants more than its principal's rights.

`converge` merges the files of a change's heads. A text file two heads
changed holds both versions between conflict markers (`<<<<<<<`, `=======`,
`>>>>>>>`), and the change doesn't land until you edit them out: the next
snapshot without them resolves it.

## Serving

A server is a node of its own, hosting repositories without a working
copy, and the authority of their targets: it runs their landing queues,
and packs their small objects once a few thousand pile up loose.

```sh
origin repo create repos/demo   # a repository the server orders `policy` and `main` of
origin serve                    # host the repositories over HTTP
```

Both read `origin-server.toml` (or `--config <file>`), whose paths are
relative to it:

```toml
listen = "127.0.0.1:7878"
node = "hosted"          # the server's node name
key = "node.key"         # its secret key, created if missing

[repos]
demo = "repos/demo"

[tokens]                 # static tokens, by principal
"user:alice" = "a long random secret"
```

The HTTP API, under `/api/v1`, puts records in batches and fetches them by
id, walks snapshots for the trees and blobs a client lacks (the only way
files leave a server), compares the records of changes, lists statements,
revisions, targets and their logs, lists the directories and reads the
files of a target's tip or any revision, without its history, issues
scoped tokens, lands changes and targets, and streams
events by long-poll: `GET
/api/v1/repos/<repo>/events?cursor=<n>&wait=30s` answers as soon as there
are events after the cursor, with a JSON batch and the next cursor.
[`docs/openapi.json`](docs/openapi.json) describes it, and
[`origin-client`](crates/origin-client) is its Rust SDK. Callers send
`Authorization: Bearer <token>`; without one, they are anonymous, and only
read what everyone may.

## Development

```sh
just             # format, lint, test, and audit dependencies
just bench       # micro-benchmarks
just bench-git   # origin against git, with hyperfine (docs/benchmarks.md)
just openapi     # rewrite docs/openapi.json after changing the API
just serve       # run `origin serve`
```

rustup installs the toolchain `rust-toolchain.toml` pins. The minimum
supported Rust version is the `rust-version` in `Cargo.toml`.
