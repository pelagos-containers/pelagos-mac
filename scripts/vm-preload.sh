#!/usr/bin/env bash
# vm-preload.sh — Pull all images needed for testing into the VM's image store.
#
# Run once after build-vm-image.sh or after a root.img recreation.
# After this, test scripts never hit a remote registry during normal runs.
#
# Usage:
#   bash scripts/vm-preload.sh

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
DOCKER="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos-docker"

if [ ! -f "$DOCKER" ]; then
    echo "Missing: $DOCKER" >&2
    echo "Run 'cargo build -p pelagos-mac --release && bash scripts/sign.sh' first." >&2
    exit 1
fi

IMAGES=(
    public.ecr.aws/docker/library/ubuntu:22.04
)

echo "=== Ensuring VM is running ==="
bash "$SCRIPT_DIR/vm-ping.sh"

for image in "${IMAGES[@]}"; do
    echo "=== Pulling $image ==="
    "$DOCKER" pull "$image"
done

echo "=== Preload complete ==="
