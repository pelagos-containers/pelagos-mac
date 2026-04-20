#!/usr/bin/env bash
# install-pfctl-daemon.sh — Install the pelagos-pfctl privileged helper daemon.
#
# Called automatically by `pelagos` via AuthorizationExecuteWithPrivileges.
# Can also be run manually as root.
#
# Usage (automatic — called by pelagos):
#   bash install-pfctl-daemon.sh <helper_binary> <launchd_plist>
#
# Usage (manual fallback — e.g. SSH session, cancelled dialog):
#   sudo bash "$(brew --prefix)/share/pelagos-mac/install-pfctl-daemon.sh"
#   sudo bash scripts/install-pfctl-daemon.sh

set -euo pipefail

if [[ "$(id -u)" != "0" ]]; then
    echo "ERROR: must run as root: sudo bash $0" >&2
    exit 1
fi

HELPER_DST="/Library/PrivilegedHelperTools/com.pelagos.pfctl"
PLIST_DST="/Library/LaunchDaemons/com.pelagos.pfctl.plist"
LABEL="com.pelagos.pfctl"

# ---------------------------------------------------------------------------
# Resolve helper binary and plist paths.
#
# When called by pelagos (AuthorizationExecuteWithPrivileges):
#   $1 = absolute path to the helper binary
#   $2 = absolute path to a pre-written launchd plist (in /tmp)
#
# When called manually (no arguments):
#   auto-detect from Homebrew pkgshare or repo working copy.
# ---------------------------------------------------------------------------
if [[ $# -ge 2 ]]; then
    SRC_PFCTL="$1"
    SRC_PLIST="$2"
else
    BREW_PREFIX="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
    PKGSHARE="$BREW_PREFIX/share/pelagos-mac"

    if [[ -f "$PKGSHARE/com.pelagos.pfctl" ]]; then
        SRC_PFCTL="$PKGSHARE/com.pelagos.pfctl"
        SRC_PLIST="$PKGSHARE/com.pelagos.pfctl.plist"
    else
        REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
        SRC_PFCTL="$REPO/target/aarch64-apple-darwin/release/Contents/Library/LaunchServices/com.pelagos.pfctl"
        SRC_PLIST="$REPO/scripts/com.pelagos.pfctl.plist"
        if [[ ! -f "$SRC_PFCTL" ]]; then
            echo "ERROR: helper binary not found at $SRC_PFCTL" >&2
            echo "       Run 'brew install skeptomai/tap/pelagos-mac' or 'scripts/sign.sh' first." >&2
            exit 1
        fi
    fi
fi

if [[ ! -f "$SRC_PFCTL" ]]; then
    echo "ERROR: helper binary not found: $SRC_PFCTL" >&2
    exit 1
fi
if [[ ! -f "$SRC_PLIST" ]]; then
    echo "ERROR: launchd plist not found: $SRC_PLIST" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Install binary
# ---------------------------------------------------------------------------
mkdir -p /Library/PrivilegedHelperTools
cp "$SRC_PFCTL" "$HELPER_DST"
chown root:wheel "$HELPER_DST"
chmod 755 "$HELPER_DST"
echo "==> installed $HELPER_DST"

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
    echo "ERROR: socket /var/run/pelagos-pfctl.sock not present after start" >&2
    echo "       Check /var/log/pelagos-pfctl.log for errors." >&2
    exit 1
fi
