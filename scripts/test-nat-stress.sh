#!/usr/bin/env bash
# test-nat-stress.sh — Stress-test NAT stability under a persistent VM.
#
# Goal: determine whether VZNATNetworkDeviceAttachment's NAT rules degrade
# over time even when the VM is NOT restarted — i.e., whether the persistent
# VM (Phase 2) fully sidesteps issue #26, or whether degradation still occurs
# after enough network activity.
#
# The test boots the VM once, then runs N rounds of network operations,
# checking ICMP and TCP reachability from inside the VM after every round.
# If all rounds pass, option 1 (VZNATNetworkDeviceAttachment) is acceptable
# long-term with the persistent VM.  If it degrades, migration to socket_vmnet
# is necessary.
#
# Usage:
#   ./test-nat-stress.sh            — 20 rounds (default)
#   ./test-nat-stress.sh 40         — N rounds
#
# Prerequisites:
#   - make image && make sign
#
# Output:
#   Console: per-round PASS/FAIL with ICMP and TCP status
#   ~/.local/share/pelagos/daemon.log: VM console output (boot probes)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
CMDLINE="console=hvc0"
ROUNDS="${1:-20}"

PASS=0
FAIL=0
FIRST_FAIL_ROUND=""

pelagos() {
    "$BINARY" \
        --kernel  "$KERNEL" \
        --initrd  "$INITRD" \
        --disk    "$DISK" \
        --cmdline "$CMDLINE" \
        "$@" 2>&1
}

ms_now() { python3 -c "import time; print(int(time.time() * 1000))"; }

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
echo "=== preflight ==="
for f in "$KERNEL" "$INITRD" "$DISK" "$BINARY"; do
    if [ ! -f "$f" ]; then
        echo "FAIL: missing $f — run 'make image' and 'make sign' first."
        exit 1
    fi
done
echo "  [OK] all build artifacts present"

# ---------------------------------------------------------------------------
# Boot: stop any running daemon, then boot fresh.
# The VM boots once and stays up for all rounds.
# ---------------------------------------------------------------------------
echo ""
echo "=== cold boot (stopping any running daemon) ==="
pelagos vm stop > /dev/null 2>&1 || true
sleep 2

T0=$(ms_now)
OUT=$(pelagos ping)
T1=$(ms_now)
BOOT_MS=$(( T1 - T0 ))

if echo "$OUT" | grep -q "^pong$"; then
    echo "  [OK]   VM booted and ping returned pong (${BOOT_MS} ms)"
else
    echo "  FAIL: initial ping failed — output: $OUT"
    echo "  Check daemon.log: ~/.local/share/pelagos/daemon.log"
    exit 1
fi

echo ""
echo "  Boot probe results (from daemon.log):"
grep -E "\[pelagos-init\] (ICMP|TCP)" ~/.local/share/pelagos/daemon.log 2>/dev/null | tail -2 | sed 's/^/  /'

# ---------------------------------------------------------------------------
# Check connectivity from inside the VM.
# Returns: "ICMP=OK TCP=OK", "ICMP=OK TCP=FAIL", etc.
# ---------------------------------------------------------------------------
check_network() {
    local icmp tcp

    # ICMP: ping 8.8.8.8 once, 2s timeout
    ICMP_OUT=$(pelagos exec alpine /bin/sh -c \
        'busybox ping -c 1 -W 2 -q 8.8.8.8 >/dev/null 2>&1 && echo OK || echo FAIL' \
        2>/dev/null || echo "FAIL")
    icmp=$(echo "$ICMP_OUT" | grep -E "^(OK|FAIL)$" | tail -1)
    [ -z "$icmp" ] && icmp="FAIL"

    # TCP: connect to 1.1.1.1:443, 3s timeout
    TCP_OUT=$(pelagos exec alpine /bin/sh -c \
        'busybox nc -w 3 1.1.1.1 443 </dev/null >/dev/null 2>&1 && echo OK || echo FAIL' \
        2>/dev/null || echo "FAIL")
    tcp=$(echo "$TCP_OUT" | grep -E "^(OK|FAIL)$" | tail -1)
    [ -z "$tcp" ] && tcp="FAIL"

    echo "ICMP=${icmp} TCP=${tcp}"
}

# ---------------------------------------------------------------------------
# Stress rounds
# ---------------------------------------------------------------------------
echo ""
echo "=== NAT stress test: $ROUNDS rounds ==="
echo "    Each round: 3 x 'pelagos run alpine /bin/echo' + network probe"
echo ""

for i in $(seq 1 "$ROUNDS"); do
    # Run 3 back-to-back container executions (no network needed — exercises
    # the vsock/daemon path without triggering image pulls every round)
    RUN_FAIL=0
    for j in 1 2 3; do
        OUT=$(pelagos run alpine /bin/echo "round${i}-run${j}" 2>/dev/null || true)
        if ! echo "$OUT" | grep -q "round${i}-run${j}"; then
            RUN_FAIL=$((RUN_FAIL + 1))
        fi
    done

    # Network probe from inside the VM
    NET=$(check_network)
    ICMP_STATUS=$(echo "$NET" | grep -o 'ICMP=[A-Z]*' | cut -d= -f2)
    TCP_STATUS=$(echo "$NET" | grep -o 'TCP=[A-Z]*' | cut -d= -f2)

    if [ "$ICMP_STATUS" = "OK" ] && [ "$TCP_STATUS" = "OK" ] && [ "$RUN_FAIL" -eq 0 ]; then
        PASS=$((PASS + 1))
        printf "  [PASS] round %2d/%d  ICMP=OK  TCP=OK  runs=3/3\n" "$i" "$ROUNDS"
    else
        FAIL=$((FAIL + 1))
        [ -z "$FIRST_FAIL_ROUND" ] && FIRST_FAIL_ROUND="$i"
        printf "  [FAIL] round %2d/%d  ICMP=%-4s TCP=%-4s runs_ok=%d/3\n" \
            "$i" "$ROUNDS" "$ICMP_STATUS" "$TCP_STATUS" "$((3 - RUN_FAIL))"
        # On first network failure, dump current daemon.log tail for context
        if [ "$FAIL" -eq 1 ]; then
            echo ""
            echo "  --- daemon.log tail at failure ---"
            tail -10 ~/.local/share/pelagos/daemon.log 2>/dev/null | sed 's/^/  /'
            echo "  ----------------------------------"
            echo ""
        fi
    fi

    # Every 5 rounds, also pull a fresh image to exercise the full network path
    if [ $((i % 5)) -eq 0 ]; then
        PULL_OUT=$(pelagos run alpine /bin/sh -c 'pelagos image pull alpine >/dev/null 2>&1; echo pull_done' 2>/dev/null || echo "pull_fail")
        if echo "$PULL_OUT" | grep -q "pull_done"; then
            echo "         [pull check] alpine pull: OK"
        else
            echo "         [pull check] alpine pull: FAIL"
        fi
    fi
done

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "========================================"
echo "  Rounds:     $ROUNDS"
echo "  Passed:     $PASS"
echo "  Failed:     $FAIL"
if [ -n "$FIRST_FAIL_ROUND" ]; then
    echo "  First fail: round $FIRST_FAIL_ROUND"
fi
echo ""

if [ "$FAIL" -eq 0 ]; then
    echo "RESULT: NAT held for all $ROUNDS rounds with a persistent VM."
    echo "        VZNATNetworkDeviceAttachment is stable under this workload."
    echo "        socket_vmnet migration is NOT urgently required."
else
    echo "RESULT: NAT degraded at round $FIRST_FAIL_ROUND."
    echo "        socket_vmnet migration is RECOMMENDED."
    echo ""
    echo "  Remediation (macOS 26+): sudo launchctl kickstart -k system/com.apple.NetworkSharing"
    echo "  Remediation (macOS 13-15): sudo pfctl -f /etc/pf.conf"
    echo "  If neither works: reboot."
fi
echo "========================================"

[ "$FAIL" -eq 0 ] && exit 0 || exit 1
