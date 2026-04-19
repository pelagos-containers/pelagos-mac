#!/usr/bin/env bash
# update-pfctl-daemon.sh — install the locally built pelagos-pfctl daemon and restart it.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
RELEASE="$REPO/target/aarch64-apple-darwin/release"

echo "==> installing pelagos-pfctl..."
sudo cp "$RELEASE/pelagos-pfctl" /usr/local/lib/pelagos/pelagos-pfctl
sudo chown root:wheel /usr/local/lib/pelagos/pelagos-pfctl
sudo chmod 755 /usr/local/lib/pelagos/pelagos-pfctl
sudo launchctl kickstart -k system/com.pelagos.pfctl
sleep 1

if [ -S /var/run/pelagos-pfctl.sock ]; then
    echo "    daemon OK (socket present)"
else
    echo "ERROR: socket not present after restart — check /var/log/pelagos-pfctl.log"
    exit 1
fi
