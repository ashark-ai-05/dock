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
socket="$runtime_base/dock-$(id -u)/dockd.sock"
state="$repo/.dock/local"
first_log="$tmp/first.typescript"
second_log="$tmp/second.typescript"
first_error="$tmp/first.stderr"
second_error="$tmp/second.stderr"
first_result="$tmp/first.result.json"
second_result="$tmp/second.result.json"
daemon_pid=
cleanup() {
    incoming_status=$?
    trap - EXIT INT TERM
    cleanup_status=0
    if [ -z "$daemon_pid" ] && [ -S "$socket" ]; then
        daemon_pid=$(lsof -t "$socket" 2>/dev/null | head -1 || true)
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
run_pty "$first_log" "$first_error" nlq first "$first_result"
[ -S "$socket" ]
[ "$(stat -f '%Lp' "$socket")" = 600 ]
[ "$(stat -f '%Lp' "$(dirname "$socket")")" = 700 ]
[ "$(stat -f '%Lp' "$repo/.dock")" = 700 ]
[ "$(stat -f '%Lp' "$state")" = 700 ]
while IFS= read -r directory; do
    [ "$(stat -f '%Lp' "$directory")" = 700 ]
done <<EOF
$(find "$state" -type d -print)
EOF
git -C "$repo" check-ignore -q .dock/local/layout.json
daemon_pid=$(lsof -t "$socket" | head -1)

# Reconnect through the dashboard again; the persisted workspace must be visible there.
run_pty "$second_log" "$second_error" q second "$second_result" "$first_result"

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
