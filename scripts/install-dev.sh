#!/usr/bin/env bash
# install-dev.sh — build, sign, and install a development pelagos-mac build for local testing.
#
# Builds release binaries, signs pelagos with the virtualization entitlement,
# installs them to /opt/homebrew/bin, installs the pelagos-pfctl LaunchDaemon,
# and launches pelagos-ui in dev mode.
#
# Usage:
#   bash scripts/install-dev.sh
#
# Requires: sudo (prompted once via a single sudo -v at the start)

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$SCRIPT_DIR")"
RELEASE="$REPO/target/aarch64-apple-darwin/release"

cd "$REPO"

echo "=== pelagos-mac dev install ==="

# ── 1. Build ─────────────────────────────────────────────────────────────────
echo ""
echo "--- build ---"
cargo build --release -p pelagos-mac -p pelagos-pfctl

# ── 2. Sign pelagos ───────────────────────────────────────────────────────────
echo ""
echo "--- sign ---"
bash "$SCRIPT_DIR/sign.sh"

# ── 3. Privileged install (single sudo prompt) ───────────────────────────────
echo ""
echo "--- install (requires sudo) ---"
sudo bash -s <<EOF
set -euo pipefail

# Install pelagos CLI
cp "$RELEASE/pelagos" /opt/homebrew/bin/pelagos
echo "  installed: /opt/homebrew/bin/pelagos"

# Stage pelagos-pfctl next to pelagos so 'pelagos nat66 install' can find it
cp "$RELEASE/pelagos-pfctl" /opt/homebrew/bin/pelagos-pfctl
echo "  staged:    /opt/homebrew/bin/pelagos-pfctl"

# Install the LaunchDaemon (copies binary to /usr/local/lib/pelagos/, writes plist, bootstraps)
/opt/homebrew/bin/pelagos nat66 install
EOF

# ── 4. Verify ─────────────────────────────────────────────────────────────────
echo ""
echo "--- verify ---"
echo -n "  pelagos version: "
pelagos --version

echo ""
pelagos nat66 status

echo ""
echo -n "  LaunchDaemon PID: "
launchctl list com.pelagos.pfctl 2>/dev/null | grep '"PID"' || echo "not running"

# ── 5. Point vm.conf at local out/ artifacts ─────────────────────────────────
# Rewrites vm.conf to use out/vmlinuz (Ubuntu 6.11, no RCU stalls under AVF)
# and out/initramfs-custom.gz (local guest binary + Ubuntu modules + SSH key).
# --force stops any running VM and overwrites an existing vm.conf.
echo ""
echo "--- vm init (local out/) ---"
pelagos vm init --force --vm-data "$REPO/out"

# ── 6. Start VM ───────────────────────────────────────────────────────────────
echo ""
echo "--- VM ---"
pelagos vm start && echo "  VM running" || echo "  VM already running or failed — check: pelagos vm status"

# ── 7. pelagos-ui ─────────────────────────────────────────────────────────────
UI_DIR="$HOME/Projects/pelagos-ui"
if [ -d "$UI_DIR" ]; then
    echo ""
    echo "--- pelagos-ui ---"
    echo "  Starting pelagos-ui dev server in a new Terminal window..."
    osascript -e "tell application \"Terminal\" to do script \"cd '$UI_DIR' && npm run tauri dev\""
else
    echo ""
    echo "  pelagos-ui not found at $UI_DIR — skipping"
fi

echo ""
echo "=== done ==="
echo ""
echo "Test the NAT66 toggle:"
echo "  pelagos nat66 enable"
echo "  pfctl -a com.apple/pelagos-nat66 -s nat"
echo "  pelagos nat66 disable"
echo "  pelagos nat66 status"
