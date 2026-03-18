#!/usr/bin/env bash
# test-vscode-exec.sh — Reproduce the exact VS Code "Reopen in Container" exec pattern.
#
# VS Code runs three overlapping exec-into calls:
#   1. exec -i <ctr> /bin/sh          (long-running interactive; VS Code server installer)
#   2. exec -i -t <ctr> /bin/sh -c "apt-get ..."   (postCreateCommand; takes several seconds)
#   3. exec -i <ctr> <node> -e <TCP proxy>         (after #2 exits, while #1 still running)
#
# Step 3 fails with "no ready ack from guest" in the real VS Code attach.
# This script reproduces the pattern using nc/echo instead of node.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
DOCKER="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos-docker"
# Allow IMAGE override to work around ECR anonymous rate limits:
#   IMAGE=docker.io/library/ubuntu:22.04 bash scripts/test-vscode-exec.sh
IMAGE="${IMAGE:-public.ecr.aws/docker/library/ubuntu:22.04}"
NAME=dc-repro

cleanup() {
    echo "=== Cleanup ==="
    kill "$EXEC1_PID" 2>/dev/null || true
    kill "$KEEPALIVE_PID" 2>/dev/null || true
    "$DOCKER" rm -f "$NAME" 2>/dev/null || true
}
trap cleanup EXIT

# Clean up any leftover from a previous run.
"$DOCKER" rm -f "$NAME" 2>/dev/null || true

echo "=== Step 0: Pull image ==="
"$DOCKER" pull "$IMAGE"

echo "=== Step 1: Start keepalive container ==="
"$DOCKER" run --name "$NAME" --sig-proxy=false -a STDOUT -a STDERR \
    --entrypoint /bin/sh "$IMAGE" \
    -c 'echo started; while sleep 1 & wait $!; do :; done' &
KEEPALIVE_PID=$!

echo "Waiting for container to be running..."
sleep 4

echo "=== Step 2: exec #1 — long-running interactive (VS Code server installer) ==="
"$DOCKER" exec -i "$NAME" /bin/sh -c 'echo exec1-start; sleep 60; echo exec1-done' &
EXEC1_PID=$!

sleep 1

echo "=== Step 3: exec #2 — postCreateCommand (apt-get equivalent, takes a few seconds) ==="
"$DOCKER" exec -i -t "$NAME" /bin/sh -c 'echo postCreate-start; sleep 5; echo postCreate-done'
EXEC2_STATUS=$?
echo "exec2 (postCreateCommand): exit $EXEC2_STATUS"

echo "=== Step 4: exec #3 — TCP proxy exec (while exec #1 still running) ==="
# Simulates: exec -i <ctr> node -e <TCP proxy to 127.0.0.1:38739>
# No node binary available; we test the exec-into path itself.
"$DOCKER" exec -i "$NAME" /bin/sh -c 'echo tcp-proxy-exec-start; nc -z 127.0.0.1 38739 2>/dev/null && echo CONNECTED || echo ECONNREFUSED; echo tcp-proxy-exec-done'
EXEC3_STATUS=$?
echo "exec3 (TCP proxy): exit $EXEC3_STATUS"

echo "=== Step 5: VM health check ==="
bash "$SCRIPT_DIR/vm-ping.sh" && echo "VM: healthy" || echo "VM: DEAD"
