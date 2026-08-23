#!/bin/sh
set -eu

[ "$(uname -s)" = Darwin ] || { echo "Slice 6.1 smoke is macOS-only" >&2; exit 2; }
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${DOCK_SMOKE_PARENT:-/tmp}/dock-slice61.XXXXXX")
repo="$tmp/repo"
target="$tmp/target"
runtime_base="$tmp/runtime"
mkdir -m 700 "$runtime_base"
export TMPDIR="$runtime_base"
socket=
state=
first_log="$tmp/first.typescript"
second_log="$tmp/second.typescript"
first_error="$tmp/first.stderr"
second_error="$tmp/second.stderr"
first_result="$tmp/first.result.json"
second_result="$tmp/second.result.json"
daemon_pid=
# Finds the daemon holding this smoke run's socket.
#
# Dock canonicalises the socket path before spawning the daemon, so on macOS the daemon's argv
# says /private/tmp/... while this script holds /tmp/... — the same file through a symlink, but
# neither lsof nor pgrep matches across it. Searching the unresolved path found nothing, the
# caller's [ -n "$daemon_pid" ] guard skipped the whole kill, and every run of this script left a
# daemon holding PTYs behind while reporting success.
find_daemon() {
    resolved=$(cd "$(dirname "$1")" 2>/dev/null && pwd -P)/$(basename "$1")
    lsof -t "$resolved" 2>/dev/null | head -1 && return 0
    pgrep -f "dockd --socket=$resolved" 2>/dev/null | head -1
}

cleanup() {
    incoming_status=$?
    trap - EXIT INT TERM
    cleanup_status=0
    if [ -z "$daemon_pid" ] && [ -S "$socket" ]; then
        daemon_pid=$(find_daemon "$socket" || true)
    fi
    if [ -n "$daemon_pid" ]; then
        if kill -0 "$daemon_pid" 2>/dev/null; then
            kill "$daemon_pid" 2>/dev/null || cleanup_status=1
        fi
        attempts=0
        while kill -0 "$daemon_pid" 2>/dev/null && [ "$attempts" -lt 100 ]; do
            sleep 0.05
            attempts=$((attempts + 1))
        done
        if kill -0 "$daemon_pid" 2>/dev/null; then
            printf 'spawned dockd %s did not exit after termination\n' "$daemon_pid" >&2
            cleanup_status=1
        fi
        # SIGTERM cannot run Rust destructors. Once the exact daemon is gone, retire its stale
        # filesystem name and then independently assert that cleanup completed.
        if ! kill -0 "$daemon_pid" 2>/dev/null && { [ -e "$socket" ] || [ -L "$socket" ]; }; then
            rm -f "$socket" || cleanup_status=1
        fi
        if [ -e "$socket" ] || [ -L "$socket" ]; then
            printf 'spawned dockd left socket path behind: %s\n' "$socket" >&2
            cleanup_status=1
        fi
    fi
    if [ "${DOCK_SMOKE_KEEP:-0}" = 1 ]; then
        printf 'Preserved Slice 6.1 smoke artifacts at %s\n' "$tmp" >&2
    else
        rm -rf "$tmp" || cleanup_status=1
    fi
    [ "$incoming_status" -ne 0 ] && exit "$incoming_status"
    exit "$cleanup_status"
}
trap cleanup EXIT INT TERM

git init -q "$repo"
git -C "$repo" config user.email dock@example.invalid
git -C "$repo" config user.name 'Dock Smoke'
printf 'fixture\n' >"$repo/tracked"
printf '/.dock/local/\n' >"$repo/.gitignore"
git -C "$repo" add tracked .gitignore
git -C "$repo" commit -qm fixture
head_before=$(git -C "$repo" rev-parse HEAD)
tree_before=$(git -C "$repo" write-tree)
index_before=$(cksum "$repo/.git/index")
history_before=$(git -C "$repo" rev-list --all)
status_before=$(git -C "$repo" status --porcelain --untracked-files=all)

cd "$root"
CARGO_TARGET_DIR="$target" cargo build --quiet --bin dock
dock="$target/debug/dock"
[ ! -e "$target/debug/dockd" ]

run_pty() {
    log=$1
    error_log=$2
    keys=$3
    session=$4
    result=$5
    prior_result=${6:-}
    set -- --dock "$dock" --keys "$keys" \
        --transcript "$log" --error-log "$error_log" --session "$session" --result "$result"
    [ -z "$prior_result" ] || set -- "$@" --prior-result "$prior_result"
    if python3 "$root/scripts/smoke-slice61-pty.py" \
        "$@"; then
        return 0
    else
        status=$?
        printf 'foreground Dock PTY session failed (keys=%s, status=%s)\n' "$keys" "$status" >&2
        [ ! -s "$error_log" ] || sed 's/^/dock stderr: /' "$error_log" >&2
        printf 'PTY transcript: %s\nHarness diagnostic: %s\n' "$log" "$error_log" >&2
        return "$status"
    fi
}

# `dock` is the only product command. It bootstraps dockd, enters the foreground dashboard,
# creates a workspace, and launches a Dock-owned fixture via the same UI actions a user sees.
cd "$repo"
# `Ctrl+B` is the command prefix; unprefixed keys go straight to the focused pane's shell.
# `Ctrl+B n` creates the workspace and `Ctrl+B l` opens the bounded launch form; type-ahead
# inside that form selects Fixture, the first Enter reviews the exact mode/profile/target, and
# the second explicitly confirms that visible choice. `Ctrl+B q` quits.
run_pty "$first_log" "$first_error" '<C-b>n<C-b>lfix<Enter><Enter><C-b>q' first "$first_result"
socket=$(find "$runtime_base" -name dockd.sock -type s | head -1)
[ -n "$socket" ]
[ "$(stat -f '%Lp' "$socket")" = 600 ]
[ "$(stat -f '%Lp' "$(dirname "$socket")")" = 700 ]
[ ! -e "$repo/.dock" ]
state="$(dirname "$socket")/state"
[ "$(stat -f '%Lp' "$state")" = 700 ]
daemon_pid=$(find_daemon "$socket")

# Reconnect through the dashboard again; the persisted workspace must be visible there.
run_pty "$second_log" "$second_error" '<C-b>q' second "$second_result" "$first_result"

for log in "$first_log" "$second_log"; do
    grep -Fq 'TERMINAL_RESTORED' "$log"
done
[ -s "$first_result" ]
[ -s "$second_result" ]
[ "$(git -C "$repo" rev-parse HEAD)" = "$head_before" ]
[ "$(git -C "$repo" write-tree)" = "$tree_before" ]
[ "$(cksum "$repo/.git/index")" = "$index_before" ]
[ "$(git -C "$repo" rev-list --all)" = "$history_before" ]
[ "$(git -C "$repo" status --porcelain --untracked-files=all)" = "$status_before" ]
echo "Slice 6.1 interactive dock-only bootstrap, workspace, dispatch, and terminal-restoration smoke passed"
