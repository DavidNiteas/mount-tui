#!/usr/bin/env bash
# Scenario: forced unmount in the default (udisksctl) mode.
#
# The USB partition is mounted with `m` (udisksctl), then force-unmounted with
# two `f` presses: the first one only arms the confirmation, the second one runs
# `udisksctl unmount -b /dev/sdb1 -f`. Nothing reaches sudo or a real device.
set -u

# shellcheck source=common.sh
. "$(cd "$(dirname "$0")" && pwd)/common.sh"

e2e_init force || exit 1
start_tui

send Down
send Down
send Down # sdb1
cap force_select
check "selected the usb partition" "Path:    /dev/sdb1" "$OUT/force_select.txt"

send m
sleep 0.6
cap force_mounted
check "udisksctl mount before the force unmount" "Mounted /dev/sdb1 at /media/e2e/USBKEY" "$OUT/force_mounted.txt"

send f
cap force_prompt
check "first f asks for the confirmation" "Press f again to force-unmount sdb1" "$OUT/force_prompt.txt"
check "the prompt warns about data loss" "lazy unmount may lose data" "$OUT/force_prompt.txt"
check_absent "the first press runs nothing" "udisksctl unmount" "$STATE/calls.log"

send f
sleep 0.6
cap force_done
check "forced unmount command" "udisksctl unmount -b /dev/sdb1 -f" "$STATE/calls.log"
check "forced unmount reported" "Force unmounted /dev/sdb1" "$OUT/force_done.txt"
check "the device is unmounted afterwards" "Mounted: No" "$OUT/force_done.txt"
check_absent "forced unmount never falls back to sudo" "SUDO" "$STATE/calls.log"

expect_exit

e2e_finish
