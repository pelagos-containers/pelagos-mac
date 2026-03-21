#!/usr/bin/env bash
# vm-restart.sh — Kill stale VM daemon, clean up socket/pid, and boot fresh.
#
# Usage:
#   bash scripts/vm-restart.sh [--profile <name>] [--nuke]
#
# Options:
#   --profile <name>  Target a named VM profile (default: "default").
#   --nuke            Also recreate root.img (use after disk corruption).
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

PROFILE="default"
NUKE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile) PROFILE="$2"; shift 2 ;;
        --nuke)    NUKE=1; shift ;;
        *)         echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

# Determine state dir for this profile.
PELAGOS_BASE="${XDG_DATA_HOME:-$HOME/.local/share}/pelagos"
if [[ "$PROFILE" == "default" ]]; then
    STATE_DIR="$PELAGOS_BASE"
else
    STATE_DIR="$PELAGOS_BASE/profiles/$PROFILE"
fi

# Kill ALL running pelagos daemons, not just this profile's.
# All profiles share the same socket_vmnet NAT IP (192.168.105.2); only one
# VM can hold that address at a time. If a different profile's daemon is still
# running when we start, it wins the IP and the new VM becomes unreachable.
for pid_file in \
    "$PELAGOS_BASE/vm.pid" \
    "$PELAGOS_BASE"/profiles/*/vm.pid; do
    [[ -f "$pid_file" ]] || continue
    pid="$(cat "$pid_file")"
    if kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null || true
    fi
    state_dir="$(dirname "$pid_file")"
    rm -f "$pid_file" "$state_dir/vm.sock"
done
sleep 0.3

if [[ "$NUKE" -eq 1 ]]; then
    echo "=== Recreating root.img ==="
    rm -f "$REPO_ROOT/out/root.img"
    dd if=/dev/zero of="$REPO_ROOT/out/root.img" bs=1m count=0 seek=8192
fi

exec bash "$SCRIPT_DIR/vm-ping.sh" --profile "$PROFILE"
