#!/usr/bin/env bash
# Scenario: `sudo` terminal handoff and the long CJK status message.
#
# udisksctl rejects /dev/sdz1, so the TUI suspends, runs the mock sudo on the
# normal screen and waits for Enter before resuming. The long CJK error and its
# END_OF_LONG_ERROR tail must both fit the dynamically sized status bar.
set -u

# shellcheck source=common.sh
. "$(cd "$(dirname "$0")" && pwd)/common.sh"

e2e_init sudo || exit 1
start_tui

send End # sdz1 is the last item
cap sudo_select
check "selected sudo-test partition" "Path:    /dev/sdz1" "$OUT/sudo_select.txt"

send m
sleep 0.8
cap sudo_prompt
# The TUI must be suspended: mock sudo output and the retry prompt are visible.
check "sudo ran on the normal screen" "MOCK SUDO" "$OUT/sudo_prompt.txt"
check "failure keeps error visible" "Press Enter to return to Mount-TUI" "$OUT/sudo_prompt.txt"
check "sudo mount invoked with safe options" "SUDO mount -o nosuid,nodev /dev/sdz1" "$STATE/calls.log"

send Enter
sleep 0.6
cap sudo_resumed
check "TUI restored after handoff" "Mount-TUI" "$OUT/sudo_resumed.txt"
check "status reports mount failure" "Mount failed for /dev/sdz1" "$OUT/sudo_resumed.txt"
check "long status text is not clipped" "END_OF_LONG_ERROR" "$OUT/sudo_resumed.txt"

expect_exit

e2e_finish
