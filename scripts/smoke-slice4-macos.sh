#!/bin/sh
set -eu

[ "$(uname -s)" = Darwin ] || { echo "this smoke path targets macOS" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required for this compact smoke" >&2; exit 2; }
smoke_parent=${DOCK_SMOKE_PARENT:-/tmp}
smoke_dir=$(mktemp -d "$smoke_parent/dock-slice4.XXXXXX")
socket="$smoke_dir/d.sock"
state="$smoke_dir/state"
daemon_pid=
cleanup() {
    [ -z "$daemon_pid" ] || kill "$daemon_pid" 2>/dev/null || true
    rm -rf "$smoke_dir"
}
trap cleanup EXIT INT TERM

cargo build --bins >/dev/null
target/debug/dockd --socket="$socket" --state-dir="$state" &
daemon_pid=$!
while [ ! -S "$socket" ]; do sleep 0.05; done
root="$smoke_dir/repo"
git init -q "$root"
git -C "$root" config user.email dock@example.invalid
git -C "$root" config user.name 'Dock Smoke'
printf 'fixture\n' >"$root/tracked"
git -C "$root" add tracked
git -C "$root" commit -qm fixture
root=$(cd "$root" && pwd -P)

# Missing discovery must precede every durable/runtime artifact.
if target/debug/dock dispatch --socket="$socket" --repo="$root" --task=SMOKE-MISSING \
    --run-id=dock_missing --worktree="$root" --adapter=generic \
    --executable=/definitely/not/a/dock-agent >/dev/null 2>&1; then
    echo "missing adapter unexpectedly launched" >&2; exit 1
fi
[ ! -e "$state/dispatches/dock_missing.json" ]

target/debug/dock dispatch --socket="$socket" --repo="$root" --task=SMOKE-4 \
    --run-id=dock_smoke4 --worktree="$root" --adapter=fixture -- -c 'sleep 30' |
    jq -e '.adapter == "fixture" and .provider_state == "unknown"' >/dev/null
target/debug/dock agent --socket="$socket" --run-id=dock_smoke4 --operation=focus >/dev/null
target/debug/dock agent --socket="$socket" --run-id=dock_smoke4 --operation=interrupt >/dev/null
target/debug/dock agent --socket="$socket" --run-id=dock_smoke4 --operation=restart |
    jq -e '.state == "running"' >/dev/null
target/debug/dock agent --socket="$socket" --run-id=dock_smoke4 --operation=stop >/dev/null
echo "Slice 4 fixture adapter lifecycle smoke passed"
