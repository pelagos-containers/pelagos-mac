#!/usr/bin/env bash
# full-rebuild.sh -- Rebuild the entire pelagos-mac stack from source.
#
# Ensures every component is up to date: pelagos runtime (musl + glibc),
# pelagos-guest, Alpine VM image (initramfs + bridge modules), macOS binaries,
# and brew installation.
#
# Build order (strict dependencies):
#   1. Build all Linux binaries in build VM:
#      - pelagos runtime (glibc, for build VM runtime use)
#      - pelagos runtime (musl, baked into Alpine initramfs)
#      - pelagos-guest (musl, baked into Alpine initramfs + hot-swapped into build VM)
#   2. Rebuild Alpine VM image (picks up fresh musl binaries + stages bridge modules)
#   3. dev-reinstall.sh (macOS binaries only + pelagos-guest hot-swap in build VM)
#   4. Restart default VM with new image
#
# Design principle: Linux binaries are built in Linux (the build VM).
# macOS binaries (pelagos-mac, pelagos-tui, pelagos-pfctl) are built on macOS.
#
# Prerequisites:
#   - Build VM (profile: build) must be running: pelagos --profile build vm start
#   - Rust toolchain installed via rustup (not Homebrew) on macOS for pelagos-mac
#   - Build VM has musl-tools + rustup musl target (provisioned automatically on first run)
#
# Usage:
#   bash scripts/full-rebuild.sh [OPTIONS]
#
# Options:
#   --skip-pelagos      Skip rebuilding pelagos runtime (use existing binaries)
#   --skip-vm-image     Skip rebuilding the Alpine VM image
#   --skip-mac          Skip rebuilding macOS binaries (passed to dev-reinstall.sh)
#   --skip-guest        Skip rebuilding pelagos-guest (passed to dev-reinstall.sh)
#   --no-restart        Do not restart the default VM after rebuild
#   --build-profile P   Build VM profile name (default: build)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
PELAGOS_REPO="$HOME/Projects/pelagos"

SKIP_PELAGOS=0
SKIP_VM_IMAGE=0
SKIP_MAC=0
SKIP_GUEST=0
NO_RESTART=0
BUILD_PROFILE="build"

while [[ $# -gt 0 ]]; do
    case $1 in
        --skip-pelagos)    SKIP_PELAGOS=1;       shift ;;
        --skip-vm-image)   SKIP_VM_IMAGE=1;      shift ;;
        --skip-mac)        SKIP_MAC=1;            shift ;;
        --skip-guest)      SKIP_GUEST=1;          shift ;;
        --no-restart)      NO_RESTART=1;          shift ;;
        --build-profile)   BUILD_PROFILE="$2";    shift 2 ;;
        *)                 echo "Unknown: $1" >&2; exit 1 ;;
    esac
done

# Use the locally-built binary if available, otherwise fall back to brew-installed.
if [[ -x "$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos" ]]; then
    PELAGOS_BIN="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
else
    PELAGOS_BIN="$(command -v pelagos 2>/dev/null || true)"
    if [[ -z "$PELAGOS_BIN" ]]; then
        echo "ABORT: no pelagos binary found (not built and not installed)" >&2
        exit 1
    fi
fi
# These paths are on the macOS filesystem but written by the build VM via virtiofs.
PELAGOS_MUSL_TARGET="$PELAGOS_REPO/target/aarch64-unknown-linux-musl/release/pelagos"
PELAGOS_DNS_MUSL_TARGET="$PELAGOS_REPO/target/aarch64-unknown-linux-musl/release/pelagos-dns"
GUEST_MUSL_TARGET="$REPO_ROOT/target/aarch64-unknown-linux-musl/release/pelagos-guest"
# Inside the build VM, source trees are at /mnt/Projects/* (virtiofs).
PELAGOS_VM_DIR="/mnt/Projects/pelagos"
PELAGOS_MAC_VM_DIR="/mnt/Projects/pelagos-mac"

# ---------------------------------------------------------------------------
# Pre-flight checks
# ---------------------------------------------------------------------------
echo ""
echo "=== full-rebuild.sh ==="
echo ""

if [[ ! -d "$PELAGOS_REPO" ]]; then
    echo "ABORT: pelagos repo not found at $PELAGOS_REPO" >&2
    exit 1
fi

# Check build VM is running (needed for glibc build + dev-reinstall guest swap).
if ! "$PELAGOS_BIN" --profile "$BUILD_PROFILE" vm status 2>/dev/null | grep -q running; then
    echo "ABORT: build VM (profile: $BUILD_PROFILE) is not running." >&2
    echo "       Start it with: pelagos --profile $BUILD_PROFILE vm start" >&2
    exit 1
fi

echo "  pelagos repo:   $PELAGOS_REPO"
echo "  build profile:  $BUILD_PROFILE"
echo "  build VM:       running"
echo ""

# ---------------------------------------------------------------------------
# Step 1: Build pelagos runtime (glibc + musl) in build VM
# ---------------------------------------------------------------------------
if [[ $SKIP_PELAGOS -eq 0 ]]; then
    echo "[1/4] Building all Linux binaries in build VM..."

    # Ensure musl-tools and the rustup musl target are available in the build VM.
    # This is idempotent -- skips if already installed.
    "$PELAGOS_BIN" --profile "$BUILD_PROFILE" vm ssh -- bash -s <<'PROVISION_EOF'
set -euo pipefail
if ! dpkg -s musl-tools >/dev/null 2>&1; then
    echo "  installing musl-tools..."
    apt-get update -qq && apt-get install -y -qq musl-tools
fi
if ! rustup target list --installed | grep -q aarch64-unknown-linux-musl; then
    echo "  adding rustup musl target..."
    rustup target add aarch64-unknown-linux-musl
fi
PROVISION_EOF

    # Build all Linux targets:
    #   pelagos glibc  -- used at runtime by pelagos-guest in the build VM
    #   pelagos musl   -- baked into the Alpine initramfs
    #   pelagos-guest musl -- baked into the Alpine initramfs + hot-swapped into build VM
    "$PELAGOS_BIN" --profile "$BUILD_PROFILE" vm ssh -- bash -s <<BUILDEOF
set -euo pipefail

echo "  building pelagos glibc (target/release)..."
cd $PELAGOS_VM_DIR
cargo build --release -p pelagos -p pelagos-dns 2>&1

echo "  building pelagos musl (target/aarch64-unknown-linux-musl/release)..."
cargo build --target aarch64-unknown-linux-musl --release -p pelagos -p pelagos-dns 2>&1

echo "  building pelagos-guest musl..."
cd $PELAGOS_MAC_VM_DIR
cargo build --target aarch64-unknown-linux-musl --release -p pelagos-guest 2>&1
BUILDEOF

    echo "  done: all Linux builds"

    # Verify the musl binaries exist on the macOS side (written via virtiofs).
    for bin in "$PELAGOS_MUSL_TARGET" "$PELAGOS_DNS_MUSL_TARGET" "$GUEST_MUSL_TARGET"; do
        if [[ ! -f "$bin" ]]; then
            echo "ABORT: expected binary not found: $bin" >&2
            exit 1
        fi
        echo "  $(ls -lh "$bin" | awk '{print $5, $6, $7, $8, $9}')"
    done
else
    echo "[1/4] Skipping pelagos rebuild (--skip-pelagos)"
fi

# ---------------------------------------------------------------------------
# Step 2: Rebuild Alpine VM image
# ---------------------------------------------------------------------------
if [[ $SKIP_VM_IMAGE -eq 0 ]]; then
    echo "[2/4] Rebuilding Alpine VM image..."
    # Delete cached initramfs to force rebuild (build-vm-image.sh skips if
    # initramfs is newer than its inputs, but we want a guaranteed fresh build).
    rm -f "$REPO_ROOT/out/initramfs-custom.gz"
    bash "$SCRIPT_DIR/build-vm-image.sh"
    echo "  done: VM image rebuilt"
else
    echo "[2/4] Skipping VM image rebuild (--skip-vm-image)"
fi

# ---------------------------------------------------------------------------
# Step 3: dev-reinstall.sh (macOS binaries + pelagos-guest)
# ---------------------------------------------------------------------------
echo "[3/4] Running dev-reinstall.sh..."
DEV_REINSTALL_ARGS=()
if [[ $SKIP_MAC -eq 1 ]]; then
    DEV_REINSTALL_ARGS+=(--skip-mac)
fi
if [[ $SKIP_GUEST -eq 1 ]]; then
    DEV_REINSTALL_ARGS+=(--skip-guest)
fi
DEV_REINSTALL_ARGS+=(--profile "$BUILD_PROFILE")
bash "$SCRIPT_DIR/dev-reinstall.sh" "${DEV_REINSTALL_ARGS[@]}"
echo "  done: dev-reinstall"

# ---------------------------------------------------------------------------
# Step 4: Restart default VM
# ---------------------------------------------------------------------------
if [[ $NO_RESTART -eq 0 ]]; then
    echo "[4/4] Restarting default VM with new image..."
    # Stop if running.
    if "$PELAGOS_BIN" vm status 2>/dev/null | grep -q running; then
        "$PELAGOS_BIN" vm stop 2>/dev/null || true
        sleep 2
    fi
    "$PELAGOS_BIN" vm start
    echo "  waiting for VM to become ready..."
    sleep 10
    # Poll for SSH readiness.
    for i in $(seq 1 30); do
        if "$PELAGOS_BIN" vm ssh -- "echo ready" 2>/dev/null | grep -q ready; then
            echo "  VM is ready."
            break
        fi
        if [[ $i -eq 30 ]]; then
            echo "  WARNING: VM did not become ready within 60s. Check manually." >&2
        fi
        sleep 2
    done
else
    echo "[4/4] Skipping VM restart (--no-restart)"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== full-rebuild.sh complete ==="
echo ""
echo "Installed versions:"
pelagos --version 2>&1 | head -1 || true
echo ""
echo "VM status:"
"$PELAGOS_BIN" vm status 2>/dev/null || true
echo ""
if [[ $NO_RESTART -eq 0 ]]; then
    echo "Ready to test. See issue #277 for the test plan."
fi
