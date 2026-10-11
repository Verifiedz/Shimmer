#!/bin/bash
# Issue #34 (M3): the workspace checks on a real machine, written to a pass/fail log.
#
#   .dev/test-workspaces-34.sh               build this checkout (cargo build) and test it
#   .dev/test-workspaces-34.sh --installed   test the shimmer on your PATH instead
#
# It never changes your branch or installs anything. Each run tests in its own throwaway Shimmer
# folder (~/shimmer-34-test/run-<pid>, removed at the end) with its own daemon, so your real
# Shimmer data is never touched, and runs can even overlap. Logs stay in ~/shimmer-34-test. Every check runs even if an earlier one fails. Uses the smoke-test
# template, which opens no windows. Post the log on #34. Works on macOS and Linux.

set -u
case "${1:-}" in
    "" | --installed) ;;
    *) echo "usage: $0 [--installed]"; exit 2 ;;
esac
REPO="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$HOME/shimmer-34-test"
mkdir -p "$ROOT"
LOG="$ROOT/results-$(date +%Y%m%d-%H%M%S)-$$.log"
RUN="$ROOT/run-$$"
exec > >(tee "$LOG") 2>&1

RESULTS=""
FAILS=0
NOTES=""
say() { printf '\n== %s\n' "$*"; }
pass() { RESULTS="$RESULTS$1 PASS  $2\n"; echo "   >>> $1 PASS: $2"; }
fail() { RESULTS="$RESULTS$1 FAIL  $2\n"; FAILS=$((FAILS + 1)); echo "   >>> $1 FAIL: $2"; }
note() { NOTES="$NOTES- $*\n"; echo "   >>> NOTE: $*"; }

# ---------------------------------------------------------------- setup

say "Setup"
echo "date: $(date)"
echo "system: $(uname -sm)"
[ "$(uname)" = "Darwin" ] && echo "macOS: $(sw_vers -productVersion)"

missing=""
for tool in perl lsof ps; do command -v "$tool" >/dev/null || missing="$missing\n  - $tool"; done
[ -x "$HOME/.cargo/bin/cargo" ] && export PATH="$HOME/.cargo/bin:$PATH"

if [ "${1:-}" = "--installed" ]; then
    BIN="$(command -v shimmer)"
    [ -n "$BIN" ] || missing="$missing\n  - shimmer on your PATH (or run without --installed to build this checkout)"
else
    command -v cargo >/dev/null || missing="$missing\n  - Rust: https://rustup.rs (or use --installed)"
    BIN="$REPO/target/debug/shimmer"
fi
if [ -n "$missing" ]; then
    printf "Missing, install these and run the script again:$missing\n"
    echo "log: $LOG"
    exit 1
fi
if [ "${1:-}" != "--installed" ]; then
    say "Building $REPO"
    if ! (cd "$REPO" && cargo build --locked -p shimmer); then
        echo "The build failed (see above). Fix that first; nothing was tested."
        echo "log: $LOG"
        exit 1
    fi
fi
echo "shimmer: $BIN"
echo "commit: $(git -C "$REPO" log -1 --oneline 2>/dev/null) ($(git -C "$REPO" rev-parse --abbrev-ref HEAD 2>/dev/null))"
[ -n "$(git -C "$REPO" status --porcelain 2>/dev/null)" ] && echo "(the checkout has uncommitted changes)"
# Steps call `shimmer` themselves (the ping, the records callback): make it this same binary.
PATH="$(dirname "$BIN"):$PATH"
export PATH

# The throwaway Shimmer: this run's own folder and socket, so its own daemon.
export SHIMMER_HOME="$RUN/home"
export SHIMMER_SOCKET="$RUN/d.sock"
rm -rf "$RUN"
mkdir -p "$RUN"
H="$SHIMMER_HOME"
# Its own workspace name too: the templates keep run-time files in $TMPDIR/shimmer-<name>, shared
# by every Shimmer folder on the machine, so overlapping runs must not share a name.
WS="smoke-$$"
W="$H/data/workspaces/$WS"
ORIG="$RUN/smoke-orig"
case "$(uname)" in Darwin) PLATFORM=macos ;; Linux) PLATFORM=linux ;; *) PLATFORM=unknown ;; esac

# ---------------------------------------------------------------- helpers

# Run shimmer, show the command and its output; returns shimmer's exit code. OUT holds the output.
OUT=""
sh_() {
    echo "\$ shimmer $*"
    OUT="$("$BIN" "$@" 2>&1)"
    local rc=$?
    [ -n "$OUT" ] && echo "$OUT" | sed 's/^/    /'
    echo "    [exit $rc]"
    return $rc
}
state() { "$BIN" workspaces status "$WS" --json 2>/dev/null | sed -n 's/^ *"state": *"\([a-z]*\)".*/\1/p' | head -1; }
# The processes this run starts sleep for durations no other process will have: step 3's
# background sleep (BG) and the slow steps the tests add (SLOW), both unique to this run. Only
# this user's processes are looked at, so another run, or anything else on the machine, is never
# counted or killed. Matched field by field, so the matching command itself is never counted.
BG=$((10000000 + $$))
SLOW=$((20000000 + $$))
procs() {
    ps -U "$(id -u)" -o pid=,pgid=,command= | awk -v bg="$BG" -v slow="$SLOW" \
        '$3 == "sleep" && ($4 == bg || $4 == slow) && NF == 4'
}
count() { procs | awk -v n="$1" '$4 == n' | wc -l | tr -d ' '; }
show_procs() { echo "\$ ps (smoke-test processes)"; procs | sed 's/^/    /' || true; [ -z "$(procs)" ] && echo "    (none)"; }
restore() {
    cp "$ORIG/workspace.toml" "$ORIG/cleanup.sh" "$W/"
    cp "$ORIG"/0*.sh "$W/steps/"
    chmod +x "$W"/steps/*.sh
}
set_fail_at() { perl -pi -e "s/^FAIL_AT = \".*\"/FAIL_AT = \"$1\"/" "$W/workspace.toml"; }
set_wait_timeout() { perl -0pi -e "s/(name = \"wait\".*?timeout_s = )\\d+/\${1}$1/s" "$W/workspace.toml"; }
daemon_pid() { lsof -t "$SHIMMER_SOCKET" 2>/dev/null | head -1; }
newest_check_log() { ls -t "$H/logs" 2>/dev/null | grep -- '-check\.log$' | head -1; }
kill_leftovers() { for pid in $(procs | awk '{print $1}'); do kill "$pid" 2>/dev/null; done; }
# Stopped early (Ctrl-C, a failed step): still end what this run started.
trap 'kill_leftovers; "$BIN" shutdown >/dev/null 2>&1; rm -rf "$RUN"' EXIT

# ---------------------------------------------------------------- the test workspace

say "Creating the test workspace"
sh_ ping
sh_ workspaces new "$WS" --from smoke-test --set FAIL_AT=none
# Step 3 sleeps for this run's own duration (see procs); how it runs is unchanged.
perl -pi -e "s/^exec sleep 300\$/exec sleep $BG/" "$W/steps/03-background.sh"
grep -q "exec sleep $BG" "$W/steps/03-background.sh" || { echo "smoke-test's step 3 changed; update this script"; exit 1; }
mkdir -p "$ORIG"
cp "$W/workspace.toml" "$W/cleanup.sh" "$ORIG/" && cp "$W"/steps/*.sh "$ORIG/"
if [ ! -f "$ORIG/02-wait.sh" ]; then
    echo "Couldn't create the test workspace (see above); stopping."
    exit 1
fi

# ---------------------------------------------------------------- the checks

say "A (#34 item 1): supervised and detached steps activate; it goes active"
sh_ workspaces activate "$WS" --wait; rc=$?
# Activate returns once the detached step is started; give it a moment to be running.
for _ in 1 2 3 4 5 6 7 8 9 10; do [ "$(count "$BG")" -ge 1 ] && break; sleep 0.5; done
s=$(state); show_procs
if [ $rc -eq 0 ] && [ "$s" = active ] && [ "$(count "$BG")" -eq 1 ]; then
    pass A "activated, state active, the detached step is running"
else
    fail A "exit $rc, state '$s', detached steps running: $(count "$BG")"
fi

say "B (#34 items 9 and 8): every SHIMMER_* variable reaches the scripts; a step reaches the daemon"
log="$(newest_check_log)"
echo "step 1's log: $log"
[ -n "$log" ] && sed 's/^/    /' "$H/logs/$log"
missing_vars=""
for v in SHIMMER_WORKSPACE_ID SHIMMER_WORKSPACE_DIR SHIMMER_HOME SHIMMER_SOCKET SHIMMER_SESSION_ID SHIMMER_PLATFORM; do
    grep -q "^$v=." "$H/logs/$log" 2>/dev/null || missing_vars="$missing_vars $v"
done
grep -q "^SHIMMER_PLATFORM=$PLATFORM$" "$H/logs/$log" 2>/dev/null || missing_vars="$missing_vars SHIMMER_PLATFORM=$PLATFORM"
if grep -q "isn't on PATH" "$H/logs/$log" 2>/dev/null; then
    note "B: shimmer wasn't on PATH inside the step, so calling back wasn't tried"
fi
if [ -z "$log" ]; then
    fail B "no log for step 1 in $H/logs"
elif [ -n "$missing_vars" ]; then
    fail B "missing or wrong:$missing_vars"
elif ! grep -q "^pong" "$H/logs/$log"; then
    fail B "all variables set, but the step's 'shimmer ping' got no pong"
else
    pass B "all six variables set (SHIMMER_PLATFORM=$PLATFORM), and the step's ping answered"
fi

say "C (#34 item 2): detached processes outlive the daemon"
sh_ shutdown
sleep 2
show_procs
survived=$(count "$BG")
sh_ workspaces stop "$WS" --wait; rc=$?
sleep 1
s=$(state); show_procs
if [ "$survived" -ge 1 ] && [ $rc -eq 0 ] && [ "$s" = ready ] && [ -z "$(procs)" ]; then
    pass C "the detached step survived shutdown; stop then ended it, state ready"
else
    fail C "survived shutdown: $survived, stop exit $rc, state '$s', left running: $(procs | wc -l | tr -d ' ')"
fi
kill_leftovers

say "D (#34 item 3): a failing supervised step leaves it dirty; later steps don't run; a log is written"
sh_ workspaces reconfigure "$WS" --set FAIL_AT=check
before_logs=$(ls "$H/logs" | wc -l | tr -d ' ')
sh_ workspaces activate "$WS" --wait; rc=$?
s=$(state)
sh_ workspaces status "$WS"
show_procs
after_logs=$(ls "$H/logs" | wc -l | tr -d ' ')
if [ $rc -ne 0 ] && [ "$s" = dirty ] && [ -z "$(procs)" ] && echo "$OUT" | grep -q 'check' && [ "$after_logs" -gt "$before_logs" ]; then
    pass D "failed at step 1, state dirty, steps 2-3 didn't run, log written"
else
    fail D "activate exit $rc, state '$s', processes running: $(procs | wc -l | tr -d ' '), logs $before_logs -> $after_logs"
fi

say "E (#34 item 5): activate on a dirty workspace is refused, naming the step and the log"
sh_ workspaces activate "$WS" --wait; rc=$?
if [ $rc -ne 0 ] && echo "$OUT" | grep -qi 'dirty' && echo "$OUT" | grep -q 'log'; then
    pass E "refused as dirty, with the log"
else
    fail E "exit $rc; expected a refusal mentioning dirty and the log"
fi

say "F (#34 item 6, part 1): cleanup succeeds, dirty -> ready"
sh_ workspaces cleanup "$WS" --wait; rc=$?
s=$(state)
if [ $rc -eq 0 ] && [ "$s" = ready ]; then pass F "cleanup ran, state ready"; else fail F "exit $rc, state '$s'"; fi

say "G (#34 item 7): force-relaunch writes workspaces.session.forced to the event log"
sh_ workspaces activate "$WS" --wait
s=$(state)
[ "$s" = dirty ] || note "G: expected dirty before forcing, got '$s'"
set_fail_at none
grep '^FAIL_AT' "$W/workspace.toml"
sh_ workspaces force-relaunch "$WS" --yes --wait; rc=$?
s=$(state)
forced=$(cat "$H"/events/*.jsonl 2>/dev/null | grep -c 'workspaces.session.forced')
echo "workspaces.session.forced events: $forced"
cat "$H"/events/*.jsonl 2>/dev/null | grep 'workspaces.session.forced' | sed 's/^/    /'
if [ $rc -eq 0 ] && [ "$s" = active ] && [ "$forced" -eq 1 ]; then
    pass G "forced relaunch went active, one session.forced event logged"
else
    fail G "exit $rc, state '$s', session.forced events: $forced"
fi
sh_ workspaces stop "$WS" --wait
kill_leftovers

say "H (#34 item 6, part 2): with no cleanup script, only reset clears dirty"
restore
set_fail_at check
sh_ workspaces activate "$WS" --wait
mv "$W/cleanup.sh" "$W/cleanup.sh.off"
perl -0pi -e 's/\[cleanup\]\ntimeout_s = \d+\n//' "$W/workspace.toml"
sh_ workspaces cleanup "$WS" --wait; rc_cleanup=$?
s1=$(state)
sh_ workspaces reset "$WS" --yes; rc_reset=$?
s2=$(state)
if [ $rc_cleanup -ne 0 ] && [ "$s1" = dirty ] && [ $rc_reset -eq 0 ] && [ "$s2" = ready ]; then
    pass H "cleanup refused without a script (still dirty); reset cleared it"
else
    fail H "cleanup exit $rc_cleanup (state '$s1'), reset exit $rc_reset (state '$s2')"
fi
rm -f "$W/cleanup.sh.off"
restore

say "I (#34 item 4): a supervised step past its timeout has its whole process group killed"
set_wait_timeout 3
printf 'sleep %s &\nsleep %s\n' "$SLOW" "$SLOW" > "$W/steps/02-wait.sh"
grep -A4 'name = "wait"' "$W/workspace.toml" | sed 's/^/    /'
sh_ workspaces activate "$WS" --wait; rc=$?
s=$(state)
sleep 2
show_procs
left=$(count "$SLOW")
if [ $rc -ne 0 ] && [ "$s" = dirty ] && echo "$OUT" | grep -qi 'time' && [ "$left" -eq 0 ]; then
    pass I "timed out after 3 s, state dirty, both sleeps killed"
else
    fail I "exit $rc, state '$s', timeout mentioned: $(echo "$OUT" | grep -ci 'time'), left running: $left"
fi
sh_ workspaces cleanup "$WS" --wait
kill_leftovers
restore

say "J (#34 item 11): killing the daemon mid-launch leaves the workspace dirty after the next start"
set_wait_timeout 120
printf 'sleep %s\n' "$SLOW" > "$W/steps/02-wait.sh"
sh_ workspaces activate "$WS"
sleep 5
pid=$(daemon_pid)
echo "daemon pid: $pid"
if [ -n "$pid" ]; then
    kill -9 "$pid"
    sleep 1
    s=$(state)
    sh_ workspaces status "$WS"
    if [ "$s" = dirty ]; then
        pass J "after kill -9 mid-launch, the restarted daemon shows it dirty"
    else
        fail J "after kill -9 mid-launch, state is '$s' (expected dirty)"
    fi
    sh_ workspaces cleanup "$WS" --wait
    sleep 1
    show_procs
    if [ -n "$(procs)" ]; then
        note "J: after the daemon was killed mid-step and cleanup ran, the step's process was still running (an orphan, known: #158): $(procs | tr -s ' ' | tr '\n' ';')"
    fi
else
    fail J "couldn't find the daemon's process (lsof on the socket)"
fi
kill_leftovers
restore

say "K (#34 item 8): a step script calls back in through SHIMMER_SOCKET"
sh_ records new lc --from leetcode
sh_ records add lc --title "Two Sum"
printf '\nshimmer records complete lc/two-sum\n' >> "$W/steps/01-check.sh"
sh_ workspaces activate "$WS" --wait; rc=$?
sh_ records get lc/two-sum --json
done_=$(echo "$OUT" | grep -c '"status": *"done"')
if [ $rc -eq 0 ] && [ "$done_" -eq 1 ]; then
    pass K "the step completed lc/two-sum through the daemon"
else
    fail K "activate exit $rc, two-sum done: $done_"
fi
sh_ workspaces stop "$WS" --wait
kill_leftovers
restore

say "L (#34 item 10): scripts run through the interpreter, not the executable bit"
chmod -x "$W"/steps/*.sh
ls -l "$W/steps/" | sed 's/^/    /'
sh_ workspaces activate "$WS" --wait; rc=$?
s=$(state)
if [ $rc -eq 0 ] && [ "$s" = active ]; then
    pass L "activated with chmod -x on every step"
else
    fail L "exit $rc, state '$s'"
fi
sh_ workspaces stop "$WS" --wait
kill_leftovers
restore

say "M (#34 item 9): [env] can't override SHIMMER_* variables"
printf 'SHIMMER_HOME = "/tmp"\n' >> "$W/workspace.toml"
tail -4 "$W/workspace.toml" | sed 's/^/    /'
sh_ workspaces status "$WS"; status_out="$OUT"
sh_ workspaces activate "$WS" --wait; rc=$?
if echo "$status_out$OUT" | grep -q 'reserved' && [ $rc -ne 0 ]; then
    pass M "rejected: SHIMMER_ names are reserved; activate refused"
else
    fail M "activate exit $rc; no 'reserved' message"
fi
restore
s=$(state)
[ "$s" = ready ] || note "M: after putting the file back, state is '$s' (expected ready)"

# ---------------------------------------------------------------- tidy up and summary

say "Tidying up"
sh_ workspaces remove "$WS"
kill_leftovers
"$BIN" shutdown >/dev/null 2>&1
echo "left running afterwards:"; show_procs

say "Summary"
printf "$RESULTS"
echo "failures: $FAILS of 13"
[ -n "$NOTES" ] && printf "notes:\n$NOTES"
echo
echo "Full log: $LOG"
echo "Post it on issue #34."
