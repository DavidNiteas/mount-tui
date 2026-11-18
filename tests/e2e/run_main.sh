#!/usr/bin/env bash
# Scenario: system protection, multi-mount refusal, USB mount/unmount,
# scrolling and quit.
#
# Nothing here touches a real device: `lsblk`, `udisksctl` and `sudo` all come
# from tests/e2e/mock/bin.
set -u

# shellcheck source=common.sh
. "$(cd "$(dirname "$0")" && pwd)/common.sh"

e2e_init main || exit 1
start_tui

cap start
check "startup title" "Mount-TUI" "$OUT/start.txt"
check "help bar visible" "Force unmount" "$OUT/start.txt"
check "USB partition tagged" "\[USB\]" "$OUT/start.txt"
check "removable tagged" "\[RM\]" "$OUT/start.txt"
check "read-only tagged" "\[RO\]" "$OUT/start.txt"
check "label shown in list" "USBKEY" "$OUT/start.txt"
check "LVM member shown" "LVM2_member" "$OUT/start.txt"

send Down
cap select_sda1
check "system mount flagged in details" "System:  yes (protected)" "$OUT/select_sda1.txt"

send u
cap unmount_system
check "system unmount refused" "Refusing to unmount /home: system mount" "$OUT/unmount_system.txt"
send f
cap force_system
check "system force-unmount refused" "Refusing to unmount /home: system mount" "$OUT/force_system.txt"

check_absent "no udisksctl on protected mount" "udisksctl" "$STATE/calls.log"
check_absent "no sudo on protected mount" "SUDO" "$STATE/calls.log"

send Down # sdb
send Down # sdb1
send m
sleep 0.6
cap mount_usb
check "mount reports real udisksctl mountpoint" "Mounted /dev/sdb1 at /media/e2e/USBKEY" "$OUT/mount_usb.txt"
check "udisksctl mount invoked" "udisksctl mount -b /dev/sdb1" "$STATE/calls.log"
check "mounted device state visible" "/media/e2e/USBKEY" "$OUT/mount_usb.txt"

send u
sleep 0.6
cap unmount_usb
check "unmount succeeds" "Unmounted /dev/sdb1" "$OUT/unmount_usb.txt"

send Down # sdc
send Down # sdc1
send Enter
cap lvm_enter
check "Enter explains non-mountable device" "LVM2_member" "$OUT/lvm_enter.txt"

send Down # sdx
send Down # sdx1 (mounted at /home/e2e)
send u
cap unmount_home_dir
check "per-user home mount refused" "Refusing to unmount /home/e2e: system mount" "$OUT/unmount_home_dir.txt"
send f
cap force_home_dir
check "per-user home force-unmount refused" "Refusing to unmount /home/e2e: system mount" "$OUT/force_home_dir.txt"
check_absent "no udisksctl on home mount" "udisksctl unmount -b /dev/sdx1" "$STATE/calls.log"
check_absent "no sudo on home mount" "sdx1" "$STATE/calls.log"

send Down # sdy
send Down # sdy1 (two mount points)
cap multi_details
check "multi-mount details first" "/mnt/multi1" "$OUT/multi_details.txt"
check "multi-mount details second" "/mnt/multi2" "$OUT/multi_details.txt"
send u
cap multi_unmount
check "multi-mount unmount refused" "has 2 mount points" "$OUT/multi_unmount.txt"
send f
cap multi_force
check "multi-mount force-unmount refused" "has 2 mount points" "$OUT/multi_force.txt"
check_absent "no udisksctl on multi-mount" "sdy1" "$STATE/calls.log"
check_absent "no sudo on multi-mount" "sdy1" "$STATE/calls.log"

send End
cap scroll_end
check "End scrolls to last device" "Path:    /dev/sdz1" "$OUT/scroll_end.txt"
send Home
cap scroll_home
check "Home returns to first device" "Path:    /dev/sda" "$OUT/scroll_home.txt"

expect_exit

e2e_finish
