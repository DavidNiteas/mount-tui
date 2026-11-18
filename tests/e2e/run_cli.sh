#!/usr/bin/env bash
# Scenario: the non-interactive CLI (`list`, `status`, `mount`, `unmount`,
# `force-unmount`) driven without a terminal.
#
# No tmux, no real device and no real privilege escalation: `lsblk`,
# `udisksctl` and `sudo` all come from tests/e2e/mock/bin, the state lives in a
# temporary `MOCK_STATE` directory and `HOME` points at a scratch directory.
set -u

E2E_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(git -C "$E2E_DIR" rev-parse --show-toplevel 2>/dev/null || true)"
BIN="${MOUNT_TUI_BIN:-$ROOT/target/debug/mount-tui}"

if [ -z "$ROOT" ]; then
    echo "FAIL: tests/e2e must live inside the mount-tui git repository" >&2
    exit 1
fi
if [ ! -x "$BIN" ]; then
    echo "FAIL: $BIN not found; run 'cargo build --offline' first" >&2
    exit 1
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/mount-tui-cli.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

STATE="$TMP/state"
HOME_DIR="$TMP/home"
OUT="$TMP/out"
ERR="$TMP/err"
mkdir -p "$STATE" "$HOME_DIR" "$OUT" "$ERR"
: >"$STATE/calls.log"

# Never inherit the caller's configuration: HOME is redirected, the TUI mode
# variables are cleared (the sudo section sets them explicitly) and
# XDG_CONFIG_HOME would point at a real config file.
unset MOUNT_TUI_MODE MOUNT_TUI_MOUNT_ROOT XDG_CONFIG_HOME

CHECKS=0
FAILURES=0

# Run the binary with the mock PATH and the scratch HOME/state.
run_cli() {
    HOME="$HOME_DIR" MOCK_STATE="$STATE" PATH="$E2E_DIR/mock/bin:$PATH" "$BIN" "$@"
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

check_code() { # name expected actual
    CHECKS=$((CHECKS + 1))
    if [ "$3" -eq "$2" ]; then
        echo "PASS: $1"
    else
        echo "FAIL: $1 (expected exit $2, got $3)"
        FAILURES=$((FAILURES + 1))
    fi
}

check_empty() { # name file
    CHECKS=$((CHECKS + 1))
    if [ -s "$2" ]; then
        echo "FAIL: $1 ($2 must be empty)"
        FAILURES=$((FAILURES + 1))
    else
        echo "PASS: $1"
    fi
}

check_path() { # name path
    CHECKS=$((CHECKS + 1))
    if [ -d "$2" ]; then
        echo "PASS: $1"
    else
        echo "FAIL: $1 (missing directory $2)"
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

# Parse `$2` as JSON and run `$3` against it (`doc` holds the parsed value).
check_json() { # name file check-body
    CHECKS=$((CHECKS + 1))
    if python3 -c "
import json, sys
with open(sys.argv[1]) as handle:
    doc = json.load(handle)
$3
" "$2" 2>"$TMP/json_err.txt"; then
        echo "PASS: $1"
    else
        echo "FAIL: $1 (invalid JSON or failed check: $(tr '\n' ' ' <"$TMP/json_err.txt"))"
        FAILURES=$((FAILURES + 1))
    fi
}

# --- help / version ---------------------------------------------------------

run_cli --help >"$OUT/help.txt" 2>"$ERR/help.txt"
code=$?
check_code "--help exits 0" 0 "$code"
check "help lists the commands" "force-unmount <dev>" "$OUT/help.txt"
check "help documents the exit codes" "2  usage or configuration error" "$OUT/help.txt"

run_cli --version >"$OUT/version.txt" 2>"$ERR/version.txt"
code=$?
check_code "--version exits 0" 0 "$code"
check "version prints the program name" "mount-tui " "$OUT/version.txt"

# --- list -------------------------------------------------------------------

run_cli list >"$OUT/list.txt" 2>"$ERR/list.txt"
code=$?
check_code "list exits 0" 0 "$code"
check "list prints the disk" "sda 100G disk" "$OUT/list.txt"
check "list prints the system mount" "sda1 100G part ext4 - /home \[sys\]" "$OUT/list.txt"
check "list prints the usb partition" "sdb1 8G part vfat USBKEY - \[usb\] \[rm\]" "$OUT/list.txt"
check "list flags the removable disk" "sdb 8G disk - - - \[usb\] \[rm\]" "$OUT/list.txt"
check "list prints multiple mount points" \
    "sdy1 1T part ext4 MULTI /mnt/multi1,/mnt/multi2" "$OUT/list.txt"
check_empty "list stderr is empty" "$ERR/list.txt"

run_cli list --json >"$OUT/list.json" 2>"$ERR/list.json"
code=$?
check_code "list --json exits 0" 0 "$code"
check_json "list --json is an array of devices" "$OUT/list.json" '
assert isinstance(doc, list) and len(doc) > 5, doc
sdb = [d for d in doc if d["name"] == "sdb"][0]
assert sdb["type"] == "disk" and sdb["usb"] is True and sdb["rm"] is True
assert sdb["children"][0]["name"] == "sdb1"
assert sdb["children"][0]["label"] == "USBKEY"
assert sdb["children"][0]["mountable"] is True
'
check_json "list --json keeps stable fields" "$OUT/list.json" '
sda1 = [d for d in doc if d["name"] == "sda"][0]["children"][0]
assert sda1["mountpoints"] == ["/home"]
assert sda1["protected"] is True
assert sda1["children"] == []
'

# A broken lsblk is an operation failure (exit 1), not a usage error.
env -u MOCK_STATE -u XDG_CONFIG_HOME HOME="$HOME_DIR" \
    PATH="$E2E_DIR/mock/bin:$PATH" \
    "$BIN" list >"$OUT/broken.txt" 2>"$ERR/broken.txt"
code=$?
check_code "a failing lsblk exits 1" 1 "$code"
check "a failing lsblk is reported on stderr" \
    "failed to list block devices" "$ERR/broken.txt"

# --- status -----------------------------------------------------------------

run_cli status sdb1 >"$OUT/status.txt" 2>"$ERR/status.txt"
code=$?
check_code "status exits 0" 0 "$code"
check "status prints the path" "path: /dev/sdb1" "$OUT/status.txt"
check "status reports the mount state" "mounted: false" "$OUT/status.txt"
check "status reports mountability" "mountable: true" "$OUT/status.txt"
check "status reports protection" "protected: false" "$OUT/status.txt"
check "status reports single-mount" "multi_mount: false" "$OUT/status.txt"

run_cli status sdb1 --json >"$OUT/status.json" 2>"$ERR/status.json"
check_json "status --json is an object" "$OUT/status.json" '
assert isinstance(doc, dict), doc
assert doc["path"] == "/dev/sdb1"
assert doc["name"] == "sdb1" and doc["label"] == "USBKEY"
assert doc["mounted"] is False and doc["mountable"] is True
assert doc["protected"] is False and doc["multi_mount"] is False
'

# `sdb1`, `/dev/sdb1` and a label (case-insensitive) are the same device.
run_cli status /dev/sdb1 >"$OUT/status_path.txt" 2>"$ERR/status_path.txt"
check_code "status accepts a device path" 0 "$?"
run_cli status usbkey >"$OUT/status_label.txt" 2>"$ERR/status_label.txt"
check_code "status accepts a label" 0 "$?"
check "label lookup resolves to the device" "path: /dev/sdb1" "$OUT/status_label.txt"

run_cli status sdy1 --json >"$OUT/status_multi.json" 2>"$ERR/status_multi.json"
check_json "status --json reports multi-mount" "$OUT/status_multi.json" '
assert doc["multi_mount"] is True
assert doc["mountpoints"] == ["/mnt/multi1", "/mnt/multi2"]
assert doc["mounted"] is True
'

# --- default mode: mount / unmount / force ----------------------------------

check_no_path "no mount root below HOME before any mount" "$HOME_DIR/mnt"

run_cli mount sdb1 >"$OUT/mount.txt" 2>"$ERR/mount.txt"
code=$?
check_code "default mount exits 0" 0 "$code"
check "mount reports the real mount point" \
    "Mounted /dev/sdb1 at /media/e2e/USBKEY" "$OUT/mount.txt"
check "udisksctl mount is used" "udisksctl mount -b /dev/sdb1" "$STATE/calls.log"
check_absent "default mode never uses sudo" "SUDO" "$STATE/calls.log"
check_no_path "default mode creates no directory" "$HOME_DIR/mnt"

run_cli status sdb1 >"$OUT/status_mounted.txt" 2>"$ERR/status_mounted.txt"
check "status reflects the new mount" "mounted: true" "$OUT/status_mounted.txt"

run_cli unmount sdb1 >"$OUT/unmount.txt" 2>"$ERR/unmount.txt"
code=$?
check_code "default unmount exits 0" 0 "$code"
check "unmount is reported" "Unmounted /dev/sdb1" "$OUT/unmount.txt"
check "udisksctl unmount is used" "udisksctl unmount -b /dev/sdb1" "$STATE/calls.log"

run_cli mount sdb1 >"$OUT/remount.txt" 2>"$ERR/remount.txt"
check_code "second mount exits 0" 0 "$?"
run_cli force-unmount sdb1 --yes >"$OUT/force.txt" 2>"$ERR/force.txt"
code=$?
check_code "force-unmount exits 0" 0 "$code"
check "force-unmount is reported" "Force unmounted /dev/sdb1" "$OUT/force.txt"
check "force-unmount uses udisksctl -f" \
    "udisksctl unmount -b /dev/sdb1 -f" "$STATE/calls.log"

# --- safety checks (nothing runs) -------------------------------------------

before="$(grep -c '^udisksctl' "$STATE/calls.log" 2>/dev/null || true)"

run_cli unmount sda1 >"$OUT/umount_system.txt" 2>"$ERR/umount_system.txt"
code=$?
check_code "unmounting a system mount exits 1" 1 "$code"
check "system mount is refused" "system mount" "$ERR/umount_system.txt"

run_cli mount sda1 >"$OUT/mount_mounted.txt" 2>"$ERR/mount_mounted.txt"
code=$?
check_code "mounting a mounted device exits 1" 1 "$code"
check "already mounted is reported" "already mounted at /home" "$ERR/mount_mounted.txt"

run_cli force-unmount sdy1 --yes >"$OUT/force_multi.txt" 2>"$ERR/force_multi.txt"
code=$?
check_code "multi-mount force-unmount exits 1" 1 "$code"
check "multi-mount is refused" "has 2 mount points" "$ERR/force_multi.txt"

run_cli mount sdc1 >"$OUT/mount_lvm.txt" 2>"$ERR/mount_lvm.txt"
code=$?
check_code "mounting an LVM member exits 1" 1 "$code"
check "LVM member has no filesystem" "no filesystem" "$ERR/mount_lvm.txt"

after="$(grep -c '^udisksctl' "$STATE/calls.log" 2>/dev/null || true)"
CHECKS=$((CHECKS + 1))
if [ "$before" -eq "$after" ]; then
    echo "PASS: refused operations run no udisksctl command"
else
    echo "FAIL: refused operations ran udisksctl ($before -> $after)"
    FAILURES=$((FAILURES + 1))
fi

# --- usage errors (exit 2) ---------------------------------------------------

run_cli force-unmount sdb1 >"$OUT/force_no_yes.txt" 2>"$ERR/force_no_yes.txt"
code=$?
check_code "force-unmount without --yes exits 2" 2 "$code"
check "missing --yes is explained" "--yes" "$ERR/force_no_yes.txt"

run_cli status sdq9 >"$OUT/status_missing.txt" 2>"$ERR/status_missing.txt"
code=$?
check_code "an unknown device exits 2" 2 "$code"
check "an unknown device lists the available names" \
    "available:.*sdb1" "$ERR/status_missing.txt"

run_cli wipe sdb1 >"$OUT/unknown_cmd.txt" 2>"$ERR/unknown_cmd.txt"
code=$?
check_code "an unknown command exits 2" 2 "$code"
check "an unknown command is reported" "unknown command" "$ERR/unknown_cmd.txt"

# --- sudo mode ---------------------------------------------------------------

ROOT_DIR="$TMP/mnt/usb"
TARGET="$ROOT_DIR/sdz1"

# The mock sudo refuses by default: the mount fails and its directory is gone.
run_cli --mount-mode sudo --mount-root "$ROOT_DIR" mount sdz1 \
    >"$OUT/sudo_fail.txt" 2>"$ERR/sudo_fail.txt"
code=$?
check_code "a failing sudo mount exits 1" 1 "$code"
check "the failed sudo mount is reported" \
    "Mount failed for /dev/sdz1" "$ERR/sudo_fail.txt"
check "sudo mount used the configured root" "mnt/usb/sdz1" "$STATE/calls.log"
check_no_path "the failed sudo mount leaves no directory" "$TARGET"

touch "$STATE/sudo_ok"
run_cli --mount-mode sudo --mount-root "$ROOT_DIR" mount sdz1 \
    >"$OUT/sudo_ok.txt" 2>"$ERR/sudo_ok.txt"
code=$?
check_code "a successful sudo mount exits 0" 0 "$code"
check "sudo mount reports the real mount point" \
    "Mounted /dev/sdz1 at" "$OUT/sudo_ok.txt"
check_path "the successful sudo mount keeps its directory" "$TARGET"
check_absent "sudo mode never calls udisksctl" \
    "udisksctl mount -b /dev/sdz1" "$STATE/calls.log"

run_cli --mount-mode sudo --mount-root "$ROOT_DIR" status sdz1 \
    >"$OUT/sudo_status.txt" 2>"$ERR/sudo_status.txt"
check "sudo mount is visible through status" "mounted: true" "$OUT/sudo_status.txt"

touch "$STATE/sudo_umount_ok"
run_cli --mount-mode sudo --mount-root "$ROOT_DIR" unmount sdz1 \
    >"$OUT/sudo_umount.txt" 2>"$ERR/sudo_umount.txt"
code=$?
check_code "sudo unmount exits 0" 0 "$code"
check "sudo unmount is reported" "Unmounted /dev/sdz1" "$OUT/sudo_umount.txt"
check "sudo umount is used" "SUDO umount /dev/sdz1" "$STATE/calls.log"
check_no_path "sudo unmount clears the mount point" "$STATE/sdz1_mp"

run_cli --mount-mode sudo --mount-root "$ROOT_DIR" mount sdz1 \
    >"$OUT/sudo_remount.txt" 2>"$ERR/sudo_remount.txt"
check_code "sudo remount exits 0" 0 "$?"
run_cli --mount-mode sudo --mount-root "$ROOT_DIR" force-unmount sdz1 --yes \
    >"$OUT/sudo_force.txt" 2>"$ERR/sudo_force.txt"
code=$?
check_code "sudo force-unmount exits 0" 0 "$code"
check "sudo force-unmount is reported" "Force unmounted /dev/sdz1" "$OUT/sudo_force.txt"
check "sudo force-unmount uses umount -l" "SUDO umount -l /dev/sdz1" "$STATE/calls.log"

# The environment variables are the same layer as the flags.
(
    export MOUNT_TUI_MODE=sudo MOUNT_TUI_MOUNT_ROOT="$ROOT_DIR"
    run_cli status sdz1
) >"$OUT/sudo_env.txt" 2>"$ERR/sudo_env.txt"
code=$?
check_code "MOUNT_TUI_MODE=sudo is honoured" 0 "$code"
check "the environment root is used" "path: /dev/sdz1" "$OUT/sudo_env.txt"

echo "--- $0: $CHECKS checks, $FAILURES failure(s) ---"
[ "$FAILURES" -eq 0 ]
