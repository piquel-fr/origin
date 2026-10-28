---
name: swarm-scenario
description: Runs Origin's swarm scenario on this machine. It starts `origin serve` and an `origin ci` worker, then has three agents, delegated through T3 Code, land overlapping changes on a feature target while processes are killed with kill -9. It then verifies the target chains, the revisions, which results reran or were promoted, and the single acceptance on `main`, and reports timings. Use when asked to run the swarm scenario, or to stress-test origin with parallel agents.
---

# The swarm scenario

Three agents work in parallel workspaces of one clone and land on a feature
target, `swarm`. One changes a shared function's signature, one adds a
caller of the old signature, so the two conflict, and one edits only docs.
Along the way a checkout, the CI worker and the server are killed with
`kill -9`. Finally `swarm` lands on `main` in one acceptance, and step 9
checks what should hold.

This is a runbook: run each block, watch the output, and note anything that
differs from what's expected.

**Conventions.** Everything lives in `/tmp/origin-swarm`, which step 1
deletes, and the server listens on `127.0.0.1:7979`. Run the blocks from
the repository root, in the session's shell. Shell state doesn't carry
over between Bash calls, so each block sets its variables. `$R/bin/<user>`
runs the release `origin` as that user, with their own configuration and
token. `swarm`, beside this file, holds the scenario's helpers: run it
without arguments to list them. Start long-running processes (`swarm
serve`, `swarm worker` and the `swarm kill-*` watchers) with the Bash
tool's `run_in_background`.

**Do a dry run first.** `bash .claude/skills/swarm-scenario/scripted.sh`
runs the same steps with scripted edits in place of the agents, in a
temporary directory of its own, on port 7978. It takes about 15 s once
built. If it fails, origin has regressed: stop and report its output.

## Known issues

None at the moment: the five the dry run found are fixed. In the report,
describe anything that goes wrong in full.

## 1. Build

```sh
R=/tmp/origin-swarm S=$PWD/.claude/skills/swarm-scenario/swarm
rm -rf $R && mkdir -p $R/server && $S mark "1. build"
cargo build --release --locked -p origin-src -p origin-ci
```

## 2. The server

```sh
R=/tmp/origin-swarm S=$PWD/.claude/skills/swarm-scenario/swarm
$S mark "2. server"
cat >$R/server/origin-server.toml <<'EOF'
listen = "127.0.0.1:7979"
node = "hosted"
key = "node.key"

[repos]
demo = "repos/demo"

[tokens]
"user:lead" = "lead-secret"
EOF
target/release/origin repo create $R/server/repos/demo --config $R/server/origin-server.toml --admin user:lead
$S user lead lead-secret
```

Start `.claude/skills/swarm-scenario/swarm serve` in the background, and
wait for `Listening on` in `/tmp/origin-swarm/serve.log`. `user:lead` acts
for you: it changes the policy, creates `swarm`, and lands it on `main`.

## 3. The fixture and the policy

```sh
R=/tmp/origin-swarm S=$PWD/.claude/skills/swarm-scenario/swarm URL=http://127.0.0.1:7979/demo
$S mark "3. fixture and policy"
$R/bin/lead clone $URL $R/lead
(cd $R/lead && $S checks && $R/bin/lead describe "Add the checks" && $R/bin/lead land)
$R/bin/lead clone $URL $R/policy --target policy
(cd $R/policy && $S policy && $R/bin/lead describe "Require ci.test from the runners" && $R/bin/lead land)
(cd $R/lead && $S fixture && $R/bin/lead describe "Add the greeter" && $R/bin/lead land)
```

`swarm policy` makes `main` require `ci.test` from `group:runners`, which
holds `user:ci`. It also makes `swarm` require both `ci.test` and
`ci.docs`, so each agent's landing shows a rerun or a promotion. `swarm
checks` writes `checks.toml`, with `ci.test` (`sleep 2 && python3 -m
unittest discover -s src`, inputs `src`) and `ci.docs` (inputs `docs`). It
lands on `main` first, before the policy requires anything: the worker
reads the checks from the tip of the target landed on. `swarm fixture`
then writes:

- `src/greet.py`, which defines `greet(name)`;
- `src/app.py`, whose `welcome()` calls it;
- `src/test_app.py`;
- `docs/guide.md`;
- 5,000 files under `assets/`, so a checkout lasts long enough to be killed.

Expected: the checks land as `main #1`, and the policy as `policy #1`. The
greeter's landing waits for `ci.test pass from group:runners`, because no
runner is up yet.

## 4. Tokens

```sh
R=/tmp/origin-swarm S=$PWD/.claude/skills/swarm-scenario/swarm
$S mark "4. tokens"
token() { target/release/origin token issue --config $R/server/origin-server.toml --ttl 1d --as "$@"; }
$S user ci "$(token user:ci --scope 'read /' --scope 'annotate ci.*')"
$S user agent-a "$(token user:agent-a --scope 'read /' --scope 'write src' --scope 'land swarm')"
$S user agent-b "$(token user:agent-b --scope 'read /' --scope 'write src' --scope 'land swarm')"
$S user agent-c "$(token user:agent-c --scope 'read /' --scope 'write docs' --scope 'land swarm')"
```

No agent can land on `main` or write outside its directory.

## 5. The worker

Run `.claude/skills/swarm-scenario/swarm mark "5. worker"`, then start
`.claude/skills/swarm-scenario/swarm worker` in the background. Within a
few seconds, `/tmp/origin-swarm/worker.log` should show `ci.test pass in
2.0…s`, and `(cd /tmp/origin-swarm/lead && /tmp/origin-swarm/bin/lead land
--status)` should print `No landing waits.`

## 6. Workspaces and agents

```sh
R=/tmp/origin-swarm S=$PWD/.claude/skills/swarm-scenario/swarm URL=http://127.0.0.1:7979/demo
$S mark "6. agents"
(cd $R/lead && $R/bin/lead target new swarm --from main)
$R/bin/lead clone $URL $R/clone --target swarm
(cd $R/clone && $R/bin/agent-a workspace new $R/ws-a && $R/bin/agent-b workspace new $R/ws-b && $S kill-checkout c)
```

`swarm kill-checkout c` is the first kill. It kills agent c's `workspace
new` mid-checkout and shows the workspace as `(incomplete)` in `workspace
list`. Then it removes the workspace and creates it again. Note how many
files the checkout got through.

Before delegating, arm the other two kills, each in the background:

- `.claude/skills/swarm-scenario/swarm kill-worker-on-check` kills the
  worker as soon as it starts the first agent's `ci.test`, then becomes the
  new worker.
- `.claude/skills/swarm-scenario/swarm kill-serve-after-push` kills the
  server once agent c says it pushed, then becomes the new server and tells
  c to go on.

Then delegate the agents. Make three `delegate_task` calls in one message,
with `mode: "async"`, titled `swarm agent a`, `swarm agent b` and `swarm
agent c`. Each task is the shared prompt, with the agent's letter in place
of `x`, followed by that agent's task. The agents run shell commands
without asking, so delegate from a full-access session. Note when they
start.

**Shared prompt:**

> You are agent x in a test of Origin, a version control system. It is not
> git: don't run git. Your workspace is `/tmp/origin-swarm/ws-x`. Run
> origin only as `/tmp/origin-swarm/bin/agent-x`, written `origin` below,
> and start every shell command with `cd /tmp/origin-swarm/ws-x &&`. There
> is no commit and no staging area: commands that build on your change,
> such as `describe` and `land`, record the working copy's files first.
>
> 1. Run `origin status`: your change is on the target `swarm`.
> 2. Do your task, below. Edit only the files it names.
> 3. Run `origin describe "<the description your task gives>"`.
> 4. Run `origin land --onto swarm`.
> 5. If the landing waits for checks, run `origin land --status` every 5
>    seconds until your change isn't listed, for up to 3 minutes. Then run
>    `origin log swarm`.
> 6. If the landing conflicts, stop there. Don't resolve the conflict.
> 7. The server is restarted on purpose during the test. If a command
>    fails with `cannot reach the server`, wait 5 seconds and retry it, up
>    to 12 times.
>
> Report every origin command you ran and its output, your change's id
> from `origin status`, and how long the whole task took you.

**Agent a:** In `src/greet.py`, change `greet(name)` to `greet(name,
punctuation)`, returning `f"Hello, {name}{punctuation}"`. Make `welcome()`
in `src/app.py` pass `"!"`. Check your work with `python3 -m unittest
discover -s src`. Description: "Make greet take the punctuation".

**Agent b:** In `src/app.py`, add `def farewell(): return
greet("friends") + " Goodbye."`. It calls `greet` with the one argument
it takes in your workspace. Never run `origin pull`. Check your work with
`python3 -m unittest discover -s src`. Description: "Add a farewell".

**Agent c:** In `docs/guide.md`, add the line ``Test it with `python3 -m
unittest discover -s src`.`` Description: "Document the tests". Replace
step 4 with these steps:

1. Run `origin push`.
2. Run `touch /tmp/origin-swarm/c-pushed`.
3. Wait until `/tmp/origin-swarm/serve-restarted` exists. Check every 2
   seconds, for up to 5 minutes.
4. Run `origin land --onto swarm`.

## 7. While they work

- The worker kill writes `killed the worker mid-check` to `worker.log`,
  followed by a new `Answering …` line. The check it cut short then runs
  again.
- The server kill writes `killed the server after c's push` to
  `serve.log`. The worker outlives it: `worker.log` shows `Cannot reach
  the remote` until the server is back, and `kill -0 $(cat
  /tmp/origin-swarm/worker.pid)` succeeds.
- To see what waits, and why, run `(cd /tmp/origin-swarm/lead &&
  /tmp/origin-swarm/bin/lead land --status)`.

Expected:

- Whichever of a and b lands second conflicts in `src/app.py`, with a
  `core.conflict`.
- c's landing waits only for `ci.docs`, because its `ci.test` result is
  promoted.

## 8. Resolve the conflict, then land on `main`

Run `.claude/skills/swarm-scenario/swarm mark "8. resolve"`. Then set `X`
to the agent whose landing conflicted, `a` or `b`:

```sh
R=/tmp/origin-swarm X=b
(cd $R/ws-$X && $R/bin/agent-$X pull)        # "Conflicts: src/app.py hold both versions ..."
cp $R/ws-$X/src/app.py $R/app.py.$X          # keep both versions, with the markers
(cd $R/clone && $R/bin/lead pull)            # the clone's working copy moves to swarm's tip
cp $R/clone/src/app.py $R/ws-$X/src/app.py   # take swarm's side
```

`pull` rebased the change onto swarm's tip, with both versions of
`src/app.py` between conflict markers and a `core.conflict` listing it, so
the change can't land until `src/app.py` changes (or `resolve` accepts it
as it is). Redo the agent's edit
on swarm's version of `src/app.py`, so that every call passes the
punctuation: for b, `farewell` calls `greet("friends", ".")`. Then test and
land:

```sh
R=/tmp/origin-swarm X=b
(cd $R/ws-$X && python3 -m unittest discover -s src && $R/bin/agent-$X land --onto swarm)
```

The landing waits for `ci.test`, which reruns because `src` changed, while
`ci.docs` is promoted. Once `land --status` no longer lists the change,
land `swarm` on `main`:

```sh
R=/tmp/origin-swarm S=$PWD/.claude/skills/swarm-scenario/swarm
$S mark "8b. land swarm on main"
(cd $R/lead && $R/bin/lead land --from swarm --onto main)
```

The landing is checked as the snapshot it would make. Its results are
promoted, so it lands at once as `main #3`. If it waits instead, run
`(cd $R/lead && $R/bin/lead fetch && $R/bin/lead log main)` until `main #3`
shows.

## 9. Verify

The target chains are the same in a fresh clone:

```sh
R=/tmp/origin-swarm S=$PWD/.claude/skills/swarm-scenario/swarm URL=http://127.0.0.1:7979/demo
$S mark "9. verify"
(cd $R/lead && $R/bin/lead fetch --target swarm && $R/bin/lead log main && $R/bin/lead log swarm)
$R/bin/lead clone $URL $R/fresh --target swarm && (cd $R/fresh && $R/bin/lead fetch --target main)
for t in main swarm; do
  diff <(cd $R/lead && $R/bin/lead log $t | grep -v -e '^@' -e '^Behind: ') <(cd $R/fresh && $R/bin/lead log $t | grep -v -e '^@' -e '^Behind: ') && echo "$t: same"
done
```

Expected:

- `main` has exactly four acceptances, #0 to #3, and `main #3` lists
  `swarm #3`: the whole swarm reached `main` in one acceptance.
- `swarm` has #0 to #3, one agent's change on each of #1 to #3.
- Both chains print `same`.

No revision is lost: the server has every revision the agent's workspace
has. Each clone lists revisions in the order they arrived, and the agent's
marks its working copy with `@`. Run this for each agent, with `c` set to
the change id it reported:

```sh
R=/tmp/origin-swarm x=a c=<change id>
full=$(cd $R/ws-$x && $R/bin/agent-$x show $c | sed -n 's/^Change \([0-9a-f]\{32\}\)$/\1/p')
(cd $R/fresh && $R/bin/lead fetch --change $full)
diff <(cd $R/fresh && $R/bin/lead evolog $c | sort) <(cd $R/ws-$x && $R/bin/agent-$x evolog $c | sed 's/^@/○/' | sort) && echo "$x: no revision lost"
```

Results reran or were promoted as they should. `statements` reads the
records a clone holds, and the fresh clone fetched every change above:

```sh
R=/tmp/origin-swarm
(cd $R/fresh && $R/bin/lead statements <c's change>)
(cd $R/fresh && $R/bin/lead statements <the resolved change>)
grep -E 'pass in|promoted|killed|Answering' $R/worker.log
```

- c's `ci.test` is `by node:hosted`, `promoted from statement … by
  user:ci`: promoted, not rerun. Its `ci.docs` is `by user:ci`.
- The resolved change's `ci.test` is `by user:ci` (stale, so it reran),
  and its `ci.docs` is promoted.
- The worker ran `ci.test` for the greeter, for the first of a and b to
  land, and for the resolved change. It also ran it for the check the kill
  cut short, and for any revision a later one replaced. It never ran
  `ci.test` for c.

## 10. Report

`.claude/skills/swarm-scenario/swarm times` prints how long each step took.
Report:

- the step timings, and how long each agent took;
- each check of step 9: whether it held, with the output if not;
- each kill: when it happened, and how origin recovered;
- everything that went wrong, each with the command, what you expected,
  and what happened.

Then stop the server and the worker: `kill -9 $(cat /tmp/origin-swarm/serve.pid
/tmp/origin-swarm/worker.pid)`.
