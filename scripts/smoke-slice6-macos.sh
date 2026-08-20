#!/bin/sh
set -eu
if [ "$(uname -s)" != Darwin ]; then
    echo "Slice 6 smoke is macOS-only" >&2
    exit 1
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${DOCK_SMOKE_PARENT:-/tmp}/dock-slice6.XXXXXX")
socket="$tmp/dock.sock"
state="$tmp/state"
repo="$tmp/repo"
daemon_pid=
cleanup() {
    if [ -n "$daemon_pid" ]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT INT TERM

cd "$root"
cargo build --quiet --bin dockd --bin dock-workspace
dockd="$root/target/debug/dockd"
workspace="$root/target/debug/dock-workspace"

git init -q "$repo"
git -C "$repo" config user.email dock@example.invalid
git -C "$repo" config user.name 'Dock Smoke'
mkdir -p "$repo/kanban/tasks"
printf '%s\n' \
    '---' \
    'id: TASK-6' \
    'title: Slice 6 workspace fixture' \
    'status: ready' \
    '---' \
    'Exercise dynamic workspace metadata without changing this task.' \
    >"$repo/kanban/tasks/TASK-6.md"
printf '%s\n' '{"task_id":"TASK-6","action":"create"}' >"$repo/kanban/activity.jsonl"
git -C "$repo" add kanban
git -C "$repo" commit -qm 'add source-controlled Slice 6 task fixture'
repo=$(cd "$repo" && pwd -P)
head_before=$(git -C "$repo" rev-parse HEAD)
status_before=$(git -C "$repo" status --porcelain --untracked-files=all)

start() {
    "$dockd" --socket="$socket" --state-dir="$state" >"$tmp/daemon.log" 2>&1 &
    daemon_pid=$!
    i=0
    while [ ! -S "$socket" ]; do
        i=$((i + 1))
        [ "$i" -lt 100 ] || { cat "$tmp/daemon.log" >&2; exit 1; }
        sleep 0.05
    done
}

cd "$repo"
start
"$workspace" --socket="$socket" create daily "Daily runtime" pane_one >/dev/null
"$workspace" --socket="$socket" split daily pane_one pane_two vertical >/dev/null
"$workspace" --socket="$socket" resize daily pane_two 650 >/dev/null
"$workspace" --socket="$socket" focus daily pane_one >/dev/null
"$workspace" --socket="$socket" rename-pane daily pane_one editor >/dev/null
kill "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=
rm -f "$socket"
start
output=$("$workspace" --socket="$socket" inspect)
echo "$output" | grep -q '"workspace_id": "daily"'
echo "$output" | grep -q '"runtime": "restored"'
! grep -Eq 'run_id|runtime|scrollback|transcript|process_group|command|/Users/' "$state/layout.json"
head_after=$(git -C "$repo" rev-parse HEAD)
status_after=$(git -C "$repo" status --porcelain --untracked-files=all)
[ "$head_after" = "$head_before" ]
[ "$status_after" = "$status_before" ]
echo "Slice 6 macOS layout/restart smoke passed"
