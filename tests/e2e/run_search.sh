#!/usr/bin/env bash
# Scenario: loop filter, live search and details scrolling.
#
# Loop devices are hidden by default and `l` toggles them; `/` opens the search
# line, filters as you type, Enter keeps the query and Esc drops it; `[` / `]`
# scroll the details pane. Nothing here touches a real device: `lsblk`,
# `udisksctl` and `sudo` all come from tests/e2e/mock/bin, and every check works
# on `tmux capture-pane` snapshots.
set -u

# shellcheck source=common.sh
. "$(cd "$(dirname "$0")" && pwd)/common.sh"

e2e_init search || exit 1
start_tui

cap default
check "usb partition listed" "sdb1" "$OUT/default.txt"
check_absent "loop devices hidden by default" "loop0" "$OUT/default.txt"

send l
cap loops_shown
check "l reveals the loop device" "loop0" "$OUT/loops_shown.txt"
check "status reports shown loops" "Loop devices shown" "$OUT/loops_shown.txt"

send l
cap loops_hidden
check_absent "l hides the loop device again" "loop0" "$OUT/loops_hidden.txt"
check "status reports hidden loops" "Loop devices hidden" "$OUT/loops_hidden.txt"

# Search: `/` opens the line, typing filters live.
send /
send -l "USBKEY"
cap search_input
check "input line shows the query" "Search: USBKEY" "$OUT/search_input.txt"
check_absent "typing filters the list live" "SYSDISK" "$OUT/search_input.txt"

send Enter
cap search_applied
check "applied filter is announced" "Filter: USBKEY (/ edit, Esc clear)" "$OUT/search_applied.txt"
check "matching branch is kept" "sdb1" "$OUT/search_applied.txt"
check_absent "unmatched devices are filtered out" "sda1" "$OUT/search_applied.txt"

send Escape
cap search_cleared
check_absent "esc clears the filter hint" "Filter:" "$OUT/search_cleared.txt"
check "esc restores the whole tree" "SYSDISK" "$OUT/search_cleared.txt"

# A query without matches empties the list and the selection.
send /
send -l "NOSUCHDEVICE"
send Enter
cap no_match
check "no match clears the selection" "No device selected" "$OUT/no_match.txt"
check_absent "no match leaves no rows" "sdb1" "$OUT/no_match.txt"

send Escape
cap no_match_cleared
check "esc restores the list after a miss" "sda1" "$OUT/no_match_cleared.txt"

# Details scrolling: `]` hides the first row, `[` brings it back.
send -l "]"
cap details_scrolled
check_absent "details scrolled down by one row" "Device:  sda" "$OUT/details_scrolled.txt"

send -l "["
cap details_unscrolled
check "details scrolled back to the top" "Device:  sda" "$OUT/details_unscrolled.txt"

expect_exit

e2e_finish
