#!/usr/bin/env bash
# vm-restart.sh — Kill stale VM daemon, clean up socket/pid, and boot fresh.
#
# Usage:
#   bash scripts/vm-restart.sh           # normal restart
#   bash scripts/vm-restart.sh --nuke    # also recreate root.img (use after ext2 corruption)
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

pkill -KILL -f "pelagos.*vm-daemon-internal" 2>/dev/null || true
rm -f ~/.local/share/pelagos/vm.pid ~/.local/share/pelagos/vm.sock

echo "=== Restarting socket_vmnet (clears NAT state) ==="
sudo brew services restart socket_vmnet
sleep 2

if [[ "${1:-}" == "--nuke" ]]; then
    echo "=== Recreating root.img ==="
    rm -f "$REPO_ROOT/out/root.img"
    dd if=/dev/zero of="$REPO_ROOT/out/root.img" bs=1m count=0 seek=8192
fi

exec bash "$SCRIPT_DIR/vm-ping.sh"
