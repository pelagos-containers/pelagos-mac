#!/usr/bin/env bash
# vm-ssh.sh — Open an SSH session to the VM.
#
# Connects as root to the VM's dropbear sshd using the key pair generated
# by build-vm-image.sh at ~/.local/share/pelagos/vm_key.
#
# Usage:
#   ./scripts/vm-ssh.sh              — interactive root shell
#   ./scripts/vm-ssh.sh -- uname -s  — run a single command
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
    vm ssh "$@"
