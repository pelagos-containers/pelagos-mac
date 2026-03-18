#!/bin/bash
# Transparent wrapper around pelagos-docker that logs every invocation.
# Usage: point VS Code's dev.containers.dockerPath at this script.
# Logs go to /tmp/pd-trace.log

REAL=/Users/cb/Projects/pelagos-mac/target/aarch64-apple-darwin/release/pelagos-docker
LOG=/tmp/pd-trace.log

printf '%s CMD: %s\n' "$(date +%H:%M:%S)" "$*" >> "$LOG"
# Tee stderr to log while keeping stdout clean for VS Code to read.
"$REAL" "$@" 2> >(tee -a "$LOG" >&2) &
CHILD=$!
# Ensure child (and the tee process substitution) are killed if this wrapper
# is terminated — prevents orphaned `pelagos-docker events` polling loops.
trap 'kill -- -$$ 2>/dev/null; wait' EXIT INT TERM HUP
wait "$CHILD"
EXIT=$?
printf '%s EXIT: %d\n' "$(date +%H:%M:%S)" "$EXIT" >> "$LOG"
exit $EXIT
