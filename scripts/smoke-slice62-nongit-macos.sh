#!/bin/sh
set -eu
[ "$(uname -s)" = Darwin ] || { echo "Slice 6.2 smoke is macOS-only" >&2; exit 2; }
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${DOCK_SMOKE_PARENT:-/tmp}/dock-slice62.XXXXXX")
plain="$tmp/plain"
runtime="$tmp/runtime"
target="$tmp/target"
mkdir -m 700 "$plain" "$runtime" "$tmp/trap-bin"
git_log="$tmp/git-called"
daemon_pid=
cleanup() {
    status=$?
    trap - EXIT INT TERM
    if [ -z "$daemon_pid" ]; then daemon_pid=$(find "$runtime" -name dockd.sock -type s -exec lsof -t {} \; 2>/dev/null | head -1 || true); fi
    [ -z "$daemon_pid" ] || kill "$daemon_pid" 2>/dev/null || true
    [ "${DOCK_SMOKE_KEEP:-0}" = 1 ] || rm -rf "$tmp"
    exit "$status"
}
trap cleanup EXIT INT TERM

cd "$root"
CARGO_TARGET_DIR="$target" cargo build --quiet --bin dock
dock="$target/debug/dock"
printf '%s\n' '#!/bin/sh' "echo called >>'$git_log'" 'exit 97' >"$tmp/trap-bin/git"
chmod +x "$tmp/trap-bin/git"
export TMPDIR="$runtime"
export PATH="$tmp/trap-bin:$PATH"

run() {
    session=$1 keys=$2 result=$3 prior=${4:-} assert_shell=${5:-}
    set -- --dock "$dock" --keys "$keys" --transcript "$tmp/$session.typescript" \
        --error-log "$tmp/$session.stderr" --result "$result" --session "$session"
    [ -z "$prior" ] || set -- "$@" --prior-result "$prior"
    [ -z "$assert_shell" ] || set -- "$@" --assert-shell-pane
    python3 "$root/scripts/smoke-slice61-pty.py" "$@"
}

cd "$plain"
# `Ctrl+B` is the command prefix; unprefixed keys reach the focused pane's shell directly.
# `Ctrl+B n` creates the workspace, whose one pane auto-launches a shell with no explicit
# launch action; `--assert-shell-pane` proves that shell is genuinely running (not just the
# pre-attach placeholder). `Ctrl+B h`/`Ctrl+B v` split, `Ctrl+B l` opens the launch form on the
# newly focused split pane, and the fixture type-ahead/confirm/quit sequence matches Slice 6.1.
run first '<C-b>n<C-b>h<C-b>v<C-b>lfix<Enter><Enter><C-b>q' "$tmp/first.json" "" assert-shell
run second '<C-b>x<C-b>q' "$tmp/second.json" "$tmp/first.json"
[ ! -e "$git_log" ]
[ -z "$(find "$plain" -mindepth 1 -print -quit)" ]
socket=$(find "$runtime" -name dockd.sock -type s | head -1)
[ -n "$socket" ]
daemon_pid=$(lsof -t "$socket" | head -1)
grep -Fq TERMINAL_RESTORED "$tmp/first.typescript"
grep -Fq TERMINAL_RESTORED "$tmp/second.typescript"
echo "Slice 6.2 foreground non-Git dock-only PTY launch/reconnect/stop smoke passed"
