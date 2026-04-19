#!/usr/bin/env bash
# test-utun-relay.sh — update pelagos-pfctl daemon and smoke-test the utun relay.
#
# Run as your normal user; sudo will prompt for the two privileged steps.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
RELEASE="$REPO/target/aarch64-apple-darwin/release"
PELAGOS="$RELEASE/pelagos"
PFCTL_INSTALLED="/usr/local/lib/pelagos/pelagos-pfctl"

# ── 1. Update pelagos-pfctl daemon (privileged) ─────────────────────────────
echo "==> updating pelagos-pfctl daemon..."
sudo cp "$RELEASE/pelagos-pfctl" "$PFCTL_INSTALLED"
sudo chown root:wheel "$PFCTL_INSTALLED"
sudo chmod 755 "$PFCTL_INSTALLED"
sudo launchctl kickstart -k system/com.pelagos.pfctl
sleep 1

if [ -S /var/run/pelagos-pfctl.sock ]; then
    echo "    daemon OK (socket present)"
else
    echo "ERROR: socket not present after restart — check /var/log/pelagos-pfctl.log"
    exit 1
fi

# ── 2. Stop current VM ──────────────────────────────────────────────────────
echo "==> stopping VM..."
"$PELAGOS" vm stop 2>/dev/null || true
sleep 2

# ── 3. Start VM with utun relay ─────────────────────────────────────────────
echo "==> starting VM with --relay utun..."
"$PELAGOS" --relay utun vm start

echo "==> waiting for VM (up to 60 s)..."
for i in $(seq 1 30); do
    if "$PELAGOS" --relay utun ping 2>/dev/null | grep -q pong; then
        echo "    VM up after $((i * 2)) s"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "ERROR: VM did not come up in time"
        echo "--- pelagos-pfctl log ---"
        tail -20 /var/log/pelagos-pfctl.log
        exit 1
    fi
    sleep 2
done

# ── 4. Show utun interface ───────────────────────────────────────────────────
echo ""
echo "==> utun interface (192.168.105.x):"
ifconfig | awk '/^utun/{iface=$0; next} iface && /192\.168\.105/{print iface; print; iface=""; next} iface && /^[^ \t]/{iface=""; next}'

# ── 5. pf NAT anchor ────────────────────────────────────────────────────────
echo ""
echo "==> pf NAT anchor (com.apple/pelagos-nat):"
sudo pfctl -a com.apple/pelagos-nat -s nat 2>&1 || echo "  (anchor not loaded)"

# ── 6. IPv4 connectivity ────────────────────────────────────────────────────
echo ""
echo "==> IPv4: ping google.com from container..."
"$PELAGOS" --relay utun run alpine ping -c 3 -W 2 google.com \
    && echo "  IPv4: PASS" \
    || echo "  IPv4: FAIL"

# ── 7. IPv6 connectivity ────────────────────────────────────────────────────
echo ""
echo "==> IPv6: ping6 google.com from container..."
"$PELAGOS" --relay utun run alpine ping6 -c 3 -W 3 2001:4860:4860::8888 \
    && echo "  IPv6: PASS" \
    || echo "  IPv6: FAIL"

# ── 8. SSH (phase 3) ─────────────────────────────────────────────────────────
echo ""
echo "==> SSH: direct ssh to VM..."
"$PELAGOS" --relay utun vm ssh -- echo "phase3 OK" \
    && echo "  SSH: PASS" \
    || echo "  SSH: FAIL"

echo ""
echo "==> done."
