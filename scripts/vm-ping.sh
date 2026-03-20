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
# Kernel/disk/initrd resolution (in precedence order):
#   1. vm.conf in the profile state dir (named profiles with build images)
#   2. out/vmlinuz, out/root.img, out/initramfs-custom.gz (default fallback)
#
# When a vm.conf exists for the named profile, CLI flags are NOT passed to
# avoid overriding vm.conf values.  When no vm.conf exists (e.g. test
# profiles), the out/ defaults are passed — same as the default profile.

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

# Determine whether the profile has a vm.conf that provides disk/kernel/initrd.
# Named profiles with a vm.conf (e.g. build profile) must NOT get explicit
# --kernel/--disk flags — those would override vm.conf with the wrong paths.
# Named profiles without a vm.conf (e.g. test-vm-profiles.sh test profiles)
# fall back to the out/ defaults, same as the default profile.
PELAGOS_BASE="${XDG_DATA_HOME:-$HOME/.local/share}/pelagos"
if [[ "$PROFILE" == "default" ]]; then
    VMCONF="$PELAGOS_BASE/vm.conf"
else
    VMCONF="$PELAGOS_BASE/profiles/$PROFILE/vm.conf"
fi

if [[ -f "$VMCONF" ]] && [[ "$PROFILE" != "default" ]]; then
    # Profile has a vm.conf — let the binary read kernel/disk/initrd from it.
    exec "$BINARY" \
        "${PROFILE_ARG[@]}" \
        ping
else
    # No vm.conf (or default profile) — pass out/ defaults explicitly.
    for f in "$KERNEL" "$INITRD" "$DISK"; do
        if [[ ! -f "$f" ]]; then
            echo "Missing: $f" >&2
            echo "Run 'bash scripts/build-vm-image.sh' first." >&2
            exit 1
        fi
    done
    exec "$BINARY" \
        "${PROFILE_ARG[@]}" \
        --kernel "$KERNEL" \
        --initrd "$INITRD" \
        --disk   "$DISK" \
        ping
fi
