#!/usr/bin/env bash
# test-perf.sh — Performance baseline for core pelagos-mac operations.
#
# Measures wall-clock latency (mean, p50, p95, p99) for four benchmarks.
# No pass/fail threshold — the printed table IS the baseline.
#
# Usage:
#   bash scripts/test-perf.sh
#
# Requires a warm (already-running) daemon.  P4 (VM restart) temporarily
# stops and restarts the daemon; all other benchmarks leave it running.
#
# Closes #123
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"

pelagos() {
    "$BINARY" \
        --kernel "$KERNEL" \
        --initrd "$INITRD" \
        --disk   "$DISK" \
        "$@"
}

ms_now() { python3 -c "import time; print(int(time.time()*1000))"; }

stats() {
    # stats <label> <n> <sample1> <sample2> ...
    # Prints: label  N  mean  p50  p95  p99  (all in ms)
    local label="$1" n="$2"; shift 2
    local samples=("$@")
    python3 - "$label" "$n" "${samples[@]}" <<'PY'
import sys, statistics
label = sys.argv[1]
n     = int(sys.argv[2])
vals  = sorted(int(x) for x in sys.argv[3:])
mean  = int(statistics.mean(vals))
def pct(p):
    i = (p/100) * (len(vals)-1)
    lo, hi = int(i), min(int(i)+1, len(vals)-1)
    return int(vals[lo] + (vals[hi]-vals[lo])*(i-lo))
p50 = pct(50); p95 = pct(95); p99 = pct(99)
print(f"{label:<18} {n:>3}   {mean:>6}ms   {p50:>6}ms   {p95:>6}ms   {p99:>6}ms")
PY
}

# ---------------------------------------------------------------------------
# Pre-flight
# ---------------------------------------------------------------------------

echo ""
echo "=== pelagos-mac performance baseline ==="
echo "    binary: $BINARY"
echo "    date:   $(date -u +%Y-%m-%dT%H:%MZ)"
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
# P1 — Warm ping latency (N=20)
# ---------------------------------------------------------------------------
printf "P1 warm-ping        "
P1_N=20
P1_SAMPLES=()
for i in $(seq 1 $P1_N); do
    t0=$(ms_now)
    pelagos ping >/dev/null 2>&1
    t1=$(ms_now)
    P1_SAMPLES+=($((t1 - t0)))
done

# ---------------------------------------------------------------------------
# P2 — SSH session establishment (N=10)
# ---------------------------------------------------------------------------
printf "\nP2 ssh-session      "
P2_N=10
P2_SAMPLES=()
for i in $(seq 1 $P2_N); do
    t0=$(ms_now)
    pelagos vm ssh -- true >/dev/null 2>&1
    t1=$(ms_now)
    P2_SAMPLES+=($((t1 - t0)))
done

# ---------------------------------------------------------------------------
# P3 — Container cold-start / echo (N=5, image pre-cached)
# ---------------------------------------------------------------------------
# Warm the image cache with a silent pre-pull.
pelagos run alpine echo warmup >/dev/null 2>&1 || true

printf "\nP3 container-run    "
P3_N=5
P3_SAMPLES=()
for i in $(seq 1 $P3_N); do
    cname="perf-p3-$i"
    pelagos rm -f "$cname" >/dev/null 2>&1 || true
    t0=$(ms_now)
    pelagos run --name "$cname" alpine echo ok >/dev/null 2>&1
    t1=$(ms_now)
    P3_SAMPLES+=($((t1 - t0)))
    pelagos rm "$cname" >/dev/null 2>&1 || true
done

# ---------------------------------------------------------------------------
# P4 — VM cold-boot (full restart, N=3)
# ---------------------------------------------------------------------------
printf "\nP4 vm-restart       "
P4_N=3
P4_SAMPLES=()
for i in $(seq 1 $P4_N); do
    t0=$(ms_now)
    bash "$SCRIPT_DIR/vm-restart.sh" >/dev/null 2>&1
    t1=$(ms_now)
    P4_SAMPLES+=($((t1 - t0)))
done

# ---------------------------------------------------------------------------
# Results table
# ---------------------------------------------------------------------------
echo ""
echo ""
echo "benchmark            N     mean       p50       p95       p99"
echo "──────────────────────────────────────────────────────────────"
stats "P1 warm-ping"     $P1_N "${P1_SAMPLES[@]}"
stats "P2 ssh-session"   $P2_N "${P2_SAMPLES[@]}"
stats "P3 container-run" $P3_N "${P3_SAMPLES[@]}"
stats "P4 vm-restart"    $P4_N "${P4_SAMPLES[@]}"
echo ""
