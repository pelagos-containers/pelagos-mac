#!/usr/bin/env bash
# run-integration-tests.sh — Run the pelagos integration test suite inside the
# Ubuntu build VM before pushing to CI.
#
# Usage:
#   bash scripts/run-integration-tests.sh [--filter <pattern>] [--no-network]
#
# Options:
#   --filter <pattern>  Run only tests matching <pattern> (passed to cargo test)
#   --no-network        Skip network tests (faster, no nftables/iptables needed)
#
# Prerequisites (once per VM rebuild):
#   - VM image built with `bash scripts/build-build-image.sh`
#   - Rust toolchain installed inside the VM (done by BOOTSTRAPPING.md)
#   - apt packages: rsync file nftables iptables iproute2 linux-modules-6.8.0-106-generic
#     (or whatever the running kernel version is)
#
# The script:
#   1. Starts the build VM if not running
#   2. Syncs the local pelagos source tree to the VM via rsync
#   3. Builds the test binary inside the VM (cross-compile not needed — native aarch64)
#   4. Runs the integration tests, streaming output back
#   5. Reports pass/fail summary

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PELAGOS_MAC="$(dirname "$SCRIPT_DIR")"
PELAGOS_BIN="$PELAGOS_MAC/target/aarch64-apple-darwin/release/pelagos"

# Locate the pelagos source repo (sibling of pelagos-mac)
PELAGOS_SRC="${PELAGOS_SRC:-$(dirname "$PELAGOS_MAC")/pelagos}"

if [[ ! -d "$PELAGOS_SRC" ]]; then
    echo "ERROR: pelagos source not found at $PELAGOS_SRC"
    echo "       Set PELAGOS_SRC=/path/to/pelagos to override"
    exit 1
fi

if [[ ! -x "$PELAGOS_BIN" ]]; then
    echo "ERROR: pelagos binary not found at $PELAGOS_BIN"
    echo "       Run: cargo build --release -p pelagos-mac && bash scripts/sign.sh"
    exit 1
fi

FILTER=""
SKIP_NETWORK=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --filter) FILTER="$2"; shift 2 ;;
        --no-network) SKIP_NETWORK=1; shift ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

vm_ssh() { "$PELAGOS_BIN" vm ssh --profile build -- "$@"; }

# ── 1. Ensure VM is running ──────────────────────────────────────────────────
echo "==> Checking build VM..."
STATUS=$("$PELAGOS_BIN" vm status --profile build 2>/dev/null || echo "stopped")
if [[ "$STATUS" != "running" ]]; then
    echo "==> Starting build VM..."
    "$PELAGOS_BIN" vm start --profile build
    sleep 4
fi
vm_ssh "uname -r" >/dev/null  # smoke-test SSH
echo "    Build VM ready."

# ── 2. Sync pelagos source to VM ─────────────────────────────────────────────
echo "==> Syncing pelagos source to VM..."
# Use rsync to mirror the workspace; exclude build artifacts and git history
RSYNC_EXCLUDES=(
    --exclude='.git/'
    --exclude='target/'
    --exclude='*.o'
    --exclude='*.d'
)

# rsync over ssh using the VM's guest socket
SSH_CMD="$PELAGOS_BIN vm ssh --profile build --"
rsync -az --delete "${RSYNC_EXCLUDES[@]}" \
    -e "$SSH_CMD rsync-helper-stub" \
    "$PELAGOS_SRC/" \
    "build-vm:/root/pelagos/" 2>/dev/null || {
    # Fallback: pipe a tar over SSH (no rsync daemon needed on host path)
    echo "    rsync daemon path unavailable, using tar..."
    tar -C "$PELAGOS_SRC" -czf - \
        --exclude='.git' --exclude='target' . \
        | vm_ssh "tar -C /root/pelagos -xzf -"
}
echo "    Source synced."

# ── 3. Build test binary ──────────────────────────────────────────────────────
echo "==> Building test binary on VM..."
vm_ssh "cd /root/pelagos && sudo env 'PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' cargo build --tests 2>&1"

# ── 4. Run tests ──────────────────────────────────────────────────────────────
echo "==> Running integration tests..."

SKIP_ARGS="--skip user_notif"
if [[ $SKIP_NETWORK -eq 1 ]]; then
    SKIP_ARGS="$SKIP_ARGS --skip network --skip multi_network"
fi

FILTER_ARGS=""
if [[ -n "$FILTER" ]]; then
    FILTER_ARGS="$FILTER"
fi

vm_ssh "cd /root/pelagos && sudo env 'PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' cargo test --test integration_tests -- $FILTER_ARGS $SKIP_ARGS"

echo "==> Done."
