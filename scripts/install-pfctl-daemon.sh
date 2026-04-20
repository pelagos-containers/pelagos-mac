#!/usr/bin/env bash
# install-pfctl-daemon.sh — privileged post-install step for pelagos-mac.
#
# Installs the pelagos-pfctl helper daemon and its LaunchDaemon plist, then
# loads the daemon so it is running immediately.
#
# Must be run as root (sudo).
#
# Usage (after brew install skeptomai/tap/pelagos-mac):
#   sudo bash "$(brew --prefix)/share/pelagos-mac/install-pfctl-daemon.sh"
#
# Or during development:
#   sudo bash scripts/install-pfctl-daemon.sh

set -euo pipefail

if [[ "$(id -u)" != "0" ]]; then
    echo "ERROR: must run as root: sudo bash $0"
    exit 1
fi

# ---------------------------------------------------------------------------
# Locate the pelagos-pfctl binary.
# Prefer Homebrew pkgshare, fall back to local build tree.
# ---------------------------------------------------------------------------
BREW_PREFIX="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
PKGSHARE="$BREW_PREFIX/share/pelagos-mac"

if [[ -f "$PKGSHARE/pelagos-pfctl" ]]; then
    SRC_PFCTL="$PKGSHARE/pelagos-pfctl"
    SRC_PLIST="$PKGSHARE/com.pelagos.pfctl.plist"
else
    # Development fallback — repo working copy
    REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    SRC_PFCTL="$REPO/target/aarch64-apple-darwin/release/pelagos-pfctl"
    SRC_PLIST="$REPO/scripts/com.pelagos.pfctl.plist"
    if [[ ! -f "$SRC_PFCTL" ]]; then
        echo "ERROR: pelagos-pfctl not found at $SRC_PFCTL"
        echo "       Run 'brew install skeptomai/tap/pelagos-mac' or 'cargo build -p pelagos-pfctl --release' first."
        exit 1
    fi
fi

INSTALL_DIR="/usr/local/lib/pelagos"
DAEMON_DST="$INSTALL_DIR/pelagos-pfctl"
PLIST_DST="/Library/LaunchDaemons/com.pelagos.pfctl.plist"
LABEL="com.pelagos.pfctl"

# ---------------------------------------------------------------------------
# Install binary
# ---------------------------------------------------------------------------
mkdir -p "$INSTALL_DIR"
cp "$SRC_PFCTL" "$DAEMON_DST"
chown root:wheel "$DAEMON_DST"
chmod 755 "$DAEMON_DST"
echo "==> installed $DAEMON_DST"

# ---------------------------------------------------------------------------
# Install plist
# ---------------------------------------------------------------------------
cp "$SRC_PLIST" "$PLIST_DST"
chown root:wheel "$PLIST_DST"
chmod 644 "$PLIST_DST"
echo "==> installed $PLIST_DST"

# ---------------------------------------------------------------------------
# Load / restart daemon
# ---------------------------------------------------------------------------
if launchctl print "system/$LABEL" &>/dev/null; then
    launchctl kickstart -k "system/$LABEL"
    echo "==> restarted $LABEL"
else
    launchctl bootstrap system "$PLIST_DST"
    echo "==> loaded $LABEL"
fi

sleep 1

if [[ -S /var/run/pelagos-pfctl.sock ]]; then
    echo "==> daemon OK (socket present)"
else
    echo "ERROR: socket /var/run/pelagos-pfctl.sock not present after start"
    echo "       Check /var/log/pelagos-pfctl.log for errors."
    exit 1
fi
