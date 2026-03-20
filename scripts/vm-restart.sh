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

# Kill only the daemon for this profile by reading its pid file.
PID_FILE="$STATE_DIR/vm.pid"
if [[ -f "$PID_FILE" ]]; then
    pid="$(cat "$PID_FILE")"
    if kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null || true
        sleep 0.3
    fi
fi
rm -f "$STATE_DIR/vm.pid" "$STATE_DIR/vm.sock"

if [[ "$NUKE" -eq 1 ]]; then
    echo "=== Recreating root.img ==="
    rm -f "$REPO_ROOT/out/root.img"
    dd if=/dev/zero of="$REPO_ROOT/out/root.img" bs=1m count=0 seek=8192
fi

exec bash "$SCRIPT_DIR/vm-ping.sh" --profile "$PROFILE"
