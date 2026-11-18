#!/usr/bin/env bash
# Scenario: hotplug auto-refresh (2 s poll) without any key press.
#
# A new disk appears when $MOCK_STATE/hotplug is created and disappears when it
# is removed; the selection and the status bar must survive both refreshes.
set -u

# shellcheck source=common.sh
. "$(cd "$(dirname "$0")" && pwd)/common.sh"

e2e_init hotplug || exit 1
start_tui

send Down
send Down
send Down # sdb1
send u    # not mounted: this status must survive the automatic refreshes

cap before
check "selection before hotplug" "Path:    /dev/sdb1" "$OUT/before.txt"
check "status before hotplug" "not mounted" "$OUT/before.txt"
check_absent "no hotplug device initially" "sdhot" "$OUT/before.txt"

touch "$STATE/hotplug"
sleep 3
cap added
check "hotplug device appears without keypress" "sdhot" "$OUT/added.txt"
check "selection preserved across auto-refresh" "Path:    /dev/sdb1" "$OUT/added.txt"
check "status not clobbered by auto-refresh" "not mounted" "$OUT/added.txt"

rm -f "$STATE/hotplug"
sleep 3
cap removed
check_absent "hotplug device disappears without keypress" "sdhot" "$OUT/removed.txt"
check "selection preserved after removal" "Path:    /dev/sdb1" "$OUT/removed.txt"

expect_exit

e2e_finish
