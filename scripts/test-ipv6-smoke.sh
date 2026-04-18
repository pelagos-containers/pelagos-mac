#!/usr/bin/env bash
# test-ipv6-smoke.sh — IPv6 connectivity smoke tests for the smoltcp NAT relay.
#
# Phase 1: ICMPv6 echo to the gateway link-local address from the VM root
#          network namespace.  The gateway LL is computed dynamically from
#          the VM's own link-local (which VZ derives from the VM's random MAC
#          each boot).  Gateway formula: fe80::ff:fe<MAC[3]>:<MAC[4]><MAC[5]>
#          — identical to nat_relay's gateway_ip6_for_vm_mac() function.
#
#          Tests NDP (handled by smoltcp) + ICMPv6 echo reply synthesis.
#
# WHY vm ssh, not "pelagos run alpine":
#   ping6 to a link-local address requires scope-binding to eth0 (%eth0).
#   "pelagos run alpine" puts the process inside a container's veth network
#   namespace — that veth has no route to the relay's gateway on the VM's
#   eth0.  The ping6 must run from the VM root namespace, which vm ssh provides.
#
# Phase 2 (after initramfs rebuild with ULA prefix): uncomment the ULA tests.
#
# Usage:
#   bash scripts/test-ipv6-smoke.sh
#
# Prerequisites:
#   - cargo build --release -p pelagos-mac && bash scripts/sign.sh
#   - bash scripts/build-vm-image.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
BINARY="${BINARY:-$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos}"
KERNEL="${KERNEL:-$REPO_ROOT/out/vmlinuz}"
INITRD="${INITRD:-$REPO_ROOT/out/initramfs-custom.gz}"
DISK="${DISK:-$REPO_ROOT/out/root.img}"

# Use a dedicated profile so this test never collides with the default or
# build VM.  --profile isolates the daemon, state dir, and relay proxy port.
PROFILE="ipv6-smoke"

PASS=0
FAIL=0

pass() { echo "  [PASS] $*"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $*"; FAIL=$((FAIL + 1)); }

PELAGOS="$BINARY --profile $PROFILE --kernel $KERNEL --initrd $INITRD --disk $DISK"

echo "=== IPv6 smoke test ==="

# Stop the smoke VM when the script exits (pass or fail).
cleanup() { $BINARY --profile "$PROFILE" vm stop 2>/dev/null || true; }
trap cleanup EXIT

# --- preflight ---------------------------------------------------------------
for f in "$BINARY" "$KERNEL" "$INITRD" "$DISK"; do
    if [ ! -f "$f" ]; then
        echo "FAIL: missing $f"
        exit 1
    fi
done

# --- VM responsive -----------------------------------------------------------
printf "  ping (VM liveness)... "
if ! $PELAGOS ping 2>&1 | grep -q pong; then
    echo "FAIL (VM not responsive)"
    exit 1
fi
echo "ok"

# Wait briefly for dropbear to finish starting.
sleep 2

# --- Phase 1: link-local ping6 from VM root namespace -----------------------
# The gateway LL is dynamic: derived from the VM's random MAC each boot.
# We discover it from the VM's own link-local address via vm ssh:
#   VM LL:  fe80::<EUI-64>  e.g.  fe80::5c9b:3dff:fe2b:744b
#   GW LL:  fe80::ff:<last-32-bits-of-VM-LL>  e.g.  fe80::ff:fe2b:744b
# Both share MAC bytes [3..5] in their last 32 bits — see nat_relay.rs:
#   gateway_ip6_for_vm_mac(vm_mac) = fe80::00ff:fe{m3}:{m4}{m5}

echo ""
echo "--- Phase 1: ICMPv6 echo (link-local, VM root namespace) ---"

printf "  VM root namespace link-local addr... "
LL_OUT=$($PELAGOS vm ssh -- "ip -6 addr show dev eth0 scope link 2>/dev/null" 2>/dev/null || true)
if echo "$LL_OUT" | grep -q "fe80::"; then
    VM_LL=$(echo "$LL_OUT" | grep -o 'fe80::[^ /]*' | head -1)
    pass "VM LL: $VM_LL"
else
    fail "VM root namespace has no link-local IPv6 on eth0"
    echo ""
    echo "RESULT: $PASS passed, $FAIL failed"
    exit 1
fi

# Compute gateway LL: fe80::ff:<last-two-groups-of-VM-LL>
# awk splits on ':'; last two fields are the last 32 bits shared with the gateway.
LAST32=$(echo "$VM_LL" | awk -F: '{print $(NF-1)":"$NF}')
GW_LL="fe80::ff:${LAST32}"

printf "  gateway LL derived from VM MAC... "
pass "gateway LL: $GW_LL"

printf "  ping6 gateway link-local (%s%%eth0) from VM root ns... " "$GW_LL"
PING6_OUT=$($PELAGOS vm ssh -- "ping6 -c 3 -W 2 ${GW_LL}%eth0 2>&1" 2>/dev/null || true)
if echo "$PING6_OUT" | grep -qE "0% packet loss"; then
    pass "ping6 $GW_LL: 0% loss"
elif echo "$PING6_OUT" | grep -qE "[1-9] (packets )?received"; then
    RECEIVED=$(echo "$PING6_OUT" | grep -oE "[0-9]+ (packets )?received" | head -1)
    pass "ping6 $GW_LL: $RECEIVED (partial — acceptable)"
else
    fail "ping6 $GW_LL failed"
    echo "    output: $(echo "$PING6_OUT" | tail -3)"
fi

# --- Phase 2: ULA ping6 (uncomment after initramfs rebuild) ------------------
# GW_ULA="fd00::1"
# printf "  ping6 gateway ULA ($GW_ULA)... "
# PING6_ULA=$($PELAGOS vm ssh -- "ping6 -c 3 -W 2 $GW_ULA 2>&1" 2>/dev/null || true)
# if echo "$PING6_ULA" | grep -qE "3 received|[1-9] received"; then
#     pass "ping6 $GW_ULA: ok"
# else
#     fail "ping6 $GW_ULA failed"
# fi

# --- summary -----------------------------------------------------------------
echo ""
echo "========================================"
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo "========================================"

[ "$FAIL" -eq 0 ] && exit 0 || exit 1
