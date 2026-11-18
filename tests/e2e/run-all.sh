#!/usr/bin/env bash
# Run every mount-tui end-to-end scenario; exit 0 only when all of them pass.
#
# Requirements: a built debug binary (cargo build --offline) and tmux.
# Mock `lsblk` / `udisksctl` / `sudo` come from tests/e2e/mock*/bin, so no real
# device, mount or sudo prompt is ever touched.
set -u

E2E_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(git -C "$E2E_DIR" rev-parse --show-toplevel 2>/dev/null || true)"
BIN="${MOUNT_TUI_BIN:-$ROOT/target/debug/mount-tui}"

if ! command -v tmux >/dev/null 2>&1; then
    echo "FAIL: tmux is required to run the end-to-end scenarios" >&2
    exit 1
fi
if [ ! -x "$BIN" ]; then
    echo "FAIL: $BIN not found; run 'cargo build --offline' first" >&2
    exit 1
fi

fail=0
for scenario in run_main.sh run_hotplug.sh run_legacy.sh run_udisks_fail.sh run_sudo.sh run_force.sh run_search.sh; do
    echo "=== ${scenario%.sh} ==="
    if ! bash "$E2E_DIR/$scenario"; then
        fail=1
    fi
done

if [ "$fail" -ne 0 ]; then
    echo "E2E: FAIL"
    exit 1
fi
echo "E2E: all scenarios PASS"
exit 0
