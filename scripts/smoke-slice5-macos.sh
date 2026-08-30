#!/bin/sh
set -eu

[ "$(uname -s)" = Darwin ] || { echo "this smoke path targets macOS" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required for this compact smoke" >&2; exit 2; }
smoke_parent=${DOCK_SMOKE_PARENT:-/tmp}
smoke_dir=$(mktemp -d "$smoke_parent/dock-slice5.XXXXXX")
socket="$smoke_dir/d.sock"
state="$smoke_dir/state"
daemon_pid=
cleanup() {
    [ -z "$daemon_pid" ] || kill "$daemon_pid" 2>/dev/null || true
    rm -rf "$smoke_dir"
}
trap cleanup EXIT INT TERM

cargo build --bins >/dev/null
target/debug/dockd --socket="$socket" --state-dir="$state" \
    --global-run-capacity=3 --repository-run-capacity=1 --human-review-reserved=1 &
daemon_pid=$!
while [ ! -S "$socket" ]; do sleep 0.05; done

make_repo() {
    repo=$1
    git init -q "$repo"
    git -C "$repo" config user.email dock@example.invalid
    git -C "$repo" config user.name 'Dock Smoke'
    printf 'fixture\n' >"$repo/tracked"
    git -C "$repo" add tracked
    git -C "$repo" commit -qm fixture
}
repo_a="$smoke_dir/repo-a"
repo_b="$smoke_dir/repo-b"
make_repo "$repo_a"
make_repo "$repo_b"
repo_a=$(cd "$repo_a" && pwd -P)
repo_b=$(cd "$repo_b" && pwd -P)
head_a=$(git -C "$repo_a" rev-parse HEAD)
head_b=$(git -C "$repo_b" rev-parse HEAD)
status_a=$(git -C "$repo_a" status --porcelain)
status_b=$(git -C "$repo_b" status --porcelain)

upstream=$(target/debug/dock-dispatch --socket="$socket" --repo="$repo_a" --task=A-1 \
    --run-id=dock_upstream --worktree="$repo_a" --adapter=fixture -- -c 'sleep 30')
if target/debug/dock-dispatch --socket="$socket" --repo="$repo_a" --task=A-2 \
    --run-id=dock_repo_refused --worktree="$repo_a" --adapter=fixture -- -c 'sleep 30' >/dev/null 2>&1; then
    echo "per-repository capacity unexpectedly admitted a second run" >&2; exit 1
fi
[ ! -e "$state/dispatches/dock_repo_refused.json" ]
target/debug/dock-dispatch --socket="$socket" --repo="$repo_b" --task=B-0 \
    --run-id=dock_blocker --worktree="$repo_b" --adapter=fixture -- -c 'sleep 30' >/dev/null
if target/debug/dock-dispatch --socket="$socket" --repo="$repo_b" --task=B-2 \
    --run-id=dock_global_refused --worktree="$repo_b" --adapter=fixture -- -c 'sleep 30' >/dev/null 2>&1; then
    echo "global capacity unexpectedly admitted a second run" >&2; exit 1
fi
[ ! -e "$state/dispatches/dock_global_refused.json" ]

target/debug/dock-programme --socket="$socket" --upstream-run-id=dock_upstream \
    --required-route=accept-scope --repo="$repo_b" --task=B-1 \
    --run-id=dock_downstream --worktree="$repo_b" |
    jq -e '.state == "awaiting_handoff"' >/dev/null
target/debug/dock-programme --socket="$socket" | jq -e '
    .global_active == 2 and
    ([.repositories[].active_run_ids[]] | sort) == ["dock_blocker", "dock_upstream"] and
    ([.repositories[].queued_run_ids[]] | sort) == ["dock_downstream"] and
    ([.repositories[].active_capacity] | sort) == [1, 1]' >/dev/null
if target/debug/dock-dispatch --socket="$socket" --repo="$repo_b" --task=B-1 \
    --run-id=dock_downstream --worktree="$repo_b" --adapter=fixture >/dev/null 2>&1; then
    echo "direct dispatch bypassed a queued dependency gate" >&2; exit 1
fi
if target/debug/dock-programme --socket="$socket" --release=dock_downstream >/dev/null 2>&1; then
    echo "gate released without handoff" >&2; exit 1
fi

printf '%s\n' "$upstream" | jq '{schema_version:1,run_id,task_id:.external_task_ref,workspace_id,pane_id,worktree,branch,base_sha,summary:"explicit fixture handoff",question:"release downstream?",checks:[{name:"fixture check",passed:true}]}' >"$smoke_dir/handoff.json"
target/debug/dock-handoff --socket="$socket" --submit="$smoke_dir/handoff.json" >/dev/null
target/debug/dock-programme --socket="$socket" | jq -e '.gates[0].state == "awaiting_decision"' >/dev/null
target/debug/dock-handoff --socket="$socket" --run-id=dock_upstream --route=accept-scope --note='release declared edge' >/dev/null
target/debug/dock-agent --socket="$socket" --run-id=dock_upstream --operation=stop >/dev/null
target/debug/dock-agent --socket="$socket" --run-id=dock_blocker --operation=stop >/dev/null
wait_terminal() {
    stopped_run=$1
    attempts=0
    while [ "$attempts" -lt 100 ]; do
        state=$(target/debug/dock inspect --socket="$socket" --run-id="$stopped_run" | jq -r '.state | if type == "string" then . else keys[0] end')
        [ "$state" = exited ] && return 0
        attempts=$((attempts + 1))
        sleep 0.05
    done
    echo "run $stopped_run did not reach terminal state" >&2
    exit 1
}
wait_terminal dock_upstream
wait_terminal dock_blocker
target/debug/dock-programme --socket="$socket" | jq -e '
    .global_active == 0 and
    ([.repositories[].active_run_ids[]] | sort) == [] and
    ([.repositories[].queued_run_ids[]] | sort) == ["dock_downstream"] and
    ([.repositories[].active_capacity] | sort) == [0]' >/dev/null
target/debug/dock-programme --socket="$socket" --release=dock_downstream | jq -e '.run_id == "dock_downstream" and .state == "running"' >/dev/null
target/debug/dock-programme --socket="$socket" | jq -e '
    .global_active == 1 and
    ([.repositories[].active_run_ids[]] | sort) == ["dock_downstream"] and
    ([.repositories[].queued_run_ids[]] | sort) == [] and
    ([.repositories[].active_capacity] | sort) == [1] and
    (.gates | length) == 0' >/dev/null
target/debug/dock-agent --socket="$socket" --run-id=dock_downstream --operation=stop >/dev/null
wait_terminal dock_downstream
target/debug/dock-programme --socket="$socket" | jq -e '
    .global_active == 0 and
    ([.repositories[].active_run_ids[]] | sort) == [] and
    ([.repositories[].queued_run_ids[]] | sort) == [] and
    ([.repositories[].active_capacity] | sort) == []' >/dev/null
[ "$(git -C "$repo_a" rev-parse HEAD)" = "$head_a" ]
[ "$(git -C "$repo_b" rev-parse HEAD)" = "$head_b" ]
[ "$(git -C "$repo_a" status --porcelain)" = "$status_a" ]
[ "$(git -C "$repo_b" status --porcelain)" = "$status_b" ]
echo "Slice 5 two-repository global/per-repository capacity and programme gate smoke passed"
