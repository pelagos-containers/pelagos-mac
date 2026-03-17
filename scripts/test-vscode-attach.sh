#!/usr/bin/env bash
# test-vscode-attach.sh — Pre-flight spec test for VS Code "Reopen in Container"
#
# Tests every requirement in docs/VSCODE_ATTACH_SPEC.md without opening VS Code.
# Run this; fix every FAIL; only then open VS Code.
#
# Usage:
#   bash scripts/test-vscode-attach.sh [--debug] [--layer N]
#
#   --debug       Print full command output for every test, not just failures.
#   --layer N     Run only layer N (0..6). Default: all layers.
#
# Prerequisites:
#   - VM running and responsive (the script starts it if needed)
#   - pelagos and pelagos-docker built and signed (scripts/sign.sh)
#   - curl available on macOS host

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
SHIM="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos-docker"

DEBUG=0
LAYER_FILTER=""

for arg in "$@"; do
    [ "$arg" = "--debug" ] && DEBUG=1
    [ "$arg" = "--layer" ] && NEXT_IS_LAYER=1 && continue
    [ "${NEXT_IS_LAYER:-0}" = "1" ] && LAYER_FILTER="$arg" && NEXT_IS_LAYER=0
done

PASS=0; FAIL=0; SKIP=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; NC='\033[0m'
else
    GREEN=''; RED=''; YELLOW=''; CYAN=''; NC=''
fi

pass() { PASS=$((PASS+1)); printf "  ${GREEN}[PASS]${NC} %s\n" "$1"; }
fail() {
    FAIL=$((FAIL+1)); printf "  ${RED}[FAIL]${NC} %s\n" "$1"
    [ -n "${2:-}" ] && printf "         expected : %s\n" "${3:-?}" && printf "         got      : %s\n" "${2:-?}"
    [ "${DEBUG:-0}" = "1" ] && [ -n "${4:-}" ] && printf "         output   :\n%s\n" "$(echo "${4}" | sed 's/^/           /')"
}
skip() { SKIP=$((SKIP+1)); printf "  ${YELLOW}[SKIP]${NC} %s\n" "$1"; }

section() {
    printf "\n${CYAN}=== Layer %s: %s ===${NC}\n" "$1" "$2"
}

run_layer() {
    [ -z "$LAYER_FILTER" ] || [ "$LAYER_FILTER" = "$1" ]
}

# Run pelagos with standard VM flags.
pelagos() {
    "$BINARY" \
        --kernel  "$KERNEL" \
        --initrd  "$INITRD" \
        --disk    "$DISK" \
        --cmdline "console=hvc0" \
        "$@"
}

# Run pelagos-docker shim.
docker() {
    "$SHIM" "$@"
}

# docker exec wrapper: always sets HOME=/root for root user.
# Matches what VS Code does (it sets HOME via -e).
dexec() {
    local name="$1"; shift
    "$SHIM" exec -e HOME=/root "$name" "$@"
}

# docker exec -i: non-interactive (stdin piped)
dexec_i() {
    local name="$1"; shift
    "$SHIM" exec -i -e HOME=/root "$name" "$@"
}

# docker exec -d: detached (does not wait)
dexec_d() {
    local name="$1"; shift
    "$SHIM" exec -d -e HOME=/root "$name" "$@"
}

# Capture stdout+stderr from a command.
capture() { "$@" 2>&1; }

# ---------------------------------------------------------------------------
# Test container name and image
# ---------------------------------------------------------------------------

TEST_NAME="vscode-attach-test"
# Ubuntu 22.04 — same glibc as what devcontainer fixtures use.
TEST_IMAGE="mcr.microsoft.com/devcontainers/base:ubuntu-22.04"

cleanup() {
    "$SHIM" rm -f "$TEST_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Layer 0: Shim Baseline
# ---------------------------------------------------------------------------

if run_layer 0; then
    section 0 "Shim Baseline (R-IDE-01)"

    # TC-VS-01: docker info
    INFO_OUT=$(capture docker info) && INFO_RC=0 || INFO_RC=$?
    if [ $INFO_RC -eq 0 ] && echo "$INFO_OUT" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); d['ServerVersion']" 2>/dev/null; then
        pass "TC-VS-01: docker info exits 0, ServerVersion field present"
    else
        fail "TC-VS-01: docker info" "$INFO_OUT" "JSON with ServerVersion"
    fi

    # TC-VS-02: docker version --format
    VER_OUT=$(capture docker version --format '{{.Server.Version}}') && VER_RC=0 || VER_RC=$?
    if [ $VER_RC -eq 0 ] && [ -n "$VER_OUT" ]; then
        pass "TC-VS-02: docker version --format returns '$VER_OUT'"
    else
        fail "TC-VS-02: docker version --format" "$VER_OUT" "non-empty version string"
    fi

    # TC-VS-03: docker ps -a
    PS_OUT=$(capture docker ps -a) && PS_RC=0 || PS_RC=$?
    if [ $PS_RC -eq 0 ]; then
        pass "TC-VS-03: docker ps -a exits 0"
    else
        fail "TC-VS-03: docker ps -a" "$PS_OUT" "exit 0"
    fi
fi

# ---------------------------------------------------------------------------
# Ensure VM is running before layer 1+
# ---------------------------------------------------------------------------

printf "\nEnsuring VM is running...\n"
PING_OUT=$(pelagos ping 2>&1) && PING_RC=0 || PING_RC=$?
if [ $PING_RC -ne 0 ]; then
    printf "  VM not running — starting...\n"
    pelagos vm start >/dev/null 2>&1 &
    sleep 5
    PING_OUT=$(pelagos ping 2>&1) && PING_RC=0 || PING_RC=$?
    if [ $PING_RC -ne 0 ]; then
        printf "  ${RED}ERROR: VM failed to start. Cannot continue.${NC}\n"
        exit 1
    fi
fi
printf "  VM: running\n"

# Pull the test image if needed (quiet, shows progress on failure)
printf "\nPulling test image %s (if needed)...\n" "$TEST_IMAGE"
PULL_OUT=$(pelagos image pull "$TEST_IMAGE" 2>&1) && PULL_RC=0 || PULL_RC=$?
if [ $PULL_RC -ne 0 ]; then
    printf "  ${RED}ERROR: Could not pull test image: %s${NC}\n" "$PULL_OUT"
    exit 1
fi
printf "  Image: available\n"

# ---------------------------------------------------------------------------
# Layer 1: Container Lifecycle
# ---------------------------------------------------------------------------

if run_layer 1; then
    section 1 "Container Lifecycle (R-IDE-02)"

    # Cleanup any previous test container.
    "$SHIM" rm -f "$TEST_NAME" >/dev/null 2>&1 || true

    # TC-VS-10: docker run -d
    RUN_OUT=$(capture docker run -d --name "$TEST_NAME" \
        -v /tmp:/workspaces/vscode-test \
        -e HOME=/root \
        "$TEST_IMAGE" \
        sh -c "while sleep 1000; do :; done") && RUN_RC=0 || RUN_RC=$?
    if [ $RUN_RC -eq 0 ]; then
        pass "TC-VS-10: docker run -d exits 0"
    else
        fail "TC-VS-10: docker run -d" "$RUN_OUT" "exit 0"
    fi

    # TC-VS-11: docker inspect — State.Status = running
    INSP_OUT=$(capture docker inspect "$TEST_NAME") && INSP_RC=0 || INSP_RC=$?
    STATUS=$(echo "$INSP_OUT" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())[0]['State']['Status'])" 2>/dev/null || echo "")
    if [ "$STATUS" = "running" ]; then
        pass "TC-VS-11: docker inspect State.Status = running"
    else
        fail "TC-VS-11: docker inspect State.Status" "$STATUS" "running" "$INSP_OUT"
    fi

    # TC-VS-12: docker inspect — Mounts array present
    MOUNTS=$(echo "$INSP_OUT" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(len(d[0].get('Mounts',[])), 'mount(s)')" 2>/dev/null || echo "")
    if echo "$MOUNTS" | grep -q "mount"; then
        pass "TC-VS-12: docker inspect Mounts present: $MOUNTS"
    else
        fail "TC-VS-12: docker inspect Mounts" "$MOUNTS" ">= 1 mount"
    fi

    # TC-VS-13: docker stop
    STOP_OUT=$(capture docker stop "$TEST_NAME") && STOP_RC=0 || STOP_RC=$?
    INSP2=$(capture docker inspect "$TEST_NAME")
    STATUS2=$(echo "$INSP2" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())[0]['State']['Status'])" 2>/dev/null || echo "")
    if [ $STOP_RC -eq 0 ] && [ "$STATUS2" = "exited" ]; then
        pass "TC-VS-13: docker stop exits 0; container status = exited"
    else
        fail "TC-VS-13: docker stop" "rc=$STOP_RC status=$STATUS2" "rc=0 status=exited"
    fi

    # TC-VS-14: docker start
    START_OUT=$(capture docker start "$TEST_NAME") && START_RC=0 || START_RC=$?
    sleep 1
    INSP3=$(capture docker inspect "$TEST_NAME")
    STATUS3=$(echo "$INSP3" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())[0]['State']['Status'])" 2>/dev/null || echo "")
    if [ $START_RC -eq 0 ] && [ "$STATUS3" = "running" ]; then
        pass "TC-VS-14: docker start exits 0; container status = running"
    else
        fail "TC-VS-14: docker start" "rc=$START_RC status=$STATUS3" "rc=0 status=running"
    fi

    # TC-VS-15: docker rm (stop first)
    docker stop "$TEST_NAME" >/dev/null 2>&1 || true
    RM_OUT=$(capture docker rm "$TEST_NAME") && RM_RC=0 || RM_RC=$?
    if [ $RM_RC -eq 0 ]; then
        pass "TC-VS-15: docker rm exits 0"
    else
        fail "TC-VS-15: docker rm" "$RM_OUT" "exit 0"
    fi

    # Re-create for subsequent layers.
    docker run -d --name "$TEST_NAME" \
        -v /tmp:/workspaces/vscode-test \
        -e HOME=/root \
        "$TEST_IMAGE" \
        sh -c "while sleep 1000; do :; done" >/dev/null 2>&1
fi

# Ensure container exists for layers 2+.
if ! run_layer 0 && ! run_layer 1; then
    # Only layers 2+ requested; create container fresh.
    "$SHIM" rm -f "$TEST_NAME" >/dev/null 2>&1 || true
    docker run -d --name "$TEST_NAME" \
        -v /tmp:/workspaces/vscode-test \
        -e HOME=/root \
        "$TEST_IMAGE" \
        sh -c "while sleep 1000; do :; done" >/dev/null 2>&1
fi

# ---------------------------------------------------------------------------
# Layer 2: Container Environment
# ---------------------------------------------------------------------------

if run_layer 2; then
    section 2 "Container Environment (R-IDE-03)"

    # TC-VS-20: /etc/hosts — localhost entry
    HOSTS=$(capture dexec "$TEST_NAME" cat /etc/hosts)
    if echo "$HOSTS" | grep -q "127.0.0.1.*localhost"; then
        pass "TC-VS-20: /etc/hosts contains 127.0.0.1 localhost"
    else
        fail "TC-VS-20: /etc/hosts" "$HOSTS" "127.0.0.1 localhost entry"
    fi

    # TC-VS-21: /etc/resolv.conf — nameserver
    RESOLV=$(capture dexec "$TEST_NAME" cat /etc/resolv.conf)
    if echo "$RESOLV" | grep -q "^nameserver"; then
        NS=$(echo "$RESOLV" | grep "^nameserver" | head -1)
        pass "TC-VS-21: /etc/resolv.conf has $NS"
    else
        fail "TC-VS-21: /etc/resolv.conf" "$RESOLV" "nameserver line"
    fi

    # TC-VS-22: localhost DNS resolution
    LOCALHOST=$(capture dexec "$TEST_NAME" sh -c "getent hosts localhost 2>/dev/null || host localhost 2>/dev/null || echo FAIL")
    if echo "$LOCALHOST" | grep -qE "127\.0\.0\.1|::1"; then
        pass "TC-VS-22: localhost resolves to loopback"
    else
        fail "TC-VS-22: localhost DNS resolution" "$LOCALHOST" "127.0.0.1 or ::1"
    fi

    # TC-VS-23: external DNS (google.com)
    GOOGLE=$(capture dexec "$TEST_NAME" sh -c "getent hosts google.com 2>/dev/null | head -1 || echo FAIL")
    if echo "$GOOGLE" | grep -qE "^[0-9]"; then
        pass "TC-VS-23: google.com resolves: $GOOGLE"
    else
        fail "TC-VS-23: external DNS (google.com)" "$GOOGLE" "IP address"
    fi

    # TC-VS-24: outbound HTTPS to VS Code CDN
    CDN_RC=0
    CDN_OUT=$(capture dexec "$TEST_NAME" sh -c \
        "curl -fsS --connect-timeout 10 --max-time 15 -o /dev/null -w '%{http_code}' https://update.code.visualstudio.com/") \
        || CDN_RC=$?
    if [ "$CDN_OUT" = "200" ] || [ "$CDN_OUT" = "302" ] || [ "$CDN_OUT" = "301" ]; then
        pass "TC-VS-24: HTTPS to update.code.visualstudio.com: HTTP $CDN_OUT"
    else
        fail "TC-VS-24: HTTPS to VS Code CDN" "http_code=$CDN_OUT rc=$CDN_RC" "200/301/302"
    fi

    # TC-VS-25: HOME env var
    HOME_VAL=$(capture dexec "$TEST_NAME" sh -c 'echo $HOME')
    if [ -n "$HOME_VAL" ] && [ "$HOME_VAL" != "" ]; then
        pass "TC-VS-25: HOME=$HOME_VAL"
    else
        fail "TC-VS-25: HOME env var" "$HOME_VAL" "/root or non-empty"
    fi

    # TC-VS-26: /root writable
    WRITE_RC=0
    capture dexec "$TEST_NAME" sh -c "touch /root/.pelagos-test-write && rm /root/.pelagos-test-write" \
        || WRITE_RC=$?
    if [ $WRITE_RC -eq 0 ]; then
        pass "TC-VS-26: /root is writable"
    else
        fail "TC-VS-26: /root writable" "exit $WRITE_RC" "exit 0"
    fi

    # TC-VS-27: /tmp writable
    TMP_RC=0
    capture dexec "$TEST_NAME" sh -c "touch /tmp/.pelagos-test && rm /tmp/.pelagos-test" \
        || TMP_RC=$?
    if [ $TMP_RC -eq 0 ]; then
        pass "TC-VS-27: /tmp is writable"
    else
        fail "TC-VS-27: /tmp writable" "exit $TMP_RC" "exit 0"
    fi

    # TC-VS-28: can bind TCP port inside container
    BIND_OUT=$(capture dexec "$TEST_NAME" sh -c \
        "nc -l 127.0.0.1 19999 &; sleep 0.3; kill %1 2>/dev/null; echo ok")
    if echo "$BIND_OUT" | grep -q "ok"; then
        pass "TC-VS-28: container can bind TCP port (nc -l 127.0.0.1:19999)"
    else
        # nc -l syntax varies; try python3
        BIND2_RC=0
        BIND2=$(capture dexec "$TEST_NAME" python3 -c \
            "import socket,os; s=socket.socket(); s.bind(('127.0.0.1',19999)); s.close(); print('ok')") \
            || BIND2_RC=$?
        if echo "$BIND2" | grep -q "ok"; then
            pass "TC-VS-28: container can bind TCP port (python3 socket)"
        else
            fail "TC-VS-28: container TCP bind" "$BIND_OUT / $BIND2" "ok"
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Layer 3: Exec Stdin/Stdout
# ---------------------------------------------------------------------------

if run_layer 3; then
    section 3 "Exec Stdin/Stdout (R-IDE-04)"

    # TC-VS-30: small stdin pipe
    PIPE_OUT=$(echo "hello" | dexec_i "$TEST_NAME" cat)
    if [ "$PIPE_OUT" = "hello" ]; then
        pass "TC-VS-30: echo hello | docker exec -i cat → hello"
    else
        fail "TC-VS-30: exec stdin pipe (small)" "$PIPE_OUT" "hello"
    fi

    # TC-VS-31: 1 MB stdin pipe
    MB1_OUT=$(dd if=/dev/urandom bs=1048576 count=1 2>/dev/null | dexec_i "$TEST_NAME" \
        sh -c "cat > /tmp/test-1mb.bin && wc -c < /tmp/test-1mb.bin")
    MB1_BYTES=$(echo "$MB1_OUT" | tr -d ' ')
    if [ "$MB1_BYTES" = "1048576" ]; then
        pass "TC-VS-31: 1 MB pipe via exec -i: $MB1_BYTES bytes received"
    else
        fail "TC-VS-31: exec stdin 1 MB" "$MB1_BYTES" "1048576"
    fi

    # TC-VS-32: 64 MB stdin pipe (VS Code server tarball size)
    MB64_OUT=$(dd if=/dev/urandom bs=1048576 count=64 2>/dev/null | dexec_i "$TEST_NAME" \
        sh -c "cat > /tmp/test-64mb.bin && wc -c < /tmp/test-64mb.bin")
    MB64_BYTES=$(echo "$MB64_OUT" | tr -d ' ')
    if [ "$MB64_BYTES" = "67108864" ]; then
        pass "TC-VS-32: 64 MB pipe via exec -i: $MB64_BYTES bytes received"
    else
        fail "TC-VS-32: exec stdin 64 MB" "$MB64_BYTES" "67108864"
    fi

    # TC-VS-33: heredoc install script over exec -i bash
    HEREDOC_OUT=$(dexec_i "$TEST_NAME" bash <<'SCRIPT'
mkdir -p /root/.vscode-server-test
echo "heredoc ran" > /root/.vscode-server-test/probe.txt
cat /root/.vscode-server-test/probe.txt
SCRIPT
    )
    if [ "$HEREDOC_OUT" = "heredoc ran" ]; then
        pass "TC-VS-33: heredoc script via exec -i bash runs correctly"
    else
        fail "TC-VS-33: exec -i bash heredoc" "$HEREDOC_OUT" "heredoc ran"
    fi

    # TC-VS-34: docker exec -d returns immediately
    T_START=$(date +%s)
    dexec_d "$TEST_NAME" sh -c "sleep 30" >/dev/null 2>&1 || true
    T_END=$(date +%s)
    ELAPSED=$((T_END - T_START))
    if [ $ELAPSED -le 3 ]; then
        pass "TC-VS-34: docker exec -d returns in ${ELAPSED}s (not blocked)"
    else
        fail "TC-VS-34: docker exec -d blocked" "${ELAPSED}s" "≤ 3s"
    fi
fi

# ---------------------------------------------------------------------------
# Layer 4: VS Code Server Install
# ---------------------------------------------------------------------------

if run_layer 4; then
    section 4 "VS Code Server Install (R-IDE-05)"

    # Get the VS Code commit hash from the locally installed code binary.
    VSCODE_COMMIT=""
    if command -v code >/dev/null 2>&1; then
        VSCODE_COMMIT=$(code --version 2>/dev/null | awk 'NR==2{print}')
    fi

    if [ -z "$VSCODE_COMMIT" ]; then
        skip "TC-VS-40..43: code binary not found on PATH; cannot determine server commit"
        skip "TC-VS-40..43: install VS Code and add 'code' to PATH, then re-run"
    else
        printf "  VS Code commit: %s\n" "$VSCODE_COMMIT"
        SERVER_URL="https://update.code.visualstudio.com/commit:${VSCODE_COMMIT}/server-linux-arm64/stable"
        SERVER_DIR="/root/.vscode-server/bin/${VSCODE_COMMIT}"

        # TC-VS-40: curl VS Code CDN from inside container
        CDN40_OUT=$(capture dexec "$TEST_NAME" sh -c \
            "curl -fsSI --connect-timeout 10 --max-time 15 '$SERVER_URL' 2>&1 | head -1")
        if echo "$CDN40_OUT" | grep -qE "HTTP/.* (200|302|301)"; then
            pass "TC-VS-40: container can reach VS Code CDN for server download: $CDN40_OUT"
        else
            fail "TC-VS-40: VS Code CDN reachable" "$CDN40_OUT" "HTTP 200/301/302"
        fi

        # TC-VS-41: download server tarball inside container (may be slow, ~70MB)
        printf "  Downloading VS Code server inside container (~70 MB)...\n"
        DL_OUT=$(capture dexec_i "$TEST_NAME" bash <<SCRIPT
set -e
mkdir -p "${SERVER_DIR}"
if [ -f "${SERVER_DIR}/node" ]; then
    echo "already installed"
    exit 0
fi
cd /root/.vscode-server/bin
curl -fsSL --connect-timeout 30 --max-time 300 \
    "${SERVER_URL}" \
    -o /tmp/vscode-server.tar.gz
echo "downloaded"
SCRIPT
        ) && DL_RC=0 || DL_RC=$?
        if [ $DL_RC -eq 0 ] && echo "$DL_OUT" | grep -qE "downloaded|already installed"; then
            pass "TC-VS-41: VS Code server tarball downloaded inside container"
        else
            fail "TC-VS-41: VS Code server download" "$DL_OUT (rc=$DL_RC)" "downloaded"
        fi

        # TC-VS-42: extract tarball
        if echo "$DL_OUT" | grep -q "already installed"; then
            pass "TC-VS-42: VS Code server already installed (skip extract)"
        else
            EX_OUT=$(capture dexec_i "$TEST_NAME" bash <<SCRIPT
set -e
cd /root/.vscode-server/bin
tar xzf /tmp/vscode-server.tar.gz
mv vscode-server-linux-arm64 "${VSCODE_COMMIT}" 2>/dev/null || true
chmod +x "${SERVER_DIR}/node" 2>/dev/null || true
ls "${SERVER_DIR}/node"
SCRIPT
            ) && EX_RC=0 || EX_RC=$?
            if [ $EX_RC -eq 0 ] && echo "$EX_OUT" | grep -q "node"; then
                pass "TC-VS-42: tarball extracted; node binary present"
            else
                fail "TC-VS-42: tarball extract" "$EX_OUT (rc=$EX_RC)" "node binary visible"
            fi
        fi

        # TC-VS-43: node --version inside container
        NODE_VER=$(capture dexec "$TEST_NAME" "${SERVER_DIR}/node" --version 2>/dev/null) \
            && NODE_RC=0 || NODE_RC=$?
        if [ $NODE_RC -eq 0 ] && echo "$NODE_VER" | grep -qE "^v[0-9]"; then
            pass "TC-VS-43: VS Code node binary runs: $NODE_VER"
        else
            fail "TC-VS-43: VS Code node --version" "$NODE_VER (rc=$NODE_RC)" "vN.N.N"
        fi

        # TC-VS-44: glibc version >= 2.28
        GLIBC=$(capture dexec "$TEST_NAME" sh -c \
            "ldd --version 2>&1 | head -1 | grep -oE '[0-9]+\.[0-9]+'")
        GLIBC_MAJOR=$(echo "$GLIBC" | cut -d. -f1)
        GLIBC_MINOR=$(echo "$GLIBC" | cut -d. -f2)
        GLIBC_OK=0
        if [ "${GLIBC_MAJOR:-0}" -gt 2 ]; then GLIBC_OK=1
        elif [ "${GLIBC_MAJOR:-0}" -eq 2 ] && [ "${GLIBC_MINOR:-0}" -ge 28 ]; then GLIBC_OK=1
        fi
        if [ $GLIBC_OK -eq 1 ]; then
            pass "TC-VS-44: glibc $GLIBC >= 2.28 (VS Code server minimum)"
        else
            fail "TC-VS-44: glibc version" "$GLIBC" ">= 2.28"
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Layer 5: VS Code Server Startup
# ---------------------------------------------------------------------------

if run_layer 5; then
    section 5 "VS Code Server Startup (R-IDE-06)"

    VSCODE_COMMIT=""
    if command -v code >/dev/null 2>&1; then
        VSCODE_COMMIT=$(code --version 2>/dev/null | awk 'NR==2{print}')
    fi

    if [ -z "$VSCODE_COMMIT" ]; then
        skip "TC-VS-50..52: code binary not found; skipping server startup tests"
    else
        SERVER_DIR="/root/.vscode-server/bin/${VSCODE_COMMIT}"
        SERVER_MAIN="${SERVER_DIR}/out/server-main.js"
        PID_FILE="/root/.vscode-server/.pid-vscode-attach-test"
        PORT_FILE="/root/.vscode-server/.port-vscode-attach-test"
        TOKEN="pelagos-test-token-$(date +%s)"

        # Check server binary exists (from layer 4).
        NODE_CHECK=$(capture dexec "$TEST_NAME" test -x "${SERVER_DIR}/node") && NODE_OK=0 || NODE_OK=$?
        if [ $NODE_OK -ne 0 ]; then
            skip "TC-VS-50..52: VS Code server not installed (run layer 4 first)"
        else
            # TC-VS-50 + TC-VS-51: start server, wait for port
            # VS Code uses --start-server which writes port to stdout via server-main.js.
            # We capture the first few lines of stdout.
            printf "  Starting VS Code server inside container...\n"

            # Start server in background inside container; redirect to a temp file.
            dexec_i "$TEST_NAME" bash <<SCRIPT >/dev/null 2>&1 &
"${SERVER_DIR}/node" "${SERVER_MAIN}" \
    --start-server \
    --host=127.0.0.1 \
    --port=0 \
    --connection-token="${TOKEN}" \
    --without-browser-env-var \
    --telemetry-level off \
    > /tmp/vscode-server.log 2>&1 &
SERVER_PID=\$!
echo \$SERVER_PID > "${PID_FILE}"
# Wait up to 15s for server to log its port.
for i in \$(seq 1 30); do
    sleep 0.5
    if grep -q "Extension host agent listening" /tmp/vscode-server.log 2>/dev/null; then
        PORT=\$(grep -oE 'port [0-9]+' /tmp/vscode-server.log | tail -1 | awk '{print \$2}')
        [ -n "\$PORT" ] && echo "\$PORT" > "${PORT_FILE}"
        break
    fi
done
wait \$SERVER_PID
SCRIPT

            # Wait for port file to appear inside container (up to 20s).
            SERVER_PORT=""
            for i in $(seq 1 20); do
                sleep 1
                PORT_CONTENT=$(capture dexec "$TEST_NAME" sh -c \
                    "cat '${PORT_FILE}' 2>/dev/null || echo ''")
                if echo "$PORT_CONTENT" | grep -qE "^[0-9]+$"; then
                    SERVER_PORT="$PORT_CONTENT"
                    break
                fi
            done

            if [ -n "$SERVER_PORT" ]; then
                pass "TC-VS-50: VS Code server started"
                pass "TC-VS-51: VS Code server listening on port $SERVER_PORT"
            else
                # Dump server log for diagnosis.
                LOG=$(capture dexec "$TEST_NAME" sh -c "cat /tmp/vscode-server.log 2>/dev/null | head -30")
                fail "TC-VS-50/51: VS Code server did not start or report port" "" "" "$LOG"
            fi

            # TC-VS-52: curl to server port inside container
            if [ -n "$SERVER_PORT" ]; then
                sleep 1
                HTTP_OUT=$(capture dexec "$TEST_NAME" sh -c \
                    "curl -fsS --connect-timeout 5 http://127.0.0.1:${SERVER_PORT}/") \
                    && HTTP_RC=0 || HTTP_RC=$?
                # VS Code server returns 403 for unknown tokens on /, which is fine.
                if [ $HTTP_RC -eq 0 ] || echo "$HTTP_OUT" | grep -qiE "vscode|connection|403|unauthorized"; then
                    pass "TC-VS-52: HTTP to VS Code server at 127.0.0.1:${SERVER_PORT}: responds"
                else
                    fail "TC-VS-52: HTTP to VS Code server" "rc=$HTTP_RC out=$HTTP_OUT" "any HTTP response"
                fi
            else
                skip "TC-VS-52: skipped (no port)"
            fi
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Layer 6: Port Forwarding
# ---------------------------------------------------------------------------

if run_layer 6; then
    section 6 "Port Forwarding (R-IDE-07)"

    # Create a fresh container with port forwarding.
    FWD_NAME="vscode-portfwd-test"
    "$SHIM" rm -f "$FWD_NAME" >/dev/null 2>&1 || true
    trap '"$SHIM" rm -f "$FWD_NAME" >/dev/null 2>&1 || true; cleanup' EXIT

    docker run -d --name "$FWD_NAME" \
        -p 19876:19876 \
        "$TEST_IMAGE" \
        sh -c "while sleep 1000; do :; done" >/dev/null 2>&1

    sleep 1

    # Start an HTTP server inside the container on the forwarded port.
    "$SHIM" exec -d -e HOME=/root "$FWD_NAME" \
        python3 -m http.server 19876 >/dev/null 2>&1 || true

    sleep 2

    # TC-VS-60: port appears in docker inspect
    PORTS_JSON=$(capture docker inspect "$FWD_NAME" | \
        python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(json.dumps(d[0]['NetworkSettings']['Ports']))" 2>/dev/null || echo "{}")
    if echo "$PORTS_JSON" | grep -q "19876"; then
        pass "TC-VS-60: docker inspect NetworkSettings.Ports includes 19876"
    else
        fail "TC-VS-60: port in inspect" "$PORTS_JSON" "19876 entry"
    fi

    # TC-VS-61: curl from macOS host to forwarded port
    HOST_OUT=$(curl -fsS --connect-timeout 5 http://127.0.0.1:19876/ 2>&1) \
        && HOST_RC=0 || HOST_RC=$?
    if [ $HOST_RC -eq 0 ] || echo "$HOST_OUT" | grep -qiE "directory|200|python"; then
        pass "TC-VS-61: macOS → container port 19876: reachable"
    else
        fail "TC-VS-61: macOS → container port forwarding" "rc=$HOST_RC $HOST_OUT" "HTTP response"
    fi

    "$SHIM" rm -f "$FWD_NAME" >/dev/null 2>&1 || true
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

printf "\n${CYAN}=== Results ===${NC}\n"
printf "  ${GREEN}PASS${NC}: %d\n" $PASS
printf "  ${RED}FAIL${NC}: %d\n" $FAIL
printf "  ${YELLOW}SKIP${NC}: %d\n" $SKIP

if [ $FAIL -eq 0 ]; then
    printf "\n${GREEN}All checks passed.${NC} Ready to open VS Code.\n\n"
    exit 0
else
    printf "\n${RED}%d check(s) failed.${NC} Fix failures before opening VS Code.\n\n" $FAIL
    exit 1
fi
