#!/usr/bin/env bash
# test-interactive.sh — Launch an interactive shell inside an Alpine container
# running in the pelagos Linux VM on macOS.
#
# Usage:
#   ./scripts/test-interactive.sh [IMAGE] [CMD...]
#
#   IMAGE   OCI image to run (default: alpine)
#   CMD     Command to exec inside the container (default: /bin/sh)
#
# Examples:
#   ./scripts/test-interactive.sh
#   ./scripts/test-interactive.sh alpine /bin/ash
#   ./scripts/test-interactive.sh alpine /bin/sh -l
#
# Prerequisites:
#   - make image   (builds out/vmlinuz, out/initramfs-custom.gz, out/root.img)
#   - make sign    (builds and signs target/aarch64-apple-darwin/release/pelagos)
#
# How it works (end-to-end):
#   Host side:
#     - Puts your terminal in raw mode so keystrokes are forwarded unmodified
#     - Sends stdin as FRAME_STDIN binary frames over vsock
#     - Receives FRAME_STDOUT and prints to your terminal
#     - Forwards SIGWINCH (terminal resize) as FRAME_RESIZE → TIOCSWINSZ in the VM
#     - Restores terminal settings and exits with the container's exit code
#
#   Guest side (inside the Linux VM):
#     - Allocates a PTY (openpty); requires /dev/pts mounted — present since PR #38
#     - Spawns `pelagos run <image> <cmd>` with the PTY slave as stdin/stdout/stderr
#     - Master-read thread forwards output as FRAME_STDOUT back to the host
#     - Stdin thread writes incoming FRAME_STDIN to the PTY master
#     - Sends FRAME_EXIT with the container exit code when the process exits
#
# TTY auto-detection note:
#   `pelagos exec` enables TTY mode automatically when stdout is a real terminal.
#   This script passes -t explicitly so behaviour is the same whether or not
#   stdout is redirected.
#
# Image caching:
#   Images are cached under /run/pelagos inside the VM (ephemeral; cleared on
#   VM restart). If the daemon is already warm from a previous run or test-e2e.sh,
#   the image pull is skipped and startup is near-instant.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
CMDLINE="console=hvc0"

IMAGE="${1:-alpine}"
shift 2>/dev/null || true
CMD=("$@")
if [ "${#CMD[@]}" -eq 0 ]; then
    CMD=(/bin/sh)
fi

# Verify required artifacts exist.
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
    exec -t "$IMAGE" "${CMD[@]}"
