#!/bin/bash
# test-state-files.sh — Phase 0 state file audit.
# Run inside the VM via: pelagos vm shell < scripts/test-state-files.sh
# Answers Q1 (state files kept after exit?), Q2 (write timing), Q3 (field set).

STATE_DIR=/run/pelagos/containers

echo "=== Phase 0: State File Audit ==="
echo ""

echo "--- [0] State files before any run ---"
ls -la "$STATE_DIR/" 2>/dev/null || echo "(empty or absent)"
echo ""

echo "--- [1] Starting 2s container in background ---"
pelagos run alpine sh -c 'echo STARTED; sleep 2; echo DONE' &
RUN_PID=$!

# Poll every 100ms for 4 seconds
for i in $(seq 1 40); do
    TS=$(awk "BEGIN{printf \"%.1f\", $i / 10}")
    echo "T=${TS}s:"
    ls "$STATE_DIR/" 2>/dev/null | while read -r name; do
        f="$STATE_DIR/$name/state.json"
        if [ -f "$f" ]; then
            printf "  %s/state.json: " "$name"
            cat "$f"
            echo ""
        fi
    done
    sleep 0.1
done

wait "$RUN_PID"
echo ""
echo "--- [2] State files after 2s container exited ---"
ls -la "$STATE_DIR/" 2>/dev/null || echo "(empty or absent)"
for f in "$STATE_DIR"/*/state.json; do
    [ -f "$f" ] && { printf "%s: " "$f"; cat "$f"; echo ""; }
done
echo ""

echo "--- [3] Instant container (echo hello) ---"
pelagos run alpine echo hello
sleep 0.5

echo "State files 500ms after instant container:"
ls -la "$STATE_DIR/" 2>/dev/null || echo "(empty or absent)"
for f in "$STATE_DIR"/*/state.json; do
    [ -f "$f" ] && { printf "%s: " "$f"; cat "$f"; echo ""; }
done
echo ""

echo "--- [4] Instant container (true) ---"
pelagos run alpine true
sleep 0.5

echo "State files 500ms after 'true':"
ls -la "$STATE_DIR/" 2>/dev/null || echo "(empty or absent)"
for f in "$STATE_DIR"/*/state.json; do
    [ -f "$f" ] && { printf "%s: " "$f"; cat "$f"; echo ""; }
done
echo ""

echo "=== Phase 0 complete ==="
