#!/usr/bin/env bash
# test-stress.sh — Stress-test pelagos-mac operations for brittleness.
#
# Hammers each subsystem N times and fails fast on the first regression.
# All tests run against a warm (already-running) daemon unless the test
# explicitly requires a restart.
#
# Usage:
#   bash scripts/test-stress.sh [--debug]
#
# Closes #122
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"

DEBUG=0
[[ "${1:-}" == "--debug" ]] && DEBUG=1

PASS=0
FAIL=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

ok()   { echo "  PASS  $1"; ((PASS++)); }
fail() { echo "  FAIL  $1"; ((FAIL++)); }

pelagos() {
    "$BINARY" \
        --kernel "$KERNEL" \
        --initrd "$INITRD" \
        --disk   "$DISK" \
        "$@"
}

ms_now() { python3 -c "import time; print(int(time.time()*1000))"; }

# ---------------------------------------------------------------------------
# Pre-flight
# ---------------------------------------------------------------------------

echo ""
echo "=== pelagos-mac stress suite ==="
echo ""

for f in "$KERNEL" "$INITRD" "$DISK" "$BINARY"; do
    if [[ ! -f "$f" ]]; then
        echo "ABORT: missing $f — run 'make all' first."
        exit 1
    fi
done

printf "  daemon running... "
if ! pelagos ping 2>&1 | grep -q pong; then
    echo "ABORT: daemon not running — run 'bash scripts/vm-ping.sh' first."
    exit 1
fi
echo "ok"
echo ""

# ---------------------------------------------------------------------------
# S1 — Ping storm (warm daemon, 20 iterations)
# ---------------------------------------------------------------------------
echo "--- S1: ping storm (20x warm ping) ---"
S1_FAIL=0
for i in $(seq 1 20); do
    if out=$(pelagos ping 2>&1) && echo "$out" | grep -q pong; then
        [[ $DEBUG -eq 1 ]] && echo "    iter $i: pong"
    else
        echo "    iter $i FAILED: $out"
        ((S1_FAIL++))
    fi
done
if [[ $S1_FAIL -eq 0 ]]; then
    ok "S1: 20/20 pings returned pong"
else
    fail "S1: $S1_FAIL/20 pings failed"
fi

# ---------------------------------------------------------------------------
# S2 — SSH rapid repeat (10 sequential sessions)
# ---------------------------------------------------------------------------
echo "--- S2: ssh rapid repeat (10x vm ssh) ---"
S2_FAIL=0
for i in $(seq 1 10); do
    if out=$(pelagos vm ssh -- uname -r 2>&1) && echo "$out" | grep -qE "^[0-9]+\.[0-9]+"; then
        [[ $DEBUG -eq 1 ]] && echo "    iter $i: $out"
    else
        echo "    iter $i FAILED: $out"
        ((S2_FAIL++))
    fi
done
if [[ $S2_FAIL -eq 0 ]]; then
    ok "S2: 10/10 SSH sessions succeeded"
else
    fail "S2: $S2_FAIL/10 SSH sessions failed"
fi

# ---------------------------------------------------------------------------
# S3 — Container run/rm cycle (10 iterations)
# ---------------------------------------------------------------------------
echo "--- S3: container run/rm cycle (10x alpine echo) ---"
S3_FAIL=0
# Pre-pull to avoid timing out on first run.
pelagos run alpine echo preflight >/dev/null 2>&1 || true

for i in $(seq 1 10); do
    cname="stress-s3-$i"
    # Clean up any leftover from a previous failed run.
    pelagos rm -f "$cname" >/dev/null 2>&1 || true

    if out=$(pelagos run --name "$cname" alpine echo "iter-$i" 2>&1) \
        && echo "$out" | grep -q "iter-$i"; then
        [[ $DEBUG -eq 1 ]] && echo "    iter $i: ok"
        pelagos rm "$cname" >/dev/null 2>&1 || true
    else
        echo "    iter $i FAILED: $out"
        ((S3_FAIL++))
        pelagos rm -f "$cname" >/dev/null 2>&1 || true
    fi
done
if [[ $S3_FAIL -eq 0 ]]; then
    ok "S3: 10/10 run/rm cycles succeeded"
else
    fail "S3: $S3_FAIL/10 run/rm cycles failed"
fi

# ---------------------------------------------------------------------------
# S4 — VM restart cycle (3 full restart+ping cycles)
# ---------------------------------------------------------------------------
echo "--- S4: VM restart cycle (3x vm-restart) ---"
S4_FAIL=0
for i in $(seq 1 3); do
    printf "    restart %d... " "$i"
    if out=$(bash "$SCRIPT_DIR/vm-restart.sh" 2>&1) && echo "$out" | grep -q pong; then
        echo "pong"
    else
        echo "FAILED: $out"
        ((S4_FAIL++))
    fi
done
if [[ $S4_FAIL -eq 0 ]]; then
    ok "S4: 3/3 restart cycles succeeded"
else
    fail "S4: $S4_FAIL/3 restart cycles failed"
fi

# ---------------------------------------------------------------------------
# S5 — Concurrent ping (5 simultaneous invocations)
# ---------------------------------------------------------------------------
echo "--- S5: concurrent ping (5 simultaneous) ---"
TMPDIR_S5=$(mktemp -d)
for i in $(seq 1 5); do
    pelagos ping >"$TMPDIR_S5/out.$i" 2>&1 &
done
wait

S5_FAIL=0
for i in $(seq 1 5); do
    if grep -q pong "$TMPDIR_S5/out.$i"; then
        [[ $DEBUG -eq 1 ]] && echo "    worker $i: pong"
    else
        echo "    worker $i FAILED: $(cat "$TMPDIR_S5/out.$i")"
        ((S5_FAIL++))
    fi
done
rm -rf "$TMPDIR_S5"
if [[ $S5_FAIL -eq 0 ]]; then
    ok "S5: 5/5 concurrent pings returned pong"
else
    fail "S5: $S5_FAIL/5 concurrent pings failed"
fi

# ---------------------------------------------------------------------------
# S6 — Repeated vm status (20 sequential calls)
# ---------------------------------------------------------------------------
echo "--- S6: vm status repeat (20x vm status) ---"
S6_FAIL=0
for i in $(seq 1 20); do
    if out=$(pelagos vm status 2>&1) && echo "$out" | grep -q running; then
        [[ $DEBUG -eq 1 ]] && echo "    iter $i: $out"
    else
        echo "    iter $i FAILED: $out"
        ((S6_FAIL++))
    fi
done
if [[ $S6_FAIL -eq 0 ]]; then
    ok "S6: 20/20 vm status calls reported running"
else
    fail "S6: $S6_FAIL/20 vm status calls failed"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=============================="
total=$((PASS + FAIL))
echo "  $PASS/$total tests passed"
if [[ $FAIL -gt 0 ]]; then
    echo "  $FAIL FAILED"
    echo "=============================="
    exit 1
else
    echo "  PASS"
    echo "=============================="
    exit 0
fi
