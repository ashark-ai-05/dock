#!/bin/sh
set -eu

[ "$(uname -s)" = Darwin ] || { echo "this smoke path targets macOS" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required for this compact smoke" >&2; exit 2; }

smoke_parent=${DOCK_SMOKE_PARENT:-/tmp}
smoke_dir=$(mktemp -d "$smoke_parent/dock-slice3.XXXXXX")
socket="$smoke_dir/d.sock"
state="$smoke_dir/state"
packet="$smoke_dir/handoff.json"
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
branch=$(git -C "$root" branch --show-current)
base=$(git -C "$root" rev-parse HEAD)
target/debug/dock dispatch --socket="$socket" --repo="$root" --task=SMOKE-3 \
    --run-id=dock_smoke3 --worktree="$root" --adapter=fixture -- -c 'sleep 30' >/dev/null
jq -n --arg root "$root" --arg branch "$branch" --arg base "$base" '{
  schema_version: 2, run_id: "dock_smoke3", task_id: "SMOKE-3",
  workspace_id: "workspace-dock_smoke3", pane_id: "pane-dock_smoke3",
  worktree: $root, branch: $branch, base_sha: $base,
  summary: "Slice 3 smoke handoff.", question: "Accept smoke scope?",
  checks: ["fixture smoke"]
}' >"$packet"

target/debug/dock review --socket="$socket" --submit="$packet" >/dev/null
target/debug/dock review --socket="$socket" --inbox
target/debug/dock review --socket="$socket" --run-id=dock_smoke3 \
    --route=accept-scope --note='Smoke scope accepted; no Git or task mutation.'
target/debug/dock review --socket="$socket" --inbox
