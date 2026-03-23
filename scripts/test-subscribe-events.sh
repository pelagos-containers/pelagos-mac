#!/bin/bash
# test-subscribe-events.sh — Phase 1: subscription event delivery baseline.
# Run on macOS with VM running.
# Tests T1–T5: containers of varying lifetimes; measures what events arrive.

set -uo pipefail

PASS=0
FAIL=0
LOG=$(mktemp /tmp/subscribe-events-XXXXXX.log)

cleanup() {
    kill "$SUB_PID" 2>/dev/null || true
    wait "$SUB_PID" 2>/dev/null || true
    rm -f "$LOG"
}
trap cleanup EXIT

echo "Starting pelagos subscribe..."
pelagos subscribe > "$LOG" &
SUB_PID=$!

# Give subscribe time to connect and receive the initial Snapshot
sleep 2

echo "Initial snapshot received (line count: $(wc -l < "$LOG"))"

run_test() {
    local label="$1"
    local lifetime="$2"   # seconds (can be decimal)
    local timeout_s="$3"  # seconds to wait after expected exit

    local before_lines
    before_lines=$(wc -l < "$LOG")

    echo ""
    echo "=== $label ==="

    if [ "$lifetime" = "instant" ]; then
        pelagos run alpine echo hello
    else
        pelagos run alpine sh -c "sleep $lifetime"
    fi

    sleep "$timeout_s"

    local after_lines
    after_lines=$(wc -l < "$LOG")
    local new_count=$((after_lines - before_lines))

    local new_events=""
    if [ "$new_count" -gt 0 ]; then
        new_events=$(tail -n "+$((before_lines + 1))" "$LOG")
    fi

    local got_start=0
    local got_exit=0
    echo "$new_events" | grep -q '"type":"container_started"' && got_start=1 || true
    echo "$new_events" | grep -q '"type":"container_exited"'  && got_exit=1 || true

    local result="PASS"
    local notes=""
    if [ "$got_start" -eq 0 ]; then
        result="FAIL"
        notes="missing container_started"
    fi
    if [ "$got_exit" -eq 0 ]; then
        result="FAIL"
        notes="${notes:+$notes, }missing container_exited"
    fi

    if [ "$result" = "PASS" ]; then
        echo "PASS  start=$got_start exit=$got_exit  new_events=$new_count"
        ((PASS++)) || true
    else
        echo "FAIL  start=$got_start exit=$got_exit  new_events=$new_count  ($notes)"
        ((FAIL++)) || true
        if [ "$new_count" -gt 0 ]; then
            echo "Events received:"
            echo "$new_events"
        fi
    fi
}

# T1: 5s container — baseline, should always work
run_test "T1: sleep 5 (5s lifetime)" "5" "7"

# T2: 1s container — still within poll window
run_test "T2: sleep 1 (1s lifetime)" "1" "3"

# T3: 500ms container — 2× poll cycles
run_test "T3: sleep 0.5 (500ms lifetime)" "0.5" "2"

# T4: 100ms container — less than 1 poll cycle
run_test "T4: sleep 0.1 (100ms lifetime)" "0.1" "2"

# T5: Instant container (echo) — complete before first poll
run_test "T5: echo hello (instant)" "instant" "2"

echo ""
echo "=== Summary ==="
echo "PASS: $PASS  FAIL: $FAIL"

[ "$FAIL" -eq 0 ]
