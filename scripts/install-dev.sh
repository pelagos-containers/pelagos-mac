#!/usr/bin/env bash
# install-dev.sh — build, sign, and install a development pelagos-mac build for local testing.
#
# Builds release binaries, signs pelagos with the virtualization entitlement,
# installs pelagos to /opt/homebrew/bin, updates the pelagos-pfctl LaunchDaemon,
# and starts the VM.
#
# Usage:
#   bash scripts/install-dev.sh
#
# /opt/homebrew is user-owned on Apple Silicon — no sudo needed for the copy or
# codesign steps.  Only update-pfctl-daemon.sh (LaunchDaemon install) uses sudo,
# and it prompts for the password itself.

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

# ── 2. Install pelagos CLI, then sign at the installed location ───────────────
# Sign AFTER copy: macOS 26 taskgated validates the signature at the path where
# the binary actually runs. Signing at target/ then copying invalidates it.
echo ""
echo "--- install + sign pelagos ---"
INSTALLED_PELAGOS="$(realpath /opt/homebrew/bin/pelagos 2>/dev/null || echo /opt/homebrew/bin/pelagos)"
cp "$RELEASE/pelagos" "$INSTALLED_PELAGOS"
echo "  installed: $INSTALLED_PELAGOS"
codesign --sign - --entitlements "$REPO/pelagos-mac/entitlements.plist" --force "$INSTALLED_PELAGOS"
echo "  signed:    $INSTALLED_PELAGOS"

# ── 4. Install pelagos-pfctl daemon ──────────────────────────────────────────
echo ""
echo "--- install pelagos-pfctl daemon ---"
bash "$SCRIPT_DIR/update-pfctl-daemon.sh"

# ── 5. Verify ─────────────────────────────────────────────────────────────────
echo ""
echo "--- verify ---"
echo -n "  pelagos version: "
pelagos --version

# ── 6. Point vm.conf at local out/ artifacts ─────────────────────────────────
echo ""
echo "--- vm init (local out/) ---"
pelagos vm init --force --vm-data "$REPO/out"

# ── 7. Start VM ───────────────────────────────────────────────────────────────
echo ""
echo "--- VM ---"
pelagos vm start && echo "  VM running" || echo "  VM already running or failed — check: pelagos vm status"

# ── 8. pelagos-ui ─────────────────────────────────────────────────────────────
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
