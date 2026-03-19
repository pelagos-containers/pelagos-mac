#!/bin/bash
# Transparent wrapper around pelagos-docker that logs every invocation.
# Usage: point VS Code's dev.containers.dockerPath at this script.
# Logs go to /tmp/pd-trace.log

REAL=/Users/cb/Projects/pelagos-mac/target/aarch64-apple-darwin/release/pelagos-docker
LOG=/tmp/pd-trace.log
export RUST_LOG=warn

printf '%s CMD: %s\n' "$(date +%H:%M:%S)" "$*" >> "$LOG"
# For exec commands with no -c flag (bootstrap execs), also capture stdin
# (the muxrpc channel from VS Code) so we can diagnose connection failures.
if [[ "$1" == "exec" && "$*" != *" -c "* ]]; then
  STDIN_LOG=/tmp/pd-exec-stdin-$$.bin
  printf '%s BOOTSTRAP_EXEC: stdin->%s\n' "$(date +%H:%M:%S)" "$STDIN_LOG" >> "$LOG"
  # stdbuf -i0 -o0 forces unbuffered tee so muxrpc bytes reach pelagos-docker
  # without waiting for an 8k buffer fill.
  "$REAL" "$@" < <(stdbuf -i0 -o0 tee "$STDIN_LOG") 2> >(tee -a "$LOG" >&2) &
else
  "$REAL" "$@" <&0 2> >(tee -a "$LOG" >&2) &
fi
CHILD=$!
# Ensure child (and the tee process substitution) are killed if this wrapper
# is terminated — prevents orphaned `pelagos-docker events` polling loops.
trap 'kill "$CHILD" 2>/dev/null; wait "$CHILD" 2>/dev/null' EXIT INT TERM HUP
wait "$CHILD"
EXIT=$?
printf '%s EXIT: %d\n' "$(date +%H:%M:%S)" "$EXIT" >> "$LOG"
exit $EXIT
