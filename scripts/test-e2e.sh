#!/usr/bin/env bash
# test-e2e.sh — End-to-end integration tests for pelagos-mac.
#
# Usage:
#   ./test-e2e.sh             — functional tests (auto-starts daemon if needed)
#   ./test-e2e.sh --cold      — stop daemon first, then run functional + daemon lifecycle tests
#   ./test-e2e.sh --warm      — skip functional tests; only verify warm-reuse timing + lifecycle
#
# Prerequisites:
#   - make image   (builds out/vmlinuz, out/initramfs-custom.gz, out/root.img)
#   - make sign    (builds and signs target/aarch64-apple-darwin/release/pelagos)
#
# Cold-start timing note:
#   The --cold mode measures a "warm-NAT cold start" — the daemon is stopped
#   and a fresh VM is booted, but AVF NAT (InternetSharing / bridge100) is
#   still warm from the previous session.  In this state the ping gate inside
#   the VM succeeds on the first try (~100 ms) so the total cold start is
#   ~1-2 s.  On a truly fresh macOS login the first boot can take up to ~50 s
#   while the ping gate waits for NAT to come up.  If that happens, run:
#     sudo pfctl -f /etc/pf.conf
#   to reset PF and let InternetSharing re-establish cleanly.
#
# If image pulls fail with "error sending request", PF state has degraded.
# Fix with: sudo pfctl -f /etc/pf.conf

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
CMDLINE="console=hvc0"

# AWS ECR Public hosts Docker Official Images with no unauthenticated pull rate limits.
TEST_IMAGE="public.ecr.aws/docker/library/alpine"

MODE="functional"
if [ "${1:-}" = "--cold" ]; then MODE="cold"; fi
if [ "${1:-}" = "--warm" ]; then MODE="warm"; fi

PASS=0
FAIL=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

pelagos() {
    "$BINARY" \
        --kernel  "$KERNEL" \
        --initrd  "$INITRD" \
        --disk    "$DISK" \
        --cmdline "$CMDLINE" \
        "$@" 2>&1
}

pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1"; }

check_file() {
    if [ -f "$2" ]; then
        echo "  [OK]   $1: $2"
    else
        echo "  [FAIL] $1 missing: $2"
        FAIL=$((FAIL + 1))
    fi
}

# Milliseconds since epoch — portable via python3 (macOS date lacks %N).
ms_now() { python3 -c "import time; print(int(time.time() * 1000))"; }

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

echo "=== preflight (mode: $MODE) ==="
check_file "kernel"    "$KERNEL"
check_file "initramfs" "$INITRD"
check_file "disk"      "$DISK"
check_file "binary"    "$BINARY"

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "FAIL: preflight failed. Run 'make image' and 'make sign' first."
    exit 1
fi

# ---------------------------------------------------------------------------
# Cold-mode setup: stop any running daemon so test 1 measures a real cold start
# ---------------------------------------------------------------------------

if [ "$MODE" = "cold" ]; then
    echo ""
    echo "=== cold-mode setup: stopping any running daemon ==="
    pelagos vm stop > /dev/null 2>&1 || true
    sleep 1
    OUT=$(pelagos vm status 2>&1 || true)
    if echo "$OUT" | grep -q "stopped"; then
        echo "  [OK]   daemon stopped"
    else
        echo "  [WARN] vm status: $OUT (may already be stopped)"
    fi
fi

# ---------------------------------------------------------------------------
# Functional tests (skipped in --warm mode)
# ---------------------------------------------------------------------------

if [ "$MODE" != "warm" ]; then

# ---------------------------------------------------------------------------
# Test 1: ping (cold-start timing captured in cold mode)
# ---------------------------------------------------------------------------

echo ""
echo "=== test 1: ping ==="
if [ "$MODE" = "cold" ]; then
    T0=$(ms_now); OUT=$(pelagos ping); T1=$(ms_now)
    COLD_MS=$(( T1 - T0 ))
    echo "  [TIME] cold-start ping: ${COLD_MS} ms"
else
    OUT=$(pelagos ping)
fi
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "^pong$"; then
    pass "ping returned 'pong'"
else
    fail "ping did not return 'pong' (got: $(echo "$OUT" | grep -v '^\['))"
fi

# ---------------------------------------------------------------------------
# Test 2: echo hello
# ---------------------------------------------------------------------------

echo ""
echo "=== test 2: run alpine /bin/echo hello ==="
OUT=$(pelagos run "$TEST_IMAGE" /bin/echo hello)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "^hello$"; then
    pass "output contains 'hello'"
else
    fail "expected 'hello', got: $(echo "$OUT" | grep -v '^\[')"
fi

# ---------------------------------------------------------------------------
# Test 3: sh -c (hyphen arg passthrough)
# ---------------------------------------------------------------------------

echo ""
echo "=== test 3: run alpine /bin/sh -c 'echo foo; echo bar' ==="
OUT=$(pelagos run "$TEST_IMAGE" /bin/sh -c "echo foo; echo bar")
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "^foo$" && echo "$OUT" | grep -q "^bar$"; then
    pass "output contains 'foo' and 'bar'"
else
    fail "expected 'foo' and 'bar', got: $(echo "$OUT" | grep -v '^\[')"
fi

# ---------------------------------------------------------------------------
# Test 4: non-zero exit propagation
# ---------------------------------------------------------------------------

echo ""
echo "=== test 4: exit code propagation ==="
pelagos run "$TEST_IMAGE" /bin/false > /dev/null 2>&1; EXIT=$?
if [ "$EXIT" -eq 1 ]; then
    pass "exit code 1 propagated correctly"
else
    fail "expected exit 1, got $EXIT"
fi

# ---------------------------------------------------------------------------
# Test 5: back-to-back runs
# ---------------------------------------------------------------------------

echo ""
echo "=== test 5: three back-to-back runs ==="
BACK_FAIL=0
for i in 1 2 3; do
    OUT=$(pelagos run "$TEST_IMAGE" /bin/echo "run$i")
    if echo "$OUT" | grep -q "^run${i}$"; then
        echo "  [OK]   run $i: ok"
    else
        echo "  [FAIL] run $i: expected 'run${i}', got: $(echo "$OUT" | grep -v '^\[')"
        BACK_FAIL=$((BACK_FAIL + 1))
    fi
done
if [ "$BACK_FAIL" -eq 0 ]; then
    pass "all 3 back-to-back runs succeeded"
else
    fail "$BACK_FAIL of 3 back-to-back runs failed"
fi

# ---------------------------------------------------------------------------
# Test 6: virtiofs bind mount
#
# Mounts are fixed at daemon startup.  Stop the currently-running daemon
# (started without -v by tests 1-5), boot a fresh one with -v, run the
# mount test, then stop it so the lifecycle tests (7-9) can restart fresh.
# ---------------------------------------------------------------------------

echo ""
echo "=== test 6: virtiofs bind mount ==="
# Stop the no-mount daemon started by tests 1-5.
pelagos vm stop > /dev/null 2>&1 || true
sleep 1
TMPHOST=$(mktemp -d)
echo "hello from host" > "$TMPHOST/hello.txt"
OUT=$(pelagos -v "$TMPHOST:/data" run "$TEST_IMAGE" cat /data/hello.txt)
rm -rf "$TMPHOST"
if echo "$OUT" | grep -q "hello from host"; then
    pass "virtiofs mount: file visible inside container"
else
    fail "virtiofs mount failed; output: $(echo "$OUT" | grep -v '^\[')"
fi
# Stop the mount-enabled daemon so lifecycle tests get a clean slate.
pelagos vm stop > /dev/null 2>&1 || true
sleep 1

# ---------------------------------------------------------------------------
# Test 7: exec (non-TTY stdin forwarding)
#
# Restart daemon (no mounts) before exec tests.
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7: exec (non-tty) ==="
# Ensure daemon is running (no mounts needed for exec).
pelagos ping > /dev/null 2>&1 || true
sleep 1

# Simple exec: output from echo
OUT=$(pelagos exec "$TEST_IMAGE" /bin/echo hello)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "^hello$"; then
    pass "exec: output correct"
else
    fail "exec: expected 'hello', got: $(echo "$OUT" | grep -v '^\[')"
fi

# Stdin forwarding: pipe data to cat
OUT=$(echo "hello from stdin" | pelagos exec "$TEST_IMAGE" cat)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "hello from stdin"; then
    pass "exec: stdin forwarded and echoed back"
else
    fail "exec: expected 'hello from stdin', got: $(echo "$OUT" | grep -v '^\[')"
fi

# ---------------------------------------------------------------------------
# Test 7a: vm shell (non-TTY) — shell directly in the VM, not in a container
#
# Runs `uname -s` inside the VM shell and checks output is "Linux".
# This confirms: (a) GuestCommand::Shell is dispatched correctly, and
# (b) we are in the raw VM environment (not inside a container namespace).
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7a: vm shell (non-tty) ==="
OUT=$(pelagos vm shell <<'VMEOF'
uname -s
VMEOF
)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "^Linux$"; then
    pass "vm shell: 'uname -s' returned 'Linux' (we are in the VM)"
else
    fail "vm shell: expected 'Linux', got: $(echo "$OUT" | grep -v '^\[')"
fi

# ---------------------------------------------------------------------------
# Test 7b: vm console (non-TTY smoke test)
#
# Sends a command to the VM serial console via a pipe and checks the output.
# The getty runs /bin/sh on hvc0; we send 'uname -s\nexit\n' and expect "Linux".
# Uses printf to send input so the shell exits cleanly (no interactive mode).
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7b: vm console (non-tty) ==="
OUT=$(printf 'uname -s\nexit\n' | pelagos vm console 2>/dev/null | tr -d '\r' | grep -v '^\[')
if echo "$OUT" | grep -q "Linux"; then
    pass "vm console: 'uname -s' returned 'Linux'"
else
    fail "vm console: expected 'Linux', got: $OUT"
fi

# ---------------------------------------------------------------------------
# Test 7c: vm ssh (non-interactive command)
#
# Runs `uname -s` over SSH into the VM and checks output is "Linux".
# VM must be running (started by test 7a); key is at ~/.local/share/pelagos/vm_key.
# Uses LogLevel=ERROR so SSH doesn't print banner/warning lines.
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7c: vm ssh (non-interactive) ==="
OUT=$("$BINARY" \
    --kernel  "$KERNEL" \
    --initrd  "$INITRD" \
    --disk    "$DISK" \
    --cmdline "$CMDLINE" \
    vm ssh -- uname -s 2>/dev/null)
if echo "$OUT" | grep -q "^Linux$"; then
    pass "vm ssh: 'uname -s' returned 'Linux'"
else
    fail "vm ssh: expected 'Linux', got: $OUT"
fi

# ---------------------------------------------------------------------------
# Test 7d: exec with explicit -t (TTY / PTY mode)
#
# PTY output uses \r\n line endings; strip \r before comparing.
# This test catches two failure modes that are invisible in non-TTY context:
#   1. /dev/pts not mounted in the VM (openpty returns ENOENT)
#   2. TTY auto-detection logic choosing wrong mode
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7d: exec -t (tty mode) ==="
OUT=$(pelagos exec -t "$TEST_IMAGE" /bin/echo hello-tty 2>&1 | tr -d '\r')
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "hello-tty"; then
    pass "exec -t: PTY output correct"
else
    fail "exec -t: expected 'hello-tty', got: $(echo "$OUT" | grep -v '^\[')"
fi

# Stop daemon so lifecycle tests get a clean slate.
pelagos vm stop > /dev/null 2>&1 || true
sleep 1

# ---------------------------------------------------------------------------
# Tests 8-13: container lifecycle (ps, logs, stop, rm, --name, --detach)
#
# Requires the daemon to be running without mounts; restarts clean after
# test 7 stopped it.
# ---------------------------------------------------------------------------

echo ""
echo "=== test 8: run --detach --name ==="
# Start daemon (no mounts) before detach tests.
pelagos ping > /dev/null 2>&1 || true
sleep 1
LC_NAME="pelagos-lc-test-$$"
OUT=$(pelagos run --detach --name "$LC_NAME" "$TEST_IMAGE" /bin/sh -c "echo lc-output; sleep 15")
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "$LC_NAME"; then
    pass "run --detach: printed container name '${LC_NAME}'"
else
    fail "run --detach: expected container name '${LC_NAME}', got: $(echo "$OUT" | grep -v '^\[')"
fi

echo ""
echo "=== test 9: ps shows running container ==="
# Give the detached container a moment to register.
sleep 1
OUT=$(pelagos ps)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "$LC_NAME"; then
    pass "ps: running container '${LC_NAME}' visible"
else
    fail "ps: expected '${LC_NAME}', got: $(echo "$OUT" | grep -v '^\[')"
fi

echo ""
echo "=== test 10: logs show container output ==="
OUT=$(pelagos logs "$LC_NAME")
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "lc-output"; then
    pass "logs: 'lc-output' present in logs"
else
    fail "logs: expected 'lc-output', got: $(echo "$OUT" | grep -v '^\[')"
fi

echo ""
echo "=== test 11: stop container ==="
pelagos stop "$LC_NAME"; STOP_EXIT=$?
# pelagos stop returns 0 on success (output may vary)
if [ "$STOP_EXIT" -eq 0 ]; then
    pass "stop: exited 0"
else
    fail "stop: non-zero exit (got $STOP_EXIT)"
fi

echo ""
echo "=== test 12: ps --all shows exited container ==="
sleep 1
OUT=$(pelagos ps --all)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "$LC_NAME"; then
    pass "ps --all: stopped container '${LC_NAME}' still visible"
else
    fail "ps --all: expected '${LC_NAME}', got: $(echo "$OUT" | grep -v '^\[')"
fi

echo ""
echo "=== test 13: rm removes container ==="
pelagos rm "$LC_NAME" > /dev/null 2>&1
OUT=$(pelagos ps --all)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "$LC_NAME"; then
    fail "rm: '${LC_NAME}' still appears after rm"
else
    pass "rm: '${LC_NAME}' no longer in ps --all"
fi

# Stop daemon so lifecycle tests get a clean slate.
pelagos vm stop > /dev/null 2>&1 || true
sleep 1

fi  # end of functional tests

# ---------------------------------------------------------------------------
# Daemon lifecycle tests (cold and warm modes)
# ---------------------------------------------------------------------------

if [ "$MODE" = "cold" ] || [ "$MODE" = "warm" ]; then

# In cold mode, test 13 stopped the daemon.  Restart it (no mounts) so the
# lifecycle tests have a running daemon to inspect and stop.
if [ "$MODE" = "cold" ]; then
    echo ""
    echo "=== restarting daemon (no mounts) for lifecycle tests ==="
    pelagos ping > /dev/null 2>&1 || true  # triggers daemon start
    sleep 1
fi

# ---------------------------------------------------------------------------
# Test 14: vm status reports running
# ---------------------------------------------------------------------------

echo ""
echo "=== test 14: vm status ==="
OUT=$(pelagos vm status 2>&1 || true)
echo "  $OUT"
if echo "$OUT" | grep -q "running"; then
    pass "vm status reports running"
else
    fail "expected 'running', got: $OUT"
fi

# ---------------------------------------------------------------------------
# Test 15: warm ping is fast (daemon already running)
# ---------------------------------------------------------------------------

echo ""
echo "=== test 15: warm ping latency ==="
T0=$(ms_now); OUT=$(pelagos ping); T1=$(ms_now)
WARM_MS=$(( T1 - T0 ))
echo "$OUT" | grep -v "^\["
echo "  [TIME] warm ping: ${WARM_MS} ms"
if echo "$OUT" | grep -q "^pong$"; then
    # Warm run should complete in under 3 seconds (generous allowance)
    if [ "$WARM_MS" -lt 3000 ]; then
        pass "warm ping: ${WARM_MS} ms (< 3000 ms)"
    else
        fail "warm ping too slow: ${WARM_MS} ms (expected < 3000 ms)"
    fi
else
    fail "warm ping did not return 'pong'"
fi

if [ "$MODE" = "cold" ]; then
    echo "  [INFO] cold-start: ${COLD_MS} ms  →  warm: ${WARM_MS} ms"
fi

# ---------------------------------------------------------------------------
# Test 16: vm stop + verify stopped
# ---------------------------------------------------------------------------

echo ""
echo "=== test 16: vm stop ==="
pelagos vm stop > /dev/null 2>&1
sleep 2
OUT=$(pelagos vm status 2>&1 || true)
echo "  $OUT"
if echo "$OUT" | grep -q "stopped"; then
    pass "daemon stopped after 'vm stop'"
else
    fail "expected 'stopped', got: $OUT"
fi

fi  # end of daemon lifecycle tests

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "================================"
if [ "$FAIL" -eq 0 ]; then
    echo "PASS  ($PASS tests passed)"
    exit 0
else
    echo "FAIL  ($FAIL failed, $PASS passed)"
    echo ""
    echo "If image pulls are failing with 'error sending request', PF state"
    echo "has degraded. Fix with:  sudo pfctl -f /etc/pf.conf"
    exit 1
fi
