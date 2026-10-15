# Origin design

Origin is version control for repositories that many people, agents, CI
runners and review bots read and write at the same time. This document
describes the model the v2 prototype implements. Ideas planned for later are
listed at the end.

## Principles

- **Zero infrastructure for small projects.** One binary and a directory hold
  everything. A solo project never needs a server, an account or a database.
- **The same model at every scale.** A monorepo with a thousand agents adds
  machinery underneath, not different concepts.
- **Performance and reliability come first.** Every hot path has a
  benchmark, and every write survives a crash.
- **A small core of generic primitives.** Plain `origin` is the CLI and the
  server. Everything else (CI, review UIs, semantic diffs, integrations) is a
  plugin built on the same public API.
- **Order only what needs ordering.** The only things with an order are the
  landings on each target. Everything else is an unordered set of immutable
  records.

## Vocabulary

| Term | Meaning |
|---|---|
| **Snapshot** | An immutable tree of files, content-addressed with BLAKE3. |
| **Change** | One piece of work, such as "add retries to fetch". It keeps a stable id while it evolves. |
| **Revision** | One immutable version of a change: a snapshot, its base, its dependencies and a description. |
| **Statement** | An attributable claim about an immutable record: a CI result, a review, a conflict, an abandonment. Annotations are statements. |
| **Target** | A long-running, ordered line of history: `main`, `staging`, `release/v3`, a months-long overhaul, or `policy`. |
| **Acceptance** | One landing on a target: what landed, the resulting snapshot and the evidence relied on. |
| **Authority** | The single node that orders a target's acceptances. |
| **Node** | One running copy of a repository: a laptop, an agent's machine, a CI runner or a hosted server. |

Everyday work happens in changes, not branches. A stack is a chain of
dependencies between changes. Targets replace branches only where a
long-lived, ordered history is wanted.

## Records

The shared state of a repository is a set of four record kinds, stored in
the same content-addressed store as blobs and trees. Records are immutable.
Two nodes that hold the same record id hold the same bytes. Each kind has
its own encoding and hash domain, so a record of one kind can never be
mistaken for another.

```rust
/// One immutable version of a change.
struct Revision {
    change: ChangeId,               // random 128-bit id, stable for the change's life
    predecessors: Vec<RevisionId>,  // the revisions this one evolves from
    base: Base,                     // an acceptance on a target, or another revision
    deps: Vec<RevisionId>,          // exact revisions this one requires
    snapshot: TreeId,
    description: BlobId,            // the intent, written for humans and agents
    author: Principal,
}

/// An attributable claim about another record.
struct Statement {
    subject: RecordId,              // a revision, an acceptance, a snapshot or a statement
    kind: Kind,                     // namespaced: "ci.test", "review", "core.conflict", ...
    verdict: Option<Verdict>,       // Pass | Fail | Neutral: the only field core interprets
    issuer: Principal,
    inputs: Option<Inputs>,         // what a check depended on (see promotion)
    payload: Option<BlobId>,        // free-form, defined by whoever owns the kind
    supersedes: Option<StatementId>,// only a statement from the same issuer
    expires: Option<Timestamp>,
}

/// One landing on a target.
struct Acceptance {
    target: TargetId,
    seq: u64,
    prev: Option<AcceptanceId>,
    state: TreeId,                  // the target's snapshot after this landing
    landed: Vec<Source>,            // revisions, or another target's acceptance
    evidence: Vec<StatementId>,     // the statements the policy relied on
    policy: AcceptanceId,           // the policy in force when it landed
    signature: Signature,           // by the target's authority
}
```

Snapshots (blobs and trees) are the fourth kind and work as they do today.

**Why no global log.** A single ordered log of every operation needs one
sequencer that offline nodes cannot reach. It breaks partial replication,
because a filtered log no longer verifies. It also makes a leaked secret
permanent. Logs per actor fork whenever one key is used on two devices or a
crashed agent restarts. Unordered records avoid both problems. Writers never
contend, offline work is free, and a node can hold just the part of the
repository it needs.

**What we accept in exchange.** No node can prove it holds *every* statement
about a revision. This doesn't matter in practice. A check that matters
eventually runs, and its result then reaches the nodes that care. An
acceptance always lists the evidence it relied on, so the reason for every
landing can be verified.

## Changes

The working copy is always a revision of the current change. Origin
snapshots it automatically, so there is no `commit` and no staging area.
`origin describe` updates the intent, and `origin edit <change>` switches the
working copy to another change.

Editing, rebasing or handing a change to another agent creates a new
revision of the *same* change. Reviews, check results and history stay
attached to the change across all of them.

**Divergence.** Two agents that revise the same revision produce two heads.
This is a normal state, not an error. The index detects it and issues a
`core.divergent` statement, which fires an event. `origin converge <change>`
creates a revision with both heads as predecessors. The landing queue never
lands a divergent change. When a node is online, pushing a revision also
compares the change's head with the remote's, so an agent learns at once
that its work is out of date.

**Failed approaches** are kept, not deleted. `origin abandon --reason "..."`
issues a `core.abandon` statement. The revision stays available, so the next
agent can see what was tried and why it failed. Origin doesn't store agent
transcripts. The descriptions of revisions and abandonments hold the useful
part.

## Statements

Statements are the one mechanism behind every annotation. Core knows nothing
about kinds beyond a few reserved ones:

| Kind | Issued by | Meaning |
|---|---|---|
| `core.abandon` | the author | The change was given up; the payload says why. |
| `core.conflict` | the landing queue | A landing could not rebase; the payload lists the conflicting paths. |
| `core.divergent` | the index | The change has more than one head. |
| `core.redact` | the policy authority | A blob must be withheld everywhere (a leaked secret). |

Every other kind (`ci.test`, `review`, `semantic.diff`, `flaky`) belongs to
whoever issues it. Statements compose: a human can mark a failed `ci.test`
statement as `flaky`, because a statement's subject can be another
statement.

Rules:

- **Supersession.** An issuer may only supersede its own statements.
  Statements from different issuers never overwrite each other. A review
  from Alice and one from Bob both stand.
- **Expiry.** A statement can carry an expiry time, after which it is
  ignored and can be garbage-collected.
- **Every statement fires an event** with its subject, kind, verdict and
  issuer (see [Events](#events)).

### Promotion by input digest

A statement is always pinned to an exact record. Carrying a check result over
to a new revision never mutates the result. Instead, Origin issues a new
statement that points at the original.

A check declares what it depends on:

```rust
struct Inputs {
    paths: Vec<Glob>,      // e.g. ["src/**", "Cargo.toml", "Cargo.lock"]
    definition: Digest,    // the check's command and configuration
    toolchain: Digest,     // e.g. the hash of `rustc -V` output
}
```

The input digest is the hash of the matching subtrees of the snapshot, plus
`definition` and `toolchain`. When a new revision's input digest equals the
digest of an existing result, the result is promoted automatically.

So a docs-only change keeps its `lint` and `build` results, because neither
reads `docs/**`. `docs-test` runs again, because it does read `docs/**`. Nobody
writes rules about which changes are "docs-only".

Promotion is only as safe as the declared inputs. A check that might read
files it can't list (the network, the environment) declares no inputs, and
then it always runs again. Human reviews don't declare inputs, so they never promote
automatically.

## Targets and policy

A target is a hash-chained list of acceptances with one authority. Its tip
is the target's current snapshot. Only the authority writes to the chain,
and every acceptance names the one before it. Two acceptances with the same
predecessor are an error, never something to merge.

**Policy is a target too.** `policy` holds `policy.toml`, and changing
permissions or land rules is a landing on `policy`. Policy edits are
therefore ordered, reviewed and audited like code, with no extra mechanism.
An agent can't grant itself rights, because landing on `policy` needs the
`admin` capability.

```toml
# policy.toml, in the snapshot of the `policy` target

[groups]
maintainers = ["alice"]
runners     = ["ci-1", "ci-2"]

[targets.main]
authority = "node:hosted"
require = [
  { kind = "ci.test", verdict = "pass", issuer = "group:runners" },
  { kind = "review",  verdict = "pass", issuer = "group:maintainers" },
]

[targets."release/v3"]
authority = "node:hosted"
require = [{ kind = "ci.test", verdict = "pass", issuer = "group:runners" }]

[[paths]]
path   = "secrets"
grants = { all = "none", "group:maintainers" = "write" }
```

`origin init` creates `policy` and `main`. Your node is the authority, and
neither target has requirements, so a solo project lands instantly.

Common workflows:

- **Long-running work.** `origin target new overhaul --from main` creates a
  target. Changes land on `overhaul`. Catching up with `main` is an
  acceptance on `overhaul` that lands `main`'s latest acceptance. The
  overhaul finishes with one acceptance on `main` that lands `overhaul`.
- **Patching old releases.** A fix is one change. Landing it on `main` and
  then with `origin land <change> --onto release/v3` creates a rebased
  revision of the same change. "Which releases contain this fix?" becomes
  a simple query.

## The landing queue

Writers never move a target. They submit changes, and the target's
authority lands them in order:

1. **Rebase** the submitted revisions onto the target's tip. In v2 this
   is a three-way merge at file level: if both sides changed the same file,
   it's a conflict. A rebase produces a new revision of the same change,
   attributed to the queue.
2. **On conflict,** issue a `core.conflict` statement on the revision. The
   change waits, blocked, and nothing fails. Its author, or any agent
   subscribed to conflicts, resolves it by pushing a new revision.
3. **Check the policy** in force *now*: required statements (promoting
   results where the input digest matches), trusted issuers and the
   submitter's permissions.
4. **If evidence is missing,** emit a `check.requested` event and wait.
   Plugins run the checks and post statements, and the queue continues
   when they arrive.
5. **Otherwise,** write a signed acceptance listing what landed and the
   evidence used, and advance the target.

For a small project the queue runs inside your own `origin` process. Later,
batching (one acceptance landing several changes) and speculative merge
trains raise a busy target's throughput without changing the model.

## Events

Every node keeps a local **operation log** of what it applied and in what
order: records written, targets advanced, working-copy snapshots. It is
never synced, and it serves three purposes:

- `origin undo` and `origin op log`, as in jj.
- Crash recovery: on start-up, a node finishes or discards incomplete
  operations.
- The event stream.

Every record write and target move emits an event. Events include
`statement.issued` (for every statement, including conflicts and
divergence), `revision.created`, `target.advanced` and `check.requested`.
Events are delivered at least once and carry the record id, so consumers
deduplicate by it.

- **Locally,** `origin watch [--kind ...] [--subject ...]` follows the log.
- **In server mode,** the server dispatches each event to the users
  subscribed to it, over a resumable server-sent-events stream
  (`GET /events?cursor=...`), filtered by what each user may read.

A cursor belongs to one node. Two nodes may deliver the same events in a
different order.

## Plugins

Plugins only use the public `OriginService` API, over HTTP or in-process. A
plugin can do nothing the API can't, which keeps the API complete.

- **Workers** are long-running processes with a scoped credential. They
  subscribe to events and issue statements. Examples: a CI runner, a review
  bot, a semantic differ.
- **Tools** are executables named `origin-<name>` on `PATH`. `origin <name>`
  runs them, as cargo and git do with their subcommands.

Official plugins live in `apps/origin-<name>`. The first is `origin-ci`. As a
tool it runs checks locally (`origin ci run`). As a worker it answers
`check.requested`. It reads the checks from `checks.toml` in the revision
being checked:

```toml
[checks.lint]
command   = "cargo clippy --workspace -- -D warnings"
inputs    = ["src/**", "Cargo.toml", "Cargo.lock"]
toolchain = "rustc -V"

[checks.docs-test]
command   = "cargo test --doc"
inputs    = ["src/**", "docs/**"]
toolchain = "rustc -V"
```

## Permissions and credentials

Path rules work as in v1. Each rule maps a path to `none`, `read` or `write`
for everyone, groups and users, and the most specific rule wins. Unreadable
files keep their place in trees, but their content is never sent. v2 moves
the rules into `policy.toml` and splits access into capabilities:

| Capability | Scope | Allows |
|---|---|---|
| `read` | path | reading content |
| `write` | path | proposing revisions that change the path |
| `annotate` | statement kind | issuing statements of that kind |
| `land` | target | submitting to the target's queue |
| `admin` | — | landing on `policy` |

Static tokens in the server configuration keep working.
`origin token issue --scope ... --ttl ...` issues short-lived tokens with
narrower scopes for agents and CI runners. A typical agent token carries
`write src/**` and nothing else.

## Signing

Each node has an ed25519 key, kept outside the repository in
`~/.config/origin/`. Authorities sign the acceptances they write, so any
node can verify a target's history. Other records are attributed by the
node that receives them, which has already authenticated the sender.
Signed statements from outside issuers, and identities separate from their
keys, come later.

## Sync

- **`clone`** fetches the chains of the targets you follow and then the tip
  snapshot, lazily, as today.
- **`push`** shares revisions and statements. It never lands anything:
  sharing work and accepting it are separate steps.
- **`fetch`** and **`pull`** fetch each target chain after the last
  acceptance you hold, then compare record inventories per change and fetch
  what's missing.
- **`land`** against a remote submits to that remote authority's queue.
- **`watch`** against a remote subscribes to its event stream.

Syncing never changes files under a running workspace.

## Workspaces and platform code

`origin workspace new [--change X] [path]` creates a workspace that shares
the repository's store. Each workspace has its own current change and stat
cache. Files are cloned copy-on-write where the filesystem supports it
(Btrfs or XFS on Linux, APFS on macOS), and copied otherwise. Missing blobs
are fetched as the workspace is checked out.

Code that differs between platforms lives behind traits in
`crates/origin-platform`:

| Trait | Linux | macOS |
|---|---|---|
| `Durability` | `fsync` of files and directories | `F_FULLFSYNC` |
| `FileLock` | `flock` | `flock` |
| `CloneFile` | `FICLONE` | `clonefile` |
| `Watch` | inotify | FSEvents |

Each platform has its own module. A single type alias picks one according to
the build target. That alias is the only conditional compilation in the
codebase. One conformance test suite runs against whichever implementation
is built. Windows is not supported.

## On disk

A repository is a directory:

```
.origin/
  format                          # format version and repository id
  objects/loose/ab/<hash>         # blobs, trees and records, recently written
  objects/packs/<id>.{pack,idx}   # compacted in the background
  targets/<blake3(name)>/log      # the target's acceptance ids, in order
  targets/<blake3(name)>/head     # the latest sequence number and acceptance
  index/                          # derived: change heads, statements by subject
  oplog/                          # this node only, size-capped
  tmp/
```

- **Durability order.** Write, fsync, rename and fsync the directory, and
  only then publish anything that refers to the new file. Writes are
  group-committed, so a burst of records costs one fsync.
- **Locking.** Records are immutable, so writing one needs no lock. Only a
  target's chain is locked, with `flock`, which the kernel releases if the
  process dies.
- **Packs.** Statements are the bulk of the data, so small objects are packed
  in the background by a single compactor.
- **Names.** User-chosen names (targets, changes) never become file names.
  They are hashed, which avoids case-folding and Unicode normalization
  problems on macOS.
- **The index is derived.** `origin index rebuild` reconstructs it from the
  store, so a corrupt index never loses data.
- **Local filesystems only.** Writers refuse to run on NFS, SMB, Dropbox or
  iCloud. A shared folder is a remote, never a live repository.

## Command line

```
origin init                         # a repository with `policy` and `main`
origin new "Add retries to fetch"   # start a change
origin describe "..."               # refine its intent
origin edit <change>                # switch the working copy
origin status | diff | log | evolog <change>
origin converge <change>            # resolve divergence
origin abandon --reason "..."       # record a failed approach
origin annotate <rev> <kind> --pass|--fail|--neutral [--inputs ...] [--payload f]
origin statements <rev>
origin target new <name> --from <target>
origin land [<change>] [--onto <target>]
origin push | fetch | pull | clone <url>
origin watch [--kind ...] [--subject ...]
origin workspace new | list | rm
origin token issue --scope ... --ttl ...
origin undo | op log
origin serve
```

## Testing and benchmarks

Unit, integration and end-to-end tests keep running against both the
in-process and the HTTP `OriginService`. Benchmarks cover:

- landing throughput with N concurrent proposers;
- workspace creation on a 20,000-file tree;
- statement writes and reads, with and without packs;
- incremental sync;
- `status` and snapshots compared with git, as today.

The **swarm scenario** is a skill run from T3 Code. Several agents work in
parallel workspaces. One changes a shared dependency, so a landing
conflicts. Stale results are rerun, unchanged ones are promoted, and the
work lands in a single acceptance. Processes are killed with `kill -9`
along the way. The skill checks that target chains stay consistent, that no
revision is lost, that results were rerun or promoted correctly and that
the work landed in one acceptance. It also reports timings.

## Later

- A virtual filesystem for workspaces: FUSE on Linux first, then FSKit on
  macOS. It goes behind a `Vfs` trait in `origin-platform`.
- Webhooks, as a plugin.
- Line-level and structure-aware merging.
- Set reconciliation (Negentropy-style) for sync, and batched merge trains.
- Remotes on plain storage, such as S3 with conditional writes.
- `origin-review` (a UI) and `origin-semantic` (tree-sitter diffs).
- Signed statements from outside issuers, and identities separate from keys.
- A git bridge.
