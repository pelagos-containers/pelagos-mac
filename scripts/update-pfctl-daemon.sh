#!/usr/bin/env bash
# update-pfctl-daemon.sh — install the locally built pelagos-pfctl daemon and restart it.
#
# The daemon plist runs /Library/PrivilegedHelperTools/com.pelagos.pfctl.
# sign.sh places the correctly-signed binary at:
#   target/aarch64-apple-darwin/release/Contents/Library/LaunchServices/com.pelagos.pfctl
# which is what this script installs.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
RELEASE="$REPO/target/aarch64-apple-darwin/release"
SRC="$RELEASE/Contents/Library/LaunchServices/com.pelagos.pfctl"
DST="/Library/PrivilegedHelperTools/com.pelagos.pfctl"

if [ ! -f "$SRC" ]; then
    echo "ERROR: signed binary not found at $SRC"
    echo "       Run 'cargo build --release -p pelagos-pfctl && bash scripts/sign.sh' first."
    exit 1
fi

echo "==> installing pelagos-pfctl..."
sudo cp "$SRC" "$DST"
sudo chown root:wheel "$DST"
sudo chmod 755 "$DST"
sudo launchctl kickstart -k system/com.pelagos.pfctl
sleep 1

if [ -S /var/run/pelagos-pfctl.sock ]; then
    echo "    daemon OK (socket present)"
else
    echo "ERROR: socket not present after restart — check logs via:"
    echo "       sudo log show --predicate 'process == \"com.pelagos.pfctl\"' --last 1m"
    exit 1
fi
