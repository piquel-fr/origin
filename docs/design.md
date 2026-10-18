# Origin design

Origin is version control for repositories that many people, agents, CI
runners and review bots read and write at the same time. This document
describes the model the v2 prototype implements. Ideas planned for later are
listed at the end.

## Principles

- **Zero infrastructure for small projects.** A solo project needs the
  `origin` binary and a directory, nothing more: no server, no account, no
  database. The repository lives in `.origin/`. The user's identity and node
  key live in the user's config directory.
- **The same model at every scale.** A monorepo with a thousand agents adds
  machinery underneath, not different concepts.
- **Performance and reliability come first.** Every hot path has a
  benchmark, and every write survives a crash.
- **A small core of generic primitives.** Plain `origin` is the CLI and the
  server. Everything else (CI, review UIs, semantic diffs, integrations) is a
  plugin built on the same public API. The first plugin is `origin-ci`.
- **Order only what needs ordering.** The only things with an order are the
  landings on each target. Everything else is an unordered set of immutable
  records.

## Vocabulary

| Term | Meaning |
|---|---|
| **Snapshot** | An immutable tree of files, content-addressed with BLAKE3. |
| **Change** | One piece of work, such as "add retries to fetch". It keeps a stable id while it evolves. |
| **Revision** | One immutable version of a change: a snapshot, the base it was made on and a description. |
| **Statement** | An attributable claim about an immutable record: a CI result, a review, a conflict, an abandonment. Annotations are statements. |
| **Attachment** | The bytes of a revision's description or a statement's payload. It has its own hash domain, so it can never be confused with a file. |
| **Target** | A long-running, ordered line of history: `main`, `staging`, `release/v3`, a months-long overhaul, or `policy`. |
| **Acceptance** | One landing on a target: what landed, the resulting snapshot and the evidence relied on, signed by the target's authority. |
| **Authority** | The single node that orders a target's acceptances. |
| **Node** | One running copy of a repository with its own key: a laptop, an agent's machine, a CI runner or a hosted server. |
| **Principal** | Who acts: `user:<name>` or `node:<name>`. Policies also name `group:<name>`. |
| **Workspace** | A working copy. Several workspaces can share one repository's store. |

Everyday work happens in changes, not branches. A stack is a chain of changes,
each based on the one below. Targets replace branches only where a long-lived,
ordered history is wanted.

## Records

The shared state of a repository is a set of immutable, content-addressed
objects. Each encoding starts with a kind byte, and an object's id is the
BLAKE3 hash of the whole encoding, so two kinds can never share an id.
Encodings are canonical: little-endian integers, length-prefixed strings and
lists, and no trailing bytes.

| Kind | Holds |
|---|---|
| Blob | A file's content. |
| Tree | A directory: names, entry kinds and ids. |
| Revision | One version of a change. |
| Statement | A claim about another record. |
| Acceptance | One landing on a target. |
| Attachment | A description or a payload. |

```rust
/// One immutable version of a change.
struct Revision {
    change: ChangeId,               // random 128-bit id, stable for the change's life
    predecessors: Vec<RevisionId>,  // the revisions this one evolves from
    base: Base,                     // an acceptance on a target, or another revision
    deps: Vec<RevisionId>,          // reserved; stacks use `base` today
    snapshot: TreeId,
    description: AttachmentId,      // the intent, written for humans and agents
    author: Principal,
}

/// An attributable claim about another record.
struct Statement {
    subject: RecordId,              // a revision, an acceptance, a snapshot or a statement
    kind: Kind,                     // dot-separated: "ci.test", "review", "core.conflict", ...
    verdict: Option<Verdict>,       // Pass | Fail | Neutral: the only field core interprets
    issuer: Principal,
    inputs: Option<Inputs>,         // what the claim depended on (see promotion)
    payload: Option<AttachmentId>,  // free-form, defined by whoever owns the kind
    supersedes: Option<StatementId>,// only a statement from the same issuer
    promoted_from: Option<StatementId>, // set on a promotion: the original result
}

/// One landing on a target. The authority's ed25519 signature over the
/// encoding follows the body.
struct AcceptanceBody {
    target: TargetId,
    seq: u64,                       // 0 for a target's first acceptance
    prev: Option<AcceptanceId>,     // None exactly when seq is 0
    state: TreeId,                  // the target's snapshot after this landing
    landed: Vec<Source>,            // a revision, or another target's acceptance
    evidence: Vec<StatementId>,     // the statements the policy relied on
    policy: Option<AcceptanceId>,   // the policy in force; None only for policy's first
}
```

**Why no global log.** A single ordered log of every operation needs one
sequencer, which offline nodes can't reach. It also breaks partial
replication, because a filtered log no longer verifies. Logs per actor fork
whenever one key is used on two devices or a crashed agent restarts.
Unordered records avoid both problems. Writers never contend, offline work is
free, and a node can hold just the part of the repository it needs.

**What we accept in exchange.** No node can prove it holds *every* statement
about a revision. This doesn't matter in practice. A check that matters
eventually runs, and its result then reaches the nodes that care. An
acceptance always lists the evidence it relied on, so the reason for every
landing can be verified.

## Changes

The working copy is always a revision of the current change. Every command
snapshots it first, so there is no `commit` and no staging area. `.origin` and
`.git` are reserved names, never stored.

- **Starting and editing.** `origin new` starts a change, on the working copy
  or on a target with `--on`. `origin describe` updates the intent, and
  `origin edit <change>` moves the working copy to another change.
- **Revising.** Editing, rebasing or handing a change to another agent
  creates a new revision of the *same* change. Reviews, check results and
  history stay attached to the change across all of them.
- **Divergence.** Two revisions made from the same revision give a change two
  heads. This is a normal state, not an error. The node that notices it
  (locally after a save, the server on a push, a client on a fetch) issues a
  `core.divergent` statement, which fires an event. `origin converge <change>`
  merges every head into one revision. The landing queue never lands a
  divergent change.
- **Pushing.** When a node is online, a push first compares the change's
  heads with the remote's and refuses a stale change before uploading
  anything. The upload itself carries the heads it expects, and the server
  checks them atomically as it publishes, so two pushes can't fork a change.
- **Failed approaches** are kept, not deleted. `origin abandon --reason "..."`
  issues a `core.abandon` statement. The revision stays available, so the next
  agent can see what was tried and why it failed. Origin doesn't store agent
  transcripts. The descriptions of revisions and abandonments hold the useful
  part.

### Conflicts

When a merge (`converge`, `pull`, or a rebase in the landing queue) leaves
both sides' changes to the same file, the file gets conflict markers. The new
revision gets a `core.conflict` statement listing each conflicted path and
the exact content written there.

- A path is resolved once a later revision has different content there, or
  when the user runs `origin resolve <path>...` to accept it as it is.
- Every operation that makes a revision carries unresolved paths forward in
  its own `core.conflict`: snapshots, `describe`, `new`, `converge`, `pull`,
  `undo` and `workspace new`.
- The statement is written before the revision, so nothing can find the
  revision without its conflicts, even after a crash.
- The landing queue refuses a revision with unresolved paths.
- Markers are only an editing aid. Origin never scans files for them.

### Behind

A change can move on without the working copy. Its head may land from
another workspace, or the queue may rebase it. The working copy is then
*behind*:

- `status` and `log` say so, and suggest `origin pull`.
- Commands that build on the working copy refuse until then.
- Edits made meanwhile aren't recorded, so the change doesn't fork. The files
  are never touched.

`origin pull` is the explicit catch-up. It fetches first when there is a
remote, then moves the working copy:
- onto the landed tip, if the change landed;
- onto the queue's rebase, if the queue rebased it;
- onto the tip of the target below it, if what it was stacked on landed.

In every case it merges the edits made meanwhile, with conflict markers where
they collide. No other command checks files out on its own.

## Statements

Statements are the one mechanism behind every annotation. Core knows nothing
about kinds beyond the reserved `core.*` ones, which each have authors:

| Kind | Who may issue it | Meaning |
|---|---|---|
| `core.abandon` | the change's creator, or an admin | The change was given up; the payload says why. |
| `core.conflict` | the revision's author, or a node | Paths left conflicted, with the content written there. From a node, it blocks only that landing. |
| `core.divergent` | a node | The change has more than one head; the payload lists them. |

A change's creator is the author of its first revisions. Reserved kinds can't
be promoted, and `origin annotate` refuses them.

Every other kind (`ci.test`, `review`, `semantic.diff`, `flaky`) belongs to
whoever issues it. Statements compose: a human can mark a failed `ci.test`
statement as `flaky`, because a statement's subject can be another
statement.

Rules:

- **Supersession.** An issuer may only supersede their own statements. A
  claim to supersede someone else's is ignored. Statements from different
  issuers never overwrite each other, so a review from Alice and one from Bob
  both stand. `origin annotate` supersedes the user's latest statement of the
  same kind about that revision.
- **Statements are permanent.** Reviews, check results and conflicts are
  history, and history is never deleted. Only bulky payloads, such as CI
  logs, can have a retention period, set per kind in the policy
  (`[retention] "ci.*" = "90d"`).
  - Age counts from when this node stored the statement.
  - `origin gc` drops expired payloads and keeps the statements: subject,
    kind, verdict, issuer and inputs.
  - Payloads of `core.*` statements are never dropped.
- **Every statement fires an event** with its subject, kind, verdict and
  issuer (see [Events](#events)).

### Promotion by input digest

A statement is always pinned to an exact record. Carrying a check result over
to a new revision never mutates the result. Instead, a node issues a new
statement with `promoted_from` pointing at the original.

A statement can declare what it depended on:

```rust
struct Inputs {
    paths: Vec<RepoPath>,  // e.g. ["src", "Cargo.toml", "Cargo.lock"]; "" is the whole snapshot
    definition: Digest,    // the check's command and configuration
    toolchain: Digest,     // e.g. the hash of `rustc -V` output
    external: bool,        // also depended on things outside the snapshot
}
```

Each path names a file or a directory and covers everything under it. The
input digest hashes what the snapshot stores at each path (absent, or the
entry's kind and id), plus `definition` and `toolchain`. Trees are already
content-addressed, so the digest costs one lookup per path, however large the
tree.

A statement is **promotable** when it declares inputs, isn't `external`,
isn't itself a promotion, isn't about another statement, and its kind isn't
reserved. Promotion happens on demand, never on every snapshot:
- `origin promote`;
- the landing queue, before it checks evidence;
- `POST /promote`;
- `origin-ci`, before it runs a check.

It copies the latest matching result from each issuer, and only results the
caller can see.

A promotion counts as its original: the original's kind, verdict and issuer
are what policy checks. It is only valid if:
- its kind, verdict, inputs and payload equal the original's;
- its subject's input digest equals the original subject's.

Nodes check this whenever a promotion is used or put.

So a docs-only change keeps its `lint` and `build` results, because neither
reads `docs`. `docs-test` runs again, because it does read `docs`. Nobody
writes rules about which changes are "docs-only".

Promotion is only as safe as the declared inputs. A check that also reads the
network or the environment still declares its paths, which remain useful
information, but sets `external`. It then runs again on every revision. Human
reviews usually declare no inputs, so they never promote automatically.

## Targets and policy

A target is a hash-chained list of acceptances with one authority. Its tip is
the target's current snapshot. Only the authority writes to the chain, and
every acceptance names the one before it. Two acceptances with the same
predecessor are an error, never something to merge: appends are
compare-and-swap.

**Policy is a target too.** `policy` holds `policy.toml`, and changing
permissions or land rules is a landing on `policy`:
- Policy edits are therefore ordered, reviewed and audited like code, with no
  extra mechanism.
- Landing on `policy` needs the `admin` capability, so an agent can't grant
  itself rights.
- A landing on `policy` is judged by the policy *before* it, never by the one
  it brings. That way a revoked key can't restore itself.
- Every other acceptance names the policy acceptance in force when it landed.

```toml
# policy.toml, in the snapshot of the `policy` target

[nodes]
hosted = "<hex ed25519 public key>"

[groups]
maintainers = ["user:alice"]
runners     = ["node:ci-1", "node:ci-2"]

[targets.policy]
authority = "node:hosted"

[targets.main]
authority = "node:hosted"
require = [
  { kind = "ci.test", verdict = "pass", issuer = "group:runners" },
  { kind = "review",  verdict = "pass", issuer = "group:maintainers" },
]

[targets."release/v3"]
authority = "node:hosted"
require = [{ kind = "ci.test", verdict = "pass", issuer = "group:runners" }]

[retention]
"ci.*" = "90d"

[grants]
all                 = ["land *", "annotate *"]
"group:maintainers" = ["admin"]
"group:runners"     = ["annotate ci.*"]

[[paths]]
path   = "secrets"
grants = { all = "none", "group:maintainers" = "write" }

[[paths]]
path   = "checks.toml"
grants = { all = "read", "group:maintainers" = "write" }
```

The rules for this file:
- Every table is optional, and unknown keys are errors.
- An authority must be a node listed in `[nodes]` with its public key.
- A target without its own table uses `policy`'s authority and has no
  requirements.
- Without `[grants]`, everyone may land on every target except `policy` and
  issue any kind. Only the local owner is an admin.

`origin init` creates `policy` and `main`. Your node is the authority, and
neither target has requirements, so a solo project lands instantly.
`origin repo create --admin <principal>` does the same on a server and names
a remote admin.

Common workflows:

- **Long-running work.** `origin target new overhaul --from main` creates a
  target, and changes land on `overhaul`. Catching up with `main` is
  `origin land --from main --onto overhaul`. The overhaul finishes with
  `origin land --from overhaul --onto main`.
- **Patching old releases.** A fix is one change.
  `origin land <change> --onto release/v3` rebases it into a new revision of
  the same change, so its history shows every target it reached.

## The landing queue

Writers never move a target. They submit changes, and the target's authority
lands them in order. A stack lands bottom first, one acceptance per change,
and stops at the first one that waits.

1. **Refuse** what can never land as it is: divergent changes, abandoned
   changes, and revisions with unresolved conflicts.
2. **Rebase** the revision onto the target's tip with a three-way merge at
   path level, recursing into directories. A revision already based on the
   tip lands as it is. Any other becomes a new revision of the same change,
   authored by the queue's node, written only if the change's head hasn't
   moved meanwhile.
   - When one target lands on another, the merge base is found from both
     histories: the best common ancestors, merged recursively when there are
     several, as git's recursive strategy does.
   - A landing that changes nothing is skipped.
3. **On conflict,** issue a `core.conflict` statement and keep the landing
   blocked. Nothing fails, and landing again retries it.
4. **Check the policy** in force *now*:
   - this node must be the target's authority;
   - the submitter must be allowed to land there, and must be able to write
     every path the landing changes;
   - the required statements must be present, promoted where the input
     digest matches.
5. **If evidence is missing,** keep the landing waiting, durably, and request
   the checks (`check.requested` events). It resumes by itself when
   statements about its subject arrive, and when the server starts. A landing
   whose submitter's token has expired waits until it's landed again.
6. **Otherwise,** write the signed acceptance, listing what landed and the
   evidence used, and advance the target.

After a landing, the working copy that submitted it follows: a new change on
the tip if its change landed, or the queue's rebase if it was rebased.

For a small project the queue runs inside your own `origin` process. Later,
batching (one acceptance landing several changes) and speculative merge trains
raise a busy target's throughput without changing the model.

## Events

Every node keeps a local **operation log** of what it applied and in what
order: records written, landings left waiting, working-copy moves, target
advances. It is never synced, and it serves three purposes:

- `origin undo` and `origin op log`. Undo only moves this workspace's working
  copy back; records and landings stay.
- Crash recovery, together with the `moving` marker (see
  [On disk](#on-disk)).
- The event stream.

Events are derived from the log when they're read:
- `statement.issued` fires for every statement this node stores, fetched and
  promoted ones included.
- `revision.created` fires for every revision.
- `target.advanced` fires when a target's head moves.
- `check.requested` fires once per missing requirement of a waiting landing.

Events are delivered at least once, and consumers deduplicate by record id.

- **Locally,** `origin watch [--kind ...] [--subject ...] [--since ...]` polls
  the op log for new entries. With a remote it follows the remote instead,
  unless given `--local`.
- **In server mode,** the server dispatches each event to the users
  subscribed to it, filtered by what each user may read. Subscribers
  long-poll: `GET /api/v1/repos/<repo>/events?cursor=...&wait=30s` answers
  as soon as there are events after the cursor, with a JSON batch and the
  next cursor.
  - Without a cursor, only new events come. `wait` is capped at 60 s.
  - A cursor the log no longer holds gets 410, and the client starts over.
  - So does a cursor from another op log (it carries a random generation),
    for example after the remote was replaced.

A cursor belongs to one node. Two nodes may deliver the same events in a
different order.

## Plugins

Plugins only use the public `OriginService` API, over HTTP or in-process. A
plugin can do nothing the API can't, which keeps the API complete.

- **Workers** are long-running processes with a scoped credential. They
  subscribe to events and issue statements. Examples: a CI runner, a review
  bot, a semantic differ.
- **Tools** are executables named `origin-<name>` on `PATH`. `origin <name>`
  replaces itself with them, as cargo and git do with their subcommands. Run
  in a working copy, it snapshots first and sets `ORIGIN_REPOSITORY`,
  `ORIGIN_REVISION` and `ORIGIN_REMOTE`.

Official plugins live in `apps/origin-<name>`. The first is `origin-ci`:
- **Tool mode:** `origin ci run [<rev>]` checks a revision as the user.
- **Worker mode:** `origin ci worker --remote <url>` answers `check.requested`
  with a token that can read the repository and annotate `ci.*`. It
  reconciles the waiting landings at start, every minute and after an
  outage, and outlives server restarts.

Each check runs in a fresh checkout and its own process group. Before running
a check, the plugin asks for a promotion and skips the check if a result
already covers the revision. Results carry the last 64 KiB of output as their
payload.

`origin-ci` reads the checks from `checks.toml`. A worker takes them from the
tip of the target being landed on, never from the candidate. The checks that
guard `main` are the ones already on `main`, so a submitter can't redefine a
required check by editing it in their change. An edit to `checks.toml` takes
effect once it has landed, after passing the checks in force. `origin ci run`
checks a revision with that revision's own `checks.toml`, since it issues
results as the user.

```toml
[checks.lint]
command   = "cargo clippy --workspace -- -D warnings"
inputs    = ["src", "Cargo.toml", "Cargo.lock"]
toolchain = "rustc -V"

[checks.docs-test]
command   = "cargo test --doc"
inputs    = ["src", "docs"]
toolchain = "rustc -V"

[checks.links]
command  = "lychee docs"
inputs   = ["docs"]
external = true          # reads the network: never promoted
```

- Each check's kind is `ci.<name>`.
- `checks.toml` itself is always one of a check's inputs, so editing a check
  runs it again.
- Protecting `checks.toml` with a `[[paths]]` rule, as in the example policy,
  keeps submitters from changing the checks at all.
- A change in a toolchain's output only reaches the queue's promotions through
  the inputs. Pin toolchains in an input file, such as `rust-toolchain.toml`.

## Permissions and credentials

Path rules map a path to `none`, `read` or `write` for everyone, groups and
users. The most specific rule wins. Paths no rule mentions are writable by
signed-in callers and readable by anonymous ones.

Access is split into capabilities:

| Capability | Scope | Allows |
|---|---|---|
| `read` | path | reading content |
| `write` | path | proposing revisions that change the path |
| `annotate` | statement kind | issuing statements of that kind |
| `land` | target | submitting to the target's queue (`land *` excludes `policy`) |
| `admin` | — | landing on `policy` |

Reading:
- A caller who can read no path doesn't see the repository at all.
- A revision is visible when it changes a path the caller reads.
- Unreadable files keep their place in trees, but their content is never
  sent. A clone records the user's rights and leaves withheld paths out of the
  working copy.

Writing:
- Writes are attributed: a revision's author and a statement's issuer must be
  the caller. Anonymous callers never write.
- A revision's *effect* on a path is the pair (what its base holds there,
  what its snapshot holds there). On every path its writer can't write:
  - a first revision may have no effect;
  - a later revision must have exactly the effect of each of its
    predecessors;
  - a stacked revision must sit on the same change below as its
    predecessors;
  - unresolved conflicts must be carried forward exactly, none dropped and
    none added; a first revision lists only those of the revision it is on.

  So nobody can empty, revert or redirect another person's work without
  write access to it. Rebasing onto a newer tip still works, and so does
  handing a change to an agent with other rights, as long as they leave alone
  what they can't write. Heads that disagree on such paths are converged by
  someone who can write them.
- A change's history is fixed: a new first revision for a change that already
  exists is refused, and so is a predecessor from another change. Only the
  creator, who wrote the first revision, or an admin can abandon the change.
  An agent who takes a change over can't abandon it.
- A statement's subject must be visible to its issuer, and so must a
  promotion's original.

Tokens:
- Static tokens in the server configuration keep working.
- `origin token issue --scope ... --ttl ...` issues short-lived tokens with
  narrower scopes for agents and CI runners.
  - They are signed by the server's node key and verified without storage.
  - A scope only narrows its holder's current rights.
  - A token lives at most 7 days, and never longer than the token that issued
    it.
- A typical agent token carries `read /` and `write src` and nothing else.

Origin has no way to erase content from history. A secret that leaks into a
revision is rotated. Once anyone has fetched it, deleting it would change
nothing.

## Signing

Each node has an ed25519 key, kept outside the repository in
`~/.config/origin/node.key` (or `$ORIGIN_CONFIG_DIR`). Its public key is
published in the policy's `[nodes]`.

- Authorities sign the acceptances they write. A fetch verifies every
  acceptance against the key named by the policy that governs it, and only a
  policy on the verified `policy` chain counts.
- The node key also signs scoped tokens.
- Other records are attributed by the node that receives them, which has
  already authenticated the sender.

Signed statements from outside issuers, and identities separate from their
keys, come later.

## Sync

- **`clone`** fetches the chains of `policy` and one target (`main` by
  default), then that tip's snapshot. Other snapshots are fetched lazily, when
  a command needs them.
- **`push`** shares revisions and statements. It never lands anything:
  sharing work and accepting it are separate steps.
  - It refuses a stale change, and compares the expected heads atomically.
  - It sends a statement together with the statements it depends on.
  - It splits large transfers into parts of at most 64 MiB.
- **`fetch`** fetches each held target's chain after the last acceptance this
  node has, and verifies it. It then fetches the changes the remote's events
  say moved since the last fetch, and falls back to comparing every known
  change when it has no valid cursor.
- **`pull`** fetches, then catches the working copy up (see
  [Behind](#behind)).
- **`land`** against a remote pushes the stack, submits it to that remote
  authority's queue, then fetches the result.
- **`watch`** against a remote subscribes to its event stream.

Blobs and trees only ever leave a server through a snapshot walk. That walk
filters by read access, and is marked durably on the receiving side so an
interrupted transfer is detected and repaired. Records are fetched by id.

Syncing never overwrites files in a running workspace. A fetch only adds files
the user may newly read.

## Workspaces and platform code

`origin workspace new [--change X] [path]` creates a workspace that shares
the repository's store:
- Each workspace has its own current change and stat cache.
- Without `--change`, it starts a new change on the current change's base.
- Files that another working copy holds unmodified are cloned copy-on-write
  from it (on Btrfs, XFS or APFS), or copied otherwise. The rest are written
  from the store.

Checkouts are crash-safe:
- Before any working-copy move that changes files, Origin durably writes a
  `moving` marker. It records the trees the move goes from and to, and the
  target revision.
- The checked-out files are made durable before the workspace pointer moves.
- A checkout never overwrites a file edited after the command's snapshot. It
  checks each file again just before replacing it, and if one changed, the
  move is refused and undone.
- After a crash, the next command finishes the move or takes it back. It only
  touches files that still match one side, so unrecorded edits survive.

Code that differs between platforms lives behind traits in
`crates/origin-platform`:

| Trait | Linux | macOS |
|---|---|---|
| `Durability` | `fdatasync` for single files, one `syncfs` per batch | `fsync` per file, then one `F_FULLFSYNC` |
| `FileLock` | `flock` | `flock` |
| `CloneFile` | `FICLONE` | `clonefile` |
| `FileSystem` | `statfs` magic numbers | `statfs` type names |
| `ReadAt` | `pread` | `pread` |
| `Processes` | kill a process group once `/proc` shows it holds its run's lock | kill nothing: it can't tell which files a group holds |

Each platform has its own module, and a single `cfg_select!` picks one by
build target. That is the only conditional compilation by OS in the codebase.
One conformance test suite runs against whichever implementation is built.
The standard library, `rustix` and `reflink-copy` provide the system calls,
so `unsafe` stays forbidden everywhere. Windows is not supported.

`FileSystem` lets writers refuse network file systems such as NFS, SMB, 9p
and AFP. It can't detect FUSE file systems or synced folders such as Dropbox
or iCloud, and accepts them.

## On disk

A repository is a directory. Its store:

```
.origin/
  objects/loose/<2 hex>/<62 hex>         # zstd objects, recently written
  objects/packs/<id>.{pack,idx}          # small objects, compacted
  objects/packs/manifest                 # the packs in use; replacing it publishes packs
  objects/packs/lock                     # held by the single compactor
  objects/collection                     # writers hold it shared, gc exclusive
  targets/<target id>/{name,log,head}    # the chain, and its latest acceptance
  queue/<target id>/                     # landings waiting, and the queue's lock
  index/{log,lock}                       # derived: heads, statements by subject, digests
  oplog/{generation,lock,writers/,<segments>}
  push                                   # serializes revision puts and queue rebases
  fetching/<tree>                        # snapshot fetches not yet complete
```

A working copy adds:

```
  workspace                              # the working copy's revision
  state                                  # the stat cache
  moving                                 # a move in progress
  workspaces/<hash>                      # the other workspaces of this repository
  remote                                 # the remote's URL
  access/                                # each user's rights and event cursor on the remote
```

Another workspace's `.origin/` holds only `workspace`, `state`, `moving` and
`repository`, which names the main `.origin`.

- **Durability order.** Each batch writes its files beside their
  destinations, syncs them in one batch sync, renames them into place and
  syncs the directories. Only then is anything published that refers to them.
  Statements are published before the revision they describe.
- **Locking.** Records are immutable, so writing one needs no lock. Target
  chains, queues, pushes, the index, the op log and the compactor take
  `flock`s, which the kernel releases if the process dies.
- **Packs.** Statements are the bulk of the data, so loose objects of up to
  16 KiB are packed. The server compacts in the background once about 8,000
  have piled up, and `origin gc` compacts on demand.
- **Names.** User-chosen names (targets, changes) never become file names.
  They are hashed, which avoids case-folding and Unicode normalization
  problems on macOS.
- **The index is derived.** It catches up from the op log on open.
  `origin index rebuild` reconstructs it from the store, so a corrupt index
  never loses data.
- **Op log segments** are checksummed frames. A half-written last frame is
  cut off. Damage in the middle is read past, reported, and makes the index
  rebuild.
- **Temporary files** are `.tmp-<pid>-<n>` beside their destination.
  `origin gc` removes the ones a crash left behind.
- **Local filesystems only.** Writers refuse network file systems. A shared
  folder is a remote, never a live repository.

## Command line

```
origin init                         # a repository with `policy` and `main`
origin new [desc] [--on <target>]   # start a change
origin describe "..."               # refine its intent
origin edit <change>                # move the working copy
origin status | diff | show [<change>] | log [<target>] | evolog [<change>]
origin converge <change>            # merge divergent heads
origin resolve <path>...            # accept conflicted files as they are
origin abandon [<change>] --reason "..."
origin annotate <rev> <kind> --pass|--fail|--neutral [--inputs ...] [--external] [--payload f]
origin statements [<rev>] | promote [<rev>]
origin target new <name> --from <target> | targets
origin land [<change>] [--onto <target>] | land --from <target> --onto <target> | land --status
origin clone <url> [path] [--target <t>] | remote [<url>] | fetch | pull | push [<change>...]
origin watch [--kind ...] [--subject ...] [--since <cursor>] [--local]
origin workspace new [--change <c>] [path] | workspace list | workspace rm <path>
origin undo | op log
origin whoami | permissions | token issue --scope ... [--ttl ...]
origin gc | index rebuild
origin serve | repo create <path> [--admin <principal>]
origin <plugin> ...                 # runs origin-<plugin>, e.g. `origin ci run`
```

## Testing and benchmarks

- **Unit and integration tests** cover each module.
- **End-to-end tests** run the `origin` binary in temporary directories, over
  a real `origin serve` and over local paths.
- **HTTP tests** drive every SDK method against a real server.
- **OpenAPI:** `docs/openapi.json` must match the server; `just openapi`
  rewrites it.

Benchmarks (`just bench`) cover:
- store writes and pack lookups;
- snapshots, checkouts and workspace creation on a 20,000-file tree;
- input digests and promotion;
- landing, including concurrent proposers and target-onto-target landings;
- permission checks;
- reading events;
- server request latency and long-poll wakeups;
- clone, push and fetch over HTTP, including a fetch that brings nothing from
  a long history;
- the CI worker's overhead per check.

`just bench-git` times `status`, snapshots and sync against git with
hyperfine.

The **swarm scenario** is a skill run from T3 Code:
1. Three agents work in parallel workspaces and land on a feature target. One
   changes a shared signature that another depends on, so a landing
   conflicts.
2. Stale results are rerun and unchanged ones are promoted.
3. Processes are killed with `kill -9` along the way: a checkout, the CI
   worker and the server.
4. The work reaches `main` in a single acceptance.
5. The skill checks that target chains stay consistent and that no revision
   is lost, and reports timings.

`scripted.sh` is its dry run, with scripted edits in place of the agents.

## Later

- A virtual filesystem for workspaces: FUSE on Linux first, then FSKit on
  macOS. It goes behind a `Vfs` trait in `origin-platform`.
- A `Watch` trait (inotify, FSEvents) instead of polling, if polling ever
  shows up in profiles.
- Webhooks, as a plugin.
- Line-level and structure-aware merging. Merges are path-level today.
- Batching several changes per acceptance, and speculative merge trains.
- Set reconciliation (Negentropy-style) for sync.
- Remotes on plain storage, such as S3 with conditional writes.
- `origin-review` (a UI) and `origin-semantic` (tree-sitter diffs).
- Signed statements from outside issuers, and identities separate from keys.
- A git bridge.
