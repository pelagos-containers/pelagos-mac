#!/bin/bash
# test-subscribe-stress.sh — Phase 3: continuous delivery stress test.
# Starts N containers (2s lifetime each) at 1 per 3s while subscribing.
# Measures event delivery rate; requires >= 95%.

set -uo pipefail

N=${1:-15}   # number of containers (default 15 ≈ 45s test)
INTERVAL=3   # seconds between container starts
LIFETIME=2   # seconds each container runs

echo "Stress test: $N containers, ${LIFETIME}s lifetime, ${INTERVAL}s interval"
echo ""

LOG=$(mktemp /tmp/stress-events-XXXXXX.log)
cleanup() { kill "$SUB_PID" 2>/dev/null || true; wait "$SUB_PID" 2>/dev/null || true; rm -f "$LOG"; }
trap cleanup EXIT

pelagos subscribe > "$LOG" &
SUB_PID=$!
sleep 2  # wait for initial snapshot

STARTED=0
EXPECTED_STARTS=0
EXPECTED_EXITS=0

for i in $(seq 1 "$N"); do
    NAME="stress-$i"
    echo "Starting $NAME ($i/$N)..."
    pelagos run --detach --name "$NAME" alpine sleep "$LIFETIME" 2>/dev/null
    ((EXPECTED_STARTS++)) || true
    ((EXPECTED_EXITS++)) || true
    sleep "$INTERVAL"
done

# Wait for last containers to exit + event delivery
sleep $((LIFETIME + 3))

kill "$SUB_PID" 2>/dev/null || true
wait "$SUB_PID" 2>/dev/null || true

# Count delivered events
GOT_STARTS=$(grep -c '"type":"container_started"' "$LOG" 2>/dev/null || echo 0)
GOT_EXITS=$(grep -c '"type":"container_exited"' "$LOG" 2>/dev/null || echo 0)

echo ""
echo "=== Results ==="
echo "Expected: $EXPECTED_STARTS ContainerStarted, $EXPECTED_EXITS ContainerExited"
echo "Got:      $GOT_STARTS ContainerStarted, $GOT_EXITS ContainerExited"

# Calculate delivery rate (as percentage * 100 to avoid floating point)
TOTAL_EXPECTED=$((EXPECTED_STARTS + EXPECTED_EXITS))
TOTAL_GOT=$((GOT_STARTS + GOT_EXITS))
RATE=$((TOTAL_GOT * 100 / TOTAL_EXPECTED))
echo "Delivery rate: ${RATE}%"

if [ "$RATE" -ge 95 ]; then
    echo "PASS (>= 95%)"
    exit 0
else
    echo "FAIL (< 95%)"
    exit 1
fi
