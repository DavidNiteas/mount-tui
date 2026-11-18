#!/usr/bin/env bash
# Scenario: fallback to the legacy `lsblk` column set.
#
# tests/e2e/mock_legacy/bin/lsblk rejects MOUNTPOINTS (exit 32) and answers the
# MOUNTPOINT-only query, which the TUI must retry automatically.
set -u

# shellcheck source=common.sh
. "$(cd "$(dirname "$0")" && pwd)/common.sh"

e2e_init legacy || exit 1
start_tui "$E2E_DIR/mock_legacy/bin"

cap legacy_start
check "startup with legacy lsblk" "Mount-TUI" "$OUT/legacy_start.txt"
check "legacy device visible" "legacy1" "$OUT/legacy_start.txt"
check "legacy RO flag parsed" "\[RO\]" "$OUT/legacy_start.txt"
check "legacy RM flag parsed" "\[RM\]" "$OUT/legacy_start.txt"

send Down
cap legacy_detail
check "legacy mountpoint parsed" "/legacy/mnt" "$OUT/legacy_detail.txt"
check "fallback invoked first with MOUNTPOINTS" "MOUNTPOINTS" "$STATE/legacy.log"
check_count_at_least "fallback retried without MOUNTPOINTS" "^lsblk " 2 "$STATE/legacy.log"

expect_exit

e2e_finish
