#!/usr/bin/env bash
# vm-ping.sh — Start the VM daemon and verify it's responsive.
#
# Usage:
#   bash scripts/vm-ping.sh [--profile <name>]
#
# Prints "pong" on success. Safe to run repeatedly — if the daemon is already
# running this is a no-op (ensure_running detects the existing socket).
#
# --profile <name>  Use a named VM profile (isolated state dir).
#                   Default: "default" (~/.local/share/pelagos/).
#
# For the default profile, --kernel/--initrd/--disk are passed explicitly
# from out/.  For named profiles that have a vm.conf, those paths are read
# from vm.conf by the binary — do not pass flags that would override them.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"

PROFILE="default"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile)
            PROFILE="$2"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if [[ ! -f "$BINARY" ]]; then
    echo "Missing: $BINARY" >&2
    echo "Run 'cargo build -p pelagos-mac --release' and 'bash scripts/sign.sh' first." >&2
    exit 1
fi

PROFILE_ARG=()
[[ "$PROFILE" != "default" ]] && PROFILE_ARG=(--profile "$PROFILE")

# For the default profile, pass kernel/initrd/disk explicitly.
# For named profiles, vm.conf provides them — passing CLI flags would override.
if [[ "$PROFILE" == "default" ]]; then
    for f in "$KERNEL" "$INITRD" "$DISK"; do
        if [[ ! -f "$f" ]]; then
            echo "Missing: $f" >&2
            echo "Run 'bash scripts/build-vm-image.sh' first." >&2
            exit 1
        fi
    done
    exec "$BINARY" \
        --kernel "$KERNEL" \
        --initrd "$INITRD" \
        --disk   "$DISK" \
        ping
else
    exec "$BINARY" \
        "${PROFILE_ARG[@]}" \
        ping
fi
