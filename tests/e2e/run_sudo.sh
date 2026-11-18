#!/usr/bin/env bash
# Scenario: explicit sudo mode (`MOUNT_TUI_MODE=sudo`).
#
# The mount point is `<MOUNT_TUI_MOUNT_ROOT>/<label-or-name>`, created right
# before the terminal handoff and removed again when the mount fails. A second
# attempt with a succeeding `sudo mount` keeps it and reports the real mount
# point. The mounted device is then unmounted (`u`), mounted again and finally
# force-unmounted (`f` `f` → `sudo umount -l`). udisksctl is never consulted in
# this mode.
set -u

# shellcheck source=common.sh
. "$(cd "$(dirname "$0")" && pwd)/common.sh"

e2e_init sudo || exit 1
# sudo mode is an explicit opt-in; the root comes from the environment here.
export MOUNT_TUI_MODE=sudo
export MOUNT_TUI_MOUNT_ROOT="$HOME_DIR/mnt/usb"
TARGET="$MOUNT_TUI_MOUNT_ROOT/sdz1"
start_tui

send End # sdz1 is the last item
cap sudo_select
check "selected sudo-test partition" "Path:    /dev/sdz1" "$OUT/sudo_select.txt"

# --- failing `sudo mount` ------------------------------------------------
send m
sleep 0.8
cap sudo_prompt
# The TUI is suspended: the mock sudo output and the retry prompt are visible.
check "sudo ran on the normal screen" "MOCK SUDO" "$OUT/sudo_prompt.txt"
check "failure keeps the error visible" "Press Enter to return to Mount-TUI" "$OUT/sudo_prompt.txt"
check "sudo mount invoked with safe options" "SUDO mount -o nosuid,nodev /dev/sdz1" "$STATE/calls.log"
check "mount target lives under the configured root" "mnt/usb/sdz1" "$STATE/calls.log"
check_absent "no udisksctl attempt in sudo mode" "udisksctl" "$STATE/calls.log"
check_path "mount point created before the handoff" "$TARGET"

send Enter
sleep 0.6
cap sudo_resumed
check "TUI restored after handoff" "Mount-TUI" "$OUT/sudo_resumed.txt"
check "status reports mount failure" "Mount failed for /dev/sdz1" "$OUT/sudo_resumed.txt"
check_no_path "failed mount leaves no mount point behind" "$TARGET"

# --- succeeding `sudo mount` ---------------------------------------------
touch "$STATE/sudo_ok"
send m
sleep 0.8
cap sudo_mounted
check "successful mount reports the real mount point" "Mounted /dev/sdz1 at" "$OUT/sudo_mounted.txt"
check "the configured root is reported" "mnt/usb/sdz1" "$OUT/sudo_mounted.txt"
check_path "successful mount keeps the mount point" "$TARGET"

# --- succeeding `sudo umount` --------------------------------------------
touch "$STATE/sudo_umount_ok"
send u
sleep 0.8
cap sudo_unmounted
check "sudo umount invoked" "SUDO umount /dev/sdz1" "$STATE/calls.log"
check "unmount reports success" "Unmounted /dev/sdz1" "$OUT/sudo_unmounted.txt"
check_no_path "unmounted device has no mount point any more" "$STATE/sdz1_mp"

# --- mounting the same device again --------------------------------------
send m
sleep 0.8
cap sudo_remounted
check "second sudo mount reports the mount point" "Mounted /dev/sdz1 at" "$OUT/sudo_remounted.txt"
check_path "the mount point is reused" "$TARGET"

# --- sudo force unmount (`umount -l`) ------------------------------------
send f
cap sudo_force_prompt
check "first f asks for the confirmation" "Press f again to force-unmount sdz1" "$OUT/sudo_force_prompt.txt"
send f
sleep 0.8
cap sudo_forced
check "sudo force unmount uses umount -l" "SUDO umount -l /dev/sdz1" "$STATE/calls.log"
check "force unmount reports success" "Force unmounted /dev/sdz1" "$OUT/sudo_forced.txt"
check_no_path "force unmount clears the mount point" "$STATE/sdz1_mp"

expect_exit

e2e_finish
