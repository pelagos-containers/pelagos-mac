#!/usr/bin/env bash
# vm-console.sh — Attach to the VM's hvc0 serial console.
#
# Connects to the raw serial console (hvc0) inside the VM, starting the
# daemon first if it is not already running.
# A /bin/sh loop runs on hvc0, so you drop directly into a root shell.
# Press Ctrl-] to detach without stopping the VM.
#
# Usage:
#   ./scripts/vm-console.sh
#
# Prerequisites:
#   - make image   (builds out/vmlinuz, out/initramfs-custom.gz, out/root.img)
#   - make sign    (builds and signs target/aarch64-apple-darwin/release/pelagos)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
CMDLINE="console=hvc0"

for f in "$KERNEL" "$INITRD" "$DISK" "$BINARY"; do
    if [ ! -f "$f" ]; then
        echo "Missing: $f" >&2
        echo "Run 'make image' and 'make sign' first." >&2
        exit 1
    fi
done

exec "$BINARY" \
    --kernel  "$KERNEL" \
    --initrd  "$INITRD" \
    --disk    "$DISK" \
    --cmdline "$CMDLINE" \
    vm console
