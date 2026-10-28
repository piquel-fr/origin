#!/usr/bin/env bash
# The swarm scenario's dry run: SKILL.md's steps, with the same commands,
# kills and checks, and scripted edits in place of the agents. Run it from
# the repository: `bash .claude/skills/swarm-scenario/scripted.sh`. It
# prints the timings, and leaves its directory behind, logs included.
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
S=$ROOT/.claude/skills/swarm-scenario/swarm
export RUN PORT=${PORT:-7978}
RUN=$(mktemp -d -t origin-swarm.XXXXXX)
URL=http://127.0.0.1:$PORT/demo
ORIGIN=$ROOT/target/release/origin
LEAD=$RUN/bin/lead

fail() {
  echo "FAILED: $*" >&2
  exit 1
}
step() {
  "$S" mark "$1"
  echo
  echo "== $1"
}
# `has <pattern> <text>`, and `expect`, which fails instead of returning
# false. Origin's output is read whole: `origin log | grep -q` makes it
# panic on the closed pipe.
has() { grep -q -- "$1" <<<"$2"; }
expect() { has "$1" "$2" || fail "expected \`$1\` in: $2"; }
# `until_ok <command>...`: retries a command for up to 60 s.
until_ok() {
  for _ in $(seq 600); do "$@" && return; sleep 0.1; done
  fail "gave up waiting for: $*"
}
# `agent <a|b|c> <args>`: origin as the agent, in its workspace.
agent() { (cd "$RUN/ws-$1" && "$RUN/bin/agent-$1" "${@:2}"); }
change() { agent "$1" status | sed -n 's/^Working copy: \([0-9a-f]*\) .*/\1/p'; }
# Whether change `$2` landed on `swarm`, as agent `$1` sees it.
on_swarm() { agent "$1" fetch >/dev/null && has "^    $2 " "$(agent "$1" log swarm)"; }
# `count <log> <pattern>`: the lines of `$RUN/<log>` holding `<pattern>`.
count() { grep -c "$2" "$RUN/$1" || true; }
# Whether `<log>` has more lines holding `<pattern>` than `<before>`.
grew() { (($(count "$1" "$3") > $2)); }

serve() {
  local before
  before=$(count serve.log "Listening on")
  "$S" serve &
  until_ok grew serve.log "$before" "Listening on"
}
worker() {
  local before
  before=$(count worker.log "Answering")
  "$S" worker &
  until_ok grew worker.log "$before" "Answering"
}
touch "$RUN/serve.log" "$RUN/worker.log"
trap 'kill -9 $(cat "$RUN"/*.pid) $(jobs -p) 2>/dev/null || true' EXIT

step "1. build"
cargo build --release --locked -p origin-src -p origin-ci --manifest-path "$ROOT/Cargo.toml"

step "2. server"
mkdir -p "$RUN/server"
cat >"$RUN/server/origin-server.toml" <<EOF
listen = "127.0.0.1:$PORT"
node = "hosted"
key = "node.key"

[repos]
demo = "repos/demo"

[tokens]
"user:lead" = "lead-secret"
EOF
"$ORIGIN" repo create "$RUN/server/repos/demo" --config "$RUN/server/origin-server.toml" --admin user:lead
serve
"$S" user lead lead-secret

step "3. fixture and policy"
"$LEAD" clone "$URL" "$RUN/lead"
cd "$RUN/lead"
"$S" checks
"$LEAD" describe "Add the checks"
expect "on main #1" "$("$LEAD" land)"
"$LEAD" clone "$URL" "$RUN/policy" --target policy
cd "$RUN/policy"
"$S" policy
"$LEAD" describe "Require ci.test from the runners"
expect "on policy #1" "$("$LEAD" land)"
cd "$RUN/lead"
"$S" fixture
"$LEAD" describe "Add the greeter"
expect "waits for ci.test pass from group:runners" "$("$LEAD" land)"

step "4. tokens"
token() { "$ORIGIN" token issue --config "$RUN/server/origin-server.toml" --ttl 1d --as "$@"; }
"$S" user ci "$(token user:ci --scope "read /" --scope "annotate ci.*")"
"$S" user agent-a "$(token user:agent-a --scope "read /" --scope "write src" --scope "land swarm")"
"$S" user agent-b "$(token user:agent-b --scope "read /" --scope "write src" --scope "land swarm")"
"$S" user agent-c "$(token user:agent-c --scope "read /" --scope "write docs" --scope "land swarm")"

step "5. worker"
worker
until_ok grep -q "ci.test pass in" "$RUN/worker.log"
expect "No landing waits." "$("$LEAD" land --status)"

step "6. swarm target and workspaces"
"$LEAD" target new swarm --from main
"$LEAD" clone "$URL" "$RUN/clone" --target swarm
cd "$RUN/clone"
"$RUN/bin/agent-a" workspace new "$RUN/ws-a"
"$RUN/bin/agent-b" workspace new "$RUN/ws-b"
"$S" kill-checkout c
A=$(change a) B=$(change b) C=$(change c)
"$S" kill-worker-on-check &
"$S" kill-serve-after-push &

step "7. agents, and the kills"
# a changes the signature, and its landing's check kills the worker.
cat >"$RUN/ws-a/src/greet.py" <<'EOF'
def greet(name, punctuation):
    return f"Hello, {name}{punctuation}"
EOF
sed -i 's/greet("world")/greet("world", "!")/' "$RUN/ws-a/src/app.py"
agent a describe "Make greet take the punctuation"
agent a fetch
expect "waits for ci.test" "$(agent a land --onto swarm)"
until_ok grep -q "killed the worker mid-check" "$RUN/worker.log"
until_ok on_swarm a "$A"

# b calls the old signature.
cat >>"$RUN/ws-b/src/app.py" <<'EOF'


def farewell():
    return greet("friends") + " Goodbye."
EOF
agent b describe "Add a farewell"
agent b fetch
expect "conflicts in src/app.py" "$(agent b land --onto swarm)"

# c documents, and the server is killed between its push and its landing.
cat >>"$RUN/ws-c/docs/guide.md" <<'EOF'

Test it with `python3 -m unittest discover -s src`.
EOF
agent c describe "Document the tests"
# c never fetched: b's fetch, with b's rights, mustn't refuse its push.
agent c push
touch "$RUN/c-pushed"
until_ok test -e "$RUN/serve-restarted"
sleep 0.5
kill -0 "$(<"$RUN/worker.pid")" 2>/dev/null || fail "the worker died with the server"
agent c land --onto swarm
until_ok on_swarm c "$C"

step "8. resolve the conflict"
# b's pull rebases it onto swarm's tip, with the conflict between markers.
expect "Conflicts: src/app.py" "$(agent b pull)"
expect "<<<<<<<" "$(<"$RUN/ws-b/src/app.py")"
(cd "$RUN/clone" && "$LEAD" pull)
cp "$RUN/clone/src/app.py" "$RUN/ws-b/src/app.py"
cat >>"$RUN/ws-b/src/app.py" <<'EOF'


def farewell():
    return greet("friends", ".") + " Goodbye."
EOF
agent b fetch
agent b land --onto swarm
until_ok on_swarm b "$B"
cd "$RUN/lead"
"$LEAD" land --from swarm --onto main
main_advanced() { "$LEAD" fetch >/dev/null && has "main #3" "$("$LEAD" log main)"; }
until_ok main_advanced

step "9. verify"
# The chains, without the working copy's lines, are the same in a fresh
# clone.
chain() { "$LEAD" log "$1" | grep -v -e '^@' -e '^Behind: '; }
"$LEAD" fetch --target swarm >/dev/null
chain main | tee "$RUN/main.log"
chain swarm | tee "$RUN/swarm.log"
"$LEAD" clone "$URL" "$RUN/fresh" --target swarm >/dev/null
cd "$RUN/fresh"
"$LEAD" fetch --target main >/dev/null
diff <(chain main) "$RUN/main.log"
diff <(chain swarm) "$RUN/swarm.log"
# main advanced by one acceptance, landing swarm's tip.
test "$(grep -c '^◆ main' "$RUN/main.log")" -eq 4 || fail "main has more than 4 acceptances"
expect "$(printf '◆ main #3 [0-9a-f]*\n    swarm #3')" "$(head -2 "$RUN/main.log")"
# Every revision of each change reached the server. Each clone lists them
# in the order they arrived, and the agent's marks its working copy.
for name in a b c; do
  id=${name^^}
  id=${!id}
  full=$(agent $name show "$id" | sed -n 's/^Change \([0-9a-f]\{32\}\)$/\1/p')
  "$LEAD" fetch --change "$full" >/dev/null
  diff <("$LEAD" evolog "$id" | sort) <(agent $name evolog "$id" | sed 's/^@/○/' | sort) ||
    fail "$name's revisions differ"
  agent $name evolog "$id"
done
# c's docs-only change had ci.test promoted; b's resolved one reran it.
STATEMENTS=$("$LEAD" statements "$C")
echo "$STATEMENTS"
expect "ci.test pass by node:hosted" "$STATEMENTS"
expect "promoted from statement" "$(grep -A1 "ci.test" <<<"$STATEMENTS")"
STATEMENTS=$("$LEAD" statements "$B")
echo "$STATEMENTS"
expect "ci.test pass by user:ci" "$STATEMENTS"
# ci.test ran for the fixture, a and b, and no more.
test "$(count worker.log "ci.test pass in")" -eq 3 || fail "ci.test ran other than 3 times"
cat "$RUN/worker.log"

echo
"$S" times
echo "Run directory: $RUN"
