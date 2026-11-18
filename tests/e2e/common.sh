#!/usr/bin/env bash
# Shared helpers for the mount-tui end-to-end scenarios.
#
# Every scenario sources this file, calls `e2e_init <name>` and then uses
# `start_tui` / `send` / `cap` / `check` / `check_absent` / `expect_exit`.
#
# All mutable state lives in a `mktemp -d` directory and is removed on exit:
# the e2e run never writes into the repository.

set -u

E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(git -C "$E2E_DIR" rev-parse --show-toplevel 2>/dev/null || true)"
BIN="${MOUNT_TUI_BIN:-$ROOT/target/debug/mount-tui}"

# Rows / columns of the fake terminal. Wide enough for the details pane and the
# long CJK status messages.
E2E_ROWS="${E2E_ROWS:-40}"
E2E_COLS="${E2E_COLS:-120}"

# Delay after every key press / after starting the TUI.
E2E_KEY_DELAY="${E2E_KEY_DELAY:-0.4}"
E2E_START_DELAY="${E2E_START_DELAY:-1}"

CHECKS=0
FAILURES=0

e2e_init() {
    local scenario="$1"

    if ! command -v tmux >/dev/null 2>&1; then
        echo "FAIL: tmux is required to run the end-to-end scenarios" >&2
        return 1
    fi
    if [ -z "$ROOT" ]; then
        echo "FAIL: tests/e2e must live inside the mount-tui git repository" >&2
        return 1
    fi
    if [ ! -x "$BIN" ]; then
        echo "FAIL: $BIN not found; run 'cargo build --offline' first" >&2
        return 1
    fi

    TMP="$(mktemp -d "${TMPDIR:-/tmp}/mount-tui-e2e.XXXXXX")"
    STATE="$TMP/state"
    HOME_DIR="$TMP/home"
    OUT="$TMP/out"
    # `$HOME_DIR/mnt` is deliberately *not* created: the default mode must never
    # need it, and sudo mode has to create its own root.
    mkdir -p "$STATE" "$HOME_DIR" "$OUT"
    : >"$STATE/calls.log"
    : >"$STATE/legacy.log"

    # Session names carry the PID so parallel runs never collide.
    SESS="mtui_${scenario}_$$"
    trap 'e2e_cleanup' EXIT
}

e2e_cleanup() {
    tmux kill-session -t "$SESS" 2>/dev/null || true
    rm -rf "$TMP"
}

# Start the TUI inside tmux. `$1` optionally prefixes PATH (used by the legacy
# scenario to put the old-lsblk mock first).
#
# `MOUNT_TUI_MODE` / `MOUNT_TUI_MOUNT_ROOT` are forwarded when the scenario
# exports them; otherwise the process starts with the built-in defaults. The
# caller's own configuration is never inherited (HOME is redirected, and
# XDG_CONFIG_HOME would point at a real config file).
start_tui() {
    local path_prefix="${1:-}"

    tmux kill-session -t "$SESS" 2>/dev/null || true

    cat >"$TMP/launch.sh" <<EOF
#!/bin/sh
export PATH="${path_prefix:+$path_prefix:}$E2E_DIR/mock/bin:\$PATH"
export HOME="$HOME_DIR"
export MOCK_STATE="$STATE"
unset XDG_CONFIG_HOME
unset MOUNT_TUI_MODE
unset MOUNT_TUI_MOUNT_ROOT
${MOUNT_TUI_MODE:+export MOUNT_TUI_MODE="$MOUNT_TUI_MODE"}
${MOUNT_TUI_MOUNT_ROOT:+export MOUNT_TUI_MOUNT_ROOT="$MOUNT_TUI_MOUNT_ROOT"}
exec "$BIN"
EOF
    chmod +x "$TMP/launch.sh"

    tmux new-session -d -s "$SESS" -x "$E2E_COLS" -y "$E2E_ROWS" "$TMP/launch.sh"
    sleep "$E2E_START_DELAY"
}

cap() {
    tmux capture-pane -p -t "$SESS" >"$OUT/$1.txt"
}

send() {
    tmux send-keys -t "$SESS" "$@"
    sleep "$E2E_KEY_DELAY"
}

check() { # name pattern file
    CHECKS=$((CHECKS + 1))
    if grep -q -- "$2" "$3" 2>/dev/null; then
        echo "PASS: $1"
    else
        echo "FAIL: $1 (pattern '$2' not found in $3)"
        FAILURES=$((FAILURES + 1))
    fi
}

check_absent() { # name pattern file
    CHECKS=$((CHECKS + 1))
    if grep -q -- "$2" "$3" 2>/dev/null; then
        echo "FAIL: $1 (unexpected '$2' in $3)"
        FAILURES=$((FAILURES + 1))
    else
        echo "PASS: $1"
    fi
}

check_count_at_least() { # name pattern minimum file
    CHECKS=$((CHECKS + 1))
    local count
    count="$(grep -c -- "$2" "$4" 2>/dev/null || true)"
    if [ "${count:-0}" -ge "$3" ]; then
        echo "PASS: $1"
    else
        echo "FAIL: $1 (expected >= $3 '$2' lines in $4, got ${count:-0})"
        FAILURES=$((FAILURES + 1))
    fi
}

check_no_path() { # name path
    CHECKS=$((CHECKS + 1))
    if [ -e "$2" ] || [ -L "$2" ]; then
        echo "FAIL: $1 ($2 must not exist)"
        FAILURES=$((FAILURES + 1))
    else
        echo "PASS: $1"
    fi
}

check_path() { # name directory
    CHECKS=$((CHECKS + 1))
    if [ -d "$2" ]; then
        echo "PASS: $1"
    else
        echo "FAIL: $1 (missing directory $2)"
        FAILURES=$((FAILURES + 1))
    fi
}

# Quit with `q` and verify the process really left the terminal.
expect_exit() {
    CHECKS=$((CHECKS + 1))
    send q
    sleep 0.4
    if tmux has-session -t "$SESS" 2>/dev/null; then
        echo "FAIL: TUI did not exit on q"
        FAILURES=$((FAILURES + 1))
        tmux kill-session -t "$SESS" 2>/dev/null || true
    else
        echo "PASS: TUI exits on q"
    fi
}

e2e_finish() {
    echo "--- $0: $CHECKS checks, $FAILURES failure(s) ---"
    [ "$FAILURES" -eq 0 ]
}
