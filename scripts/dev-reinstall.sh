#!/usr/bin/env bash
# dev-reinstall.sh -- Rebuild and reinstall all pelagos-mac components for local development.
#
# Rebuilds and installs:
#   1. pelagos-mac (macOS binaries) -- brew install from local tarball
#   2. pelagos-guest (Linux aarch64-musl) -- live-replaces running guest in the build VM
#
# Prerequisites:
#   - The build VM (profile: build, 192.168.106.2) must be running and reachable via SSH.
#   - out/ubuntu-vmlinuz, out/initramfs-custom.gz, out/root.img must exist
#     (run scripts/build-vm-image.sh if not).
#   - pelagos binary in build VM at /mnt/Projects/pelagos/target/release/pelagos
#     (build once inside VM: cd /mnt/Projects/pelagos && cargo build --release -p pelagos).
#
# Usage:
#   bash scripts/dev-reinstall.sh [--skip-mac] [--skip-guest] [--profile PROFILE]
#
# Flags:
#   --skip-mac      Skip building and reinstalling the macOS binaries.
#   --skip-guest    Skip rebuilding and restarting pelagos-guest in the build VM.
#   --profile NAME  Build VM profile name (default: build).
#
# What this script does NOT do:
#   - Tag or publish a release (use build-release.sh + GitHub Actions for that).
#   - Build the VM disk image (use build-vm-image.sh for that).
#   - Build pelagos or rusternetes inside the build VM (do those manually or via SSH).

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SKIP_MAC=0
SKIP_GUEST=0
BUILD_PROFILE="build"

while [[ $# -gt 0 ]]; do
    case $1 in
        --skip-mac)    SKIP_MAC=1;             shift ;;
        --skip-guest)  SKIP_GUEST=1;           shift ;;
        --profile)     BUILD_PROFILE="$2";     shift 2 ;;
        *) echo "Unknown argument: $1"; exit 1 ;;
    esac
done

PELAGOS_BIN="$REPO/target/aarch64-apple-darwin/release/pelagos"

# ---------------------------------------------------------------------------
# 1. macOS binaries
# ---------------------------------------------------------------------------
if [[ $SKIP_MAC -eq 0 ]]; then
    echo "[dev-reinstall] building macOS binaries..."
    # Must use rustup's cargo (not Homebrew's) for cross-compilation sysroots.
    PATH=~/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:$PATH \
        cargo build --release -p pelagos-mac -p pelagos-tui -p pelagos-pfctl

    echo "[dev-reinstall] signing..."
    bash "$REPO/scripts/sign.sh"

    echo "[dev-reinstall] building local brew tarball..."
    bash "$REPO/scripts/build-release.sh"

    echo "[dev-reinstall] installing via brew..."
    brew uninstall pelagos-mac 2>/dev/null || true
    HOMEBREW_DEVELOPER=1 HOMEBREW_NO_INSTALL_FROM_API=1 brew install pelagos-containers/tap/pelagos-mac

    echo "[dev-reinstall] macOS install done -- $(pelagos --version 2>&1 | head -1)"
fi

# ---------------------------------------------------------------------------
# 2. pelagos-guest (Linux musl, runs inside build VM)
# ---------------------------------------------------------------------------
if [[ $SKIP_GUEST -eq 0 ]]; then
    echo "[dev-reinstall] building pelagos-guest (aarch64-unknown-linux-musl)..."
    # Must use rustup's cargo and zig linker (scripts/zig-aarch64-linux-musl.sh).
    PATH=~/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:$PATH \
        cargo build --target aarch64-unknown-linux-musl --release -p pelagos-guest

    GUEST_BIN="/mnt/Projects/pelagos-mac/target/aarch64-unknown-linux-musl/release/pelagos-guest"
    PELAGOS_IN_VM="/mnt/Projects/pelagos/target/release/pelagos"

    echo "[dev-reinstall] restarting pelagos-guest in build VM (profile: $BUILD_PROFILE)..."
    "$PELAGOS_BIN" --profile "$BUILD_PROFILE" vm ssh -- bash -s <<EOF
pkill pelagos-guest 2>/dev/null || true
sleep 0.5
# Ensure pelagos is accessible for pelagos-dockerd --pelagos-bin.
if [ ! -f /usr/local/bin/pelagos ] && [ -f "$PELAGOS_IN_VM" ]; then
    ln -sf "$PELAGOS_IN_VM" /usr/local/bin/pelagos
fi
PELAGOS_BIN="${PELAGOS_IN_VM}" nohup "$GUEST_BIN" > /var/log/pelagos-guest.log 2>&1 &
disown
sleep 0.5
pgrep -a pelagos-guest
EOF

    echo "[dev-reinstall] pelagos-guest restarted."
fi

echo "[dev-reinstall] done."
