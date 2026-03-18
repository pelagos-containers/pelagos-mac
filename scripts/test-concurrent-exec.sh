#!/usr/bin/env bash
# test-concurrent-exec.sh — Reproduce the concurrent exec-into crash seen during VS Code attach.
#
# Starts a keepalive container, then fires two concurrent exec-into calls against
# it (matching the pattern VS Code uses), then checks if the VM survived.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
DOCKER="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos-docker"
# Allow IMAGE override to work around ECR anonymous rate limits:
#   IMAGE=docker.io/library/ubuntu:22.04 bash scripts/test-concurrent-exec.sh
IMAGE="${IMAGE:-public.ecr.aws/docker/library/ubuntu:22.04}"
NAME=dc-repro

# Clean up any leftover from a previous run.
"$DOCKER" rm -f "$NAME" 2>/dev/null || true

echo "=== Pulling image ==="
"$DOCKER" pull "$IMAGE"

echo "=== Starting keepalive container ==="
"$DOCKER" run --name "$NAME" --sig-proxy=false -a STDOUT -a STDERR \
    --entrypoint /bin/sh "$IMAGE" \
    -c 'echo started; while sleep 1 & wait $!; do :; done' &
KEEPALIVE_PID=$!

echo "Waiting for container to be running..."
sleep 4

echo "=== Launching concurrent exec #1 (long-running, like VS Code server installer) ==="
"$DOCKER" exec -i "$NAME" /bin/sh -c 'sleep 20 && echo exec1-done' &
EXEC1_PID=$!

sleep 1

echo "=== Launching concurrent exec #2 (while #1 is running) ==="
"$DOCKER" exec -i "$NAME" /bin/sh -c 'echo exec2-done' &
EXEC2_PID=$!

echo "Waiting for execs to finish..."
wait "$EXEC1_PID" && echo "exec1: ok" || echo "exec1: FAILED"
wait "$EXEC2_PID" && echo "exec2: ok" || echo "exec2: FAILED"

echo "=== Checking VM health ==="
bash "$SCRIPT_DIR/vm-ping.sh" && echo "VM: healthy" || echo "VM: DEAD"

# Clean up.
kill "$KEEPALIVE_PID" 2>/dev/null || true
"$DOCKER" rm -f "$NAME" 2>/dev/null || true
