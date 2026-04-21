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
#   The --cold mode stops the daemon and boots a fresh VM. NAT is provided by
#   the utun/pf relay inside pelagos-mac; the first boot may take ~2-5 s
#   for the guest network to come up.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
SHIM="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos-docker"
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
check_file "shim"      "$SHIM"

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

# Start a named detached container to exec into.
EXEC_CTR="pelagos-exec-$$"
pelagos run --detach --name "$EXEC_CTR" "$TEST_IMAGE" /bin/sh -c "sleep 30" > /dev/null 2>&1
sleep 1

# Simple exec: output from echo
OUT=$(pelagos exec "$EXEC_CTR" /bin/echo hello)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "^hello$"; then
    pass "exec: output correct"
else
    fail "exec: expected 'hello', got: $(echo "$OUT" | grep -v '^\[')"
fi

# Stdin forwarding: pipe data to cat
OUT=$(echo "hello from stdin" | pelagos exec "$EXEC_CTR" cat)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "hello from stdin"; then
    pass "exec: stdin forwarded and echoed back"
else
    fail "exec: expected 'hello from stdin', got: $(echo "$OUT" | grep -v '^\[')"
fi

pelagos stop "$EXEC_CTR" > /dev/null 2>&1 || true
pelagos rm   "$EXEC_CTR" > /dev/null 2>&1 || true

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
EXEC_CTR_TTY="pelagos-exec-tty-$$"
pelagos run --detach --name "$EXEC_CTR_TTY" "$TEST_IMAGE" /bin/sh -c "sleep 30" > /dev/null 2>&1
sleep 1
OUT=$(pelagos exec -t "$EXEC_CTR_TTY" /bin/echo hello-tty 2>&1 | tr -d '\r')
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "hello-tty"; then
    pass "exec -t: PTY output correct"
else
    fail "exec -t: expected 'hello-tty', got: $(echo "$OUT" | grep -v '^\[')"
fi
pelagos stop "$EXEC_CTR_TTY" > /dev/null 2>&1 || true
pelagos rm   "$EXEC_CTR_TTY" > /dev/null 2>&1 || true

# Stop daemon so port-forward and lifecycle tests get a clean slate.
pelagos vm stop > /dev/null 2>&1 || true
sleep 1

# ---------------------------------------------------------------------------
# Test 7e: port forwarding (-p host_port:container_port)
#
# Starts the daemon with --port 18765:8765, runs a detached container that
# listens on container port 8765 with nc, then connects from the host via
# the forwarded port 18765 and checks the relayed output.
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7e: port forwarding ==="
"$BINARY" \
    --kernel  "$KERNEL" \
    --initrd  "$INITRD" \
    --disk    "$DISK" \
    --cmdline "$CMDLINE" \
    --port    18765:8765 \
    run --detach --name pf-test "$TEST_IMAGE" \
    /bin/sh -c 'echo pf-ok | nc -l -p 8765' > /dev/null 2>&1 || true
sleep 2
PF_OUT=$(nc -w 3 127.0.0.1 18765 2>/dev/null || echo "")
if echo "$PF_OUT" | grep -q "pf-ok"; then
    pass "port forward: host:18765 → container:8765 relayed 'pf-ok'"
else
    fail "port forward: expected 'pf-ok' via host:18765, got: $PF_OUT"
fi
pelagos vm stop > /dev/null 2>&1 || true
sleep 1

# ---------------------------------------------------------------------------
# Test 7f: Ubuntu 24.04 container with apt-get (glibc + DNS)
#
# Verifies that glibc containers work and that DNS is functional inside them
# (pelagos auto-injects host DNS via per-container resolv.conf since v0.25.0).
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7f: Ubuntu 24.04 apt-get (glibc + DNS) ==="
OUT=$("$BINARY" \
    --kernel  "$KERNEL" \
    --initrd  "$INITRD" \
    --disk    "$DISK" \
    --cmdline "$CMDLINE" \
    run public.ecr.aws/docker/library/ubuntu:24.04 \
    /bin/bash -c "apt-get update -qq && echo apt-ok" 2>&1)
if echo "$OUT" | grep -q "apt-ok"; then
    pass "ubuntu 24.04: apt-get update succeeded (glibc + DNS working)"
else
    fail "ubuntu 24.04: apt-get update failed; output: $(echo "$OUT" | grep -v '^\[')"
fi

# ---------------------------------------------------------------------------
# Tests 7g-7n: pelagos-docker shim
#
# The shim auto-detects the pelagos binary (sibling in the same release dir)
# and VM artifacts (./out/).  No --kernel/--initrd/--disk flags needed.
# A clean daemon is started on first shim invocation.
# ---------------------------------------------------------------------------

# shim() wraps the binary and merges stderr so output is fully captured.
shim() { "$SHIM" "$@" 2>&1; }

echo ""
echo "=== test 7g: pelagos-docker pull ==="
# Stop any running daemon so the pull test exercises a clean warm-start.
pelagos vm stop > /dev/null 2>&1 || true
sleep 1
OUT=$(shim pull "$TEST_IMAGE")
if [ $? -eq 0 ]; then
    pass "docker pull: exited 0"
else
    fail "docker pull: non-zero exit; output: $OUT"
fi

echo ""
echo "=== test 7h: pelagos-docker run --detach ==="
SHIM_NAME="shim-test-$$"
OUT=$(shim run --name "$SHIM_NAME" --detach \
    --label "test.suite=e2e" --label "test.name=$SHIM_NAME" \
    "$TEST_IMAGE" /bin/sh -c "echo shim-hello; sleep 30")
if echo "$OUT" | grep -q "$SHIM_NAME"; then
    pass "docker run --detach: container name '$SHIM_NAME' printed"
else
    fail "docker run --detach: expected '$SHIM_NAME', got: $OUT"
fi
sleep 1

echo ""
echo "=== test 7i: pelagos-docker ps (tabular + JSON + filter) ==="
PS_TAB=$(shim ps --all)
PS_JSON=$(shim ps --all --format json)
PS_FILTER=$(shim ps --all --filter "name=$SHIM_NAME")
PSOK=0
if echo "$PS_TAB" | grep -q "$SHIM_NAME"; then
    echo "  [OK]   tabular: '$SHIM_NAME' visible"
else
    echo "  [FAIL] tabular: '$SHIM_NAME' not found; output: $PS_TAB"
    PSOK=1
fi
if echo "$PS_JSON" | python3 -c "import sys,json; rows=[json.loads(l) for l in sys.stdin if l.strip()]; assert any(r['Names']==\"$SHIM_NAME\" for r in rows)" 2>/dev/null; then
    echo "  [OK]   JSON: '$SHIM_NAME' found in JSON output"
else
    echo "  [FAIL] JSON: '$SHIM_NAME' not found in JSON; output: $PS_JSON"
    PSOK=1
fi
if echo "$PS_FILTER" | grep -q "$SHIM_NAME"; then
    echo "  [OK]   filter: --filter name= works"
else
    echo "  [FAIL] filter: --filter name=$SHIM_NAME returned nothing; output: $PS_FILTER"
    PSOK=1
fi
if [ "$PSOK" -eq 0 ]; then
    pass "docker ps: tabular, JSON, and --filter all correct"
else
    fail "docker ps: one or more checks failed (see above)"
fi

echo ""
echo "=== test 7j: pelagos-docker logs ==="
OUT=$(shim logs "$SHIM_NAME")
if echo "$OUT" | grep -q "shim-hello"; then
    pass "docker logs: 'shim-hello' present"
else
    fail "docker logs: expected 'shim-hello', got: $OUT"
fi

echo ""
echo "=== test 7k: pelagos-docker inspect (container) ==="
INSPECT=$(shim inspect --type container "$SHIM_NAME")
INS_OK=0
if echo "$INSPECT" | python3 -c "
import sys,json
data=json.load(sys.stdin)
c=data[0]
assert c['Id']=='$SHIM_NAME'
assert c['State']['Running']==True
assert c['Config']['Labels'].get('test.suite')=='e2e'
" 2>/dev/null; then
    pass "docker inspect: Id, State.Running, and label all correct"
else
    fail "docker inspect: unexpected output: $INSPECT"
fi

echo ""
echo "=== test 7l: pelagos-docker run -e (env var) ==="
OUT=$(shim run -e "SHIM_VAR=shim-env-ok" \
    "$TEST_IMAGE" /bin/sh -c 'echo "$SHIM_VAR"')
if echo "$OUT" | grep -q "shim-env-ok"; then
    pass "docker run -e: env var passed and echoed"
else
    fail "docker run -e: expected 'shim-env-ok', got: $OUT"
fi

echo ""
echo "=== test 7n: pelagos-docker stop + rm ==="
shim stop "$SHIM_NAME" > /dev/null 2>&1; STOP_EXIT=$?
if [ "$STOP_EXIT" -eq 0 ]; then
    echo "  [OK]   stop: exited 0"
else
    echo "  [FAIL] stop: exit $STOP_EXIT"
    FAIL=$((FAIL + 1))
fi
shim rm "$SHIM_NAME" > /dev/null 2>&1
OUT=$(shim ps --all)
if echo "$OUT" | grep -q "$SHIM_NAME"; then
    fail "docker rm: '$SHIM_NAME' still appears after rm"
else
    pass "docker rm: '$SHIM_NAME' gone from ps --all"
fi

echo ""
echo "=== test 7m: pelagos-docker run -v (bind mount) ==="
# Stop the daemon so this test gets a fresh one with the virtiofs mount configured.
# Tests 7h–7l ran without any -v mounts; the existing daemon has no virtio device for /shimdata.
"$BINARY" vm stop > /dev/null 2>&1 || true
sleep 1
TMPHOST=$(mktemp -d)
echo "shim-mount-ok" > "$TMPHOST/shim.txt"
OUT=$(shim run -v "$TMPHOST:/shimdata" \
    "$TEST_IMAGE" cat /shimdata/shim.txt)
rm -rf "$TMPHOST"
if echo "$OUT" | grep -q "shim-mount-ok"; then
    pass "docker run -v: bind mount visible inside container"
else
    fail "docker run -v: expected 'shim-mount-ok', got: $OUT"
fi

echo ""
echo "=== test 7o: pelagos-docker exec (non-tty) ==="
# Stop daemon so this test gets a fresh one (test 7m left daemon with a -v mount).
"$BINARY" vm stop > /dev/null 2>&1 || true
sleep 1
# Run a detached container, exec a command into it, then clean up.
EXEC_CTR="shim-exec-$$"
shim run --name "$EXEC_CTR" --detach "$TEST_IMAGE" \
    /bin/sh -c "while true; do sleep 5; done" > /dev/null 2>&1
sleep 1
OUT=$(shim exec "$EXEC_CTR" /bin/sh -c "echo exec-ok" 2>&1)
shim stop "$EXEC_CTR" > /dev/null 2>&1 || true
shim rm   "$EXEC_CTR" > /dev/null 2>&1 || true
if echo "$OUT" | grep -q "exec-ok"; then
    pass "docker exec: command ran inside container"
else
    fail "docker exec: expected 'exec-ok', got: $OUT"
fi

echo ""
echo "=== test 7p: pelagos-docker version ==="
OUT=$(shim version 2>&1)
if echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'Client' in d and 'Server' in d" 2>/dev/null; then
    pass "docker version: valid JSON with Client and Server keys"
else
    fail "docker version: unexpected output: $OUT"
fi

echo ""
echo "=== test 7q: pelagos-docker info ==="
OUT=$(shim info 2>&1)
if echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'ServerVersion' in d" 2>/dev/null; then
    pass "docker info: valid JSON with ServerVersion key"
else
    fail "docker info: unexpected output: $OUT"
fi

# ---------------------------------------------------------------------------
# Test 7r: docker build — single-stage (Dockerfile → OCI image via pelagos build)
#
# Creates a minimal single-stage Dockerfile and verifies the build succeeds.
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7r: docker build (single-stage) ==="
# Stop daemon so build test gets a fresh one (no conflicting mounts).
"$BINARY" vm stop > /dev/null 2>&1 || true
sleep 1
BUILD_CTX=$(mktemp -d)
cat > "$BUILD_CTX/Dockerfile" <<'DOCKERFILE'
FROM public.ecr.aws/docker/library/alpine:latest
RUN echo build-ok
CMD ["/bin/sh"]
DOCKERFILE
BUILD_TAG="pelagos-e2e-build-$$:latest"
OUT=$(shim build -t "$BUILD_TAG" "$BUILD_CTX" 2>&1)
BUILD_EXIT=$?
rm -rf "$BUILD_CTX"
if [ "$BUILD_EXIT" -eq 0 ]; then
    pass "docker build single-stage: exited 0"
else
    fail "docker build single-stage: exit $BUILD_EXIT; output: $OUT"
fi

# ---------------------------------------------------------------------------
# Test 7r2: docker build — multi-stage (simulates devcontainer features pattern)
#
# Verifies COPY --from=<stage> works and that all required base images are
# pulled automatically.  Uses alpine for both stages to keep the test fast.
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7r2: docker build (multi-stage) ==="
BUILD_CTX2=$(mktemp -d)
printf 'hello from feature\n' > "$BUILD_CTX2/feature.txt"
cat > "$BUILD_CTX2/Dockerfile" <<'DOCKERFILE'
FROM public.ecr.aws/docker/library/alpine:latest AS feature_source
COPY feature.txt /tmp/feature.txt

FROM public.ecr.aws/docker/library/alpine:latest AS final_stage
COPY --from=feature_source /tmp/feature.txt /feature.txt
RUN grep -q "hello from feature" /feature.txt
CMD ["/bin/sh"]
DOCKERFILE
BUILD_TAG2="pelagos-e2e-multistage-$$:latest"
OUT2=$(shim build -t "$BUILD_TAG2" "$BUILD_CTX2" 2>&1)
BUILD_EXIT2=$?
rm -rf "$BUILD_CTX2"
if [ "$BUILD_EXIT2" -eq 0 ]; then
    pass "docker build multi-stage: exited 0"
else
    fail "docker build multi-stage: exit $BUILD_EXIT2; output: $OUT2"
fi

# ---------------------------------------------------------------------------
# Test 7s: docker volume create / ls / rm round-trip
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7s: docker volume create/ls/rm ==="
# Ensure daemon is alive — test 7r stops it; a dead daemon fakes volume ops silently.
pelagos ping > /dev/null 2>&1 || true
sleep 1
VOL_NAME="pelagos-e2e-vol-$$"
shim volume create "$VOL_NAME" > /dev/null 2>&1; CREATE_EXIT=$?
LS_OUT=$(shim volume ls 2>&1)
shim volume rm "$VOL_NAME" > /dev/null 2>&1; RM_EXIT=$?
if [ "$CREATE_EXIT" -eq 0 ] && echo "$LS_OUT" | grep -q "$VOL_NAME" && [ "$RM_EXIT" -eq 0 ]; then
    pass "docker volume: create/ls/rm round-trip succeeded"
else
    fail "docker volume: create=$CREATE_EXIT rm=$RM_EXIT ls_output=$LS_OUT"
fi

# ---------------------------------------------------------------------------
# Test 7t: docker network create / ls / rm round-trip
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7t: docker network create/ls/rm ==="
NET_NAME="e2enet$$"
shim network create "$NET_NAME" > /dev/null 2>&1; CREATE_EXIT=$?
LS_OUT=$(shim network ls 2>&1)
shim network rm "$NET_NAME" > /dev/null 2>&1; RM_EXIT=$?
if [ "$CREATE_EXIT" -eq 0 ] && echo "$LS_OUT" | grep -q "$NET_NAME" && [ "$RM_EXIT" -eq 0 ]; then
    pass "docker network: create/ls/rm round-trip succeeded"
else
    fail "docker network: create=$CREATE_EXIT rm=$RM_EXIT ls_output=$LS_OUT"
fi

# ---------------------------------------------------------------------------
# Test 7u: docker cp — container→host
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7u: docker cp (container to host) ==="
CP_NAME="cpe2eu$$"
CP_OUT_DIR=$(mktemp -d)
shim run --detach --name "$CP_NAME" alpine sleep 30 > /dev/null 2>&1
sleep 2  # give container time to start
shim cp "${CP_NAME}:/etc/passwd" "$CP_OUT_DIR/" > /dev/null 2>&1; CP_FROM_EXIT=$?
shim stop "$CP_NAME" > /dev/null 2>&1
shim rm "$CP_NAME" > /dev/null 2>&1
if [ "$CP_FROM_EXIT" -eq 0 ] && [ -f "$CP_OUT_DIR/passwd" ]; then
    pass "docker cp container→host: exit=$CP_FROM_EXIT file exists"
else
    fail "docker cp container→host: exit=$CP_FROM_EXIT files=$(ls "$CP_OUT_DIR" 2>/dev/null)"
fi
rm -rf "$CP_OUT_DIR"

# ---------------------------------------------------------------------------
# Test 7v: docker cp — host→container
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7v: docker cp (host to container) ==="
CP_NAME2="cpe2ev$$"
CP_SRC=$(mktemp)
CP_TOKEN="pelagos-cp-test-$$"
echo "$CP_TOKEN" > "$CP_SRC"
CP_FNAME=$(basename "$CP_SRC")
shim run --detach --name "$CP_NAME2" alpine sleep 30 > /dev/null 2>&1
sleep 2  # give container time to start
shim cp "$CP_SRC" "${CP_NAME2}:/tmp/" > /dev/null 2>&1; CP_TO_EXIT=$?
CP_INNER=$(shim exec "$CP_NAME2" cat "/tmp/$CP_FNAME" 2>&1)
shim stop "$CP_NAME2" > /dev/null 2>&1
shim rm "$CP_NAME2" > /dev/null 2>&1
rm -f "$CP_SRC"
if [ "$CP_TO_EXIT" -eq 0 ] && [ "$CP_INNER" = "$CP_TOKEN" ]; then
    pass "docker cp host→container: exit=$CP_TO_EXIT content matched"
else
    fail "docker cp host→container: exit=$CP_TO_EXIT inner='$CP_INNER' expected='$CP_TOKEN'"
fi

# Stop daemon before lifecycle tests.
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
    echo "If image pulls are failing with 'error sending request', the utun/pf"
    echo "NAT relay may have stalled. Fix with:  pelagos vm stop && pelagos ping"
    exit 1
fi
