#!/bin/bash
# test-subscribe-reconnect.sh — Phase 2: reconnect reliability.
# Tests that a new subscription receives an accurate Snapshot, and verifies
# that heartbeats arrive within the expected window.

set -uo pipefail

PASS=0
FAIL=0

pass() { echo "PASS: $1"; ((PASS++)) || true; }
fail() { echo "FAIL: $1"; ((FAIL++)) || true; }

# --- T1: Long-running container visible in snapshot ---
echo "=== T1: Container visible in fresh snapshot ==="
pelagos run --detach --name test-reconnect-1 alpine sleep 30

sleep 1
SNAP=$(timeout 4 pelagos subscribe 2>/dev/null | head -1)
if echo "$SNAP" | grep -q "test-reconnect-1"; then
    pass "test-reconnect-1 present in snapshot"
else
    fail "test-reconnect-1 missing from snapshot"
    echo "  Snapshot: $SNAP"
fi

# --- T2: After container is stopped, it vanishes from running-only snapshot ---
echo ""
echo "=== T2: Stopped container absent from fresh snapshot (running-only) ==="
pelagos stop test-reconnect-1 2>/dev/null || true
pelagos rm test-reconnect-1 2>/dev/null || true
sleep 1

# The snapshot includes ALL containers (pelagos-guest sends all state files).
# The running-only test: the container should have status="exited".
SNAP2=$(timeout 4 pelagos subscribe 2>/dev/null | head -1)
# Check that if it appears, it has status exited (not running)
if echo "$SNAP2" | python3 -c "
import json,sys
snap=json.load(sys.stdin)
cs=[c for c in snap.get('containers',[]) if c['name']=='test-reconnect-1']
if not cs:
    print('absent')
else:
    print(cs[0]['status'])
" 2>/dev/null | grep -qE "^absent$|^exited$"; then
    pass "test-reconnect-1 absent or exited in new snapshot"
else
    fail "test-reconnect-1 still shows as running after stop"
    echo "  Snapshot: $SNAP2"
fi

# --- T3: Heartbeat arrives within 8s on idle connection ---
echo ""
echo "=== T3: Heartbeat arrives within 8s ==="
HBLOG=$(mktemp /tmp/hb-XXXXXX.log)
pelagos subscribe > "$HBLOG" &
HBPID=$!
sleep 8
kill "$HBPID" 2>/dev/null || true
wait "$HBPID" 2>/dev/null || true

if grep -q '"type":"heartbeat"' "$HBLOG"; then
    pass "heartbeat received within 8s"
else
    fail "no heartbeat within 8s — heartbeat not working"
    echo "  Events seen: $(cat $HBLOG)"
fi
rm -f "$HBLOG"

echo ""
echo "=== Summary ==="
echo "PASS: $PASS  FAIL: $FAIL"
[ "$FAIL" -eq 0 ]
