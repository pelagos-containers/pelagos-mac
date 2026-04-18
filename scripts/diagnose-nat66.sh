#!/usr/bin/env bash
# diagnose-nat66.sh — check pelagos-pfctl LaunchDaemon status

set -uo pipefail

echo "=== pelagos-pfctl diagnostics ==="
echo ""

echo "--- binary ---"
ls -la /usr/local/lib/pelagos/pelagos-pfctl 2>/dev/null || echo "MISSING"

echo ""
echo "--- plist ---"
ls -la /Library/LaunchDaemons/com.pelagos.pfctl.plist 2>/dev/null || echo "MISSING"

echo ""
echo "--- socket ---"
ls -la /var/run/pelagos-pfctl.sock 2>/dev/null || echo "MISSING"

echo ""
echo "--- launchctl list (system) ---"
sudo launchctl list com.pelagos.pfctl 2>&1 || true

echo ""
echo "--- process ---"
pgrep -lf pelagos-pfctl || echo "not running"

echo ""
echo "--- log ---"
sudo cat /var/log/pelagos-pfctl.log 2>/dev/null || echo "(empty or missing)"

echo ""
echo "--- plist contents ---"
sudo cat /Library/LaunchDaemons/com.pelagos.pfctl.plist 2>/dev/null || echo "MISSING"
