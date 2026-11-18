#!/usr/bin/env bash
# Scenario: the default (udisksctl) mode has no sudo fallback and no side
# effects at all.
#
# udisksctl rejects /dev/sdz1. Pressing `m` must report the error inside the
# TUI, must never hand the terminal to sudo and must never create `$HOME/mnt`.
set -u

# shellcheck source=common.sh
. "$(cd "$(dirname "$0")" && pwd)/common.sh"

e2e_init udisks_fail || exit 1
start_tui

send End # sdz1 is the last item
cap fail_select
check "selected failing partition" "Path:    /dev/sdz1" "$OUT/fail_select.txt"

send m
sleep 0.8
cap fail_status
check "TUI never suspends" "Mount-TUI" "$OUT/fail_status.txt"
check "mount failure reported" "Mount failed for /dev/sdz1" "$OUT/fail_status.txt"
check "long CJK error tail not clipped" "END_OF_LONG_ERROR" "$OUT/fail_status.txt"
check_absent "no sudo handoff prompt" "Press Enter to return to Mount-TUI" "$OUT/fail_status.txt"
check "udisksctl mount attempted" "udisksctl mount -b /dev/sdz1" "$STATE/calls.log"
check_absent "no sudo fallback" "SUDO" "$STATE/calls.log"
check_no_path "no \$HOME/mnt directory created" "$HOME_DIR/mnt"

expect_exit

e2e_finish
