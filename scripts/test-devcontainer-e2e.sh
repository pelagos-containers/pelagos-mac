#!/usr/bin/env bash
# test-devcontainer-e2e.sh — T2 integration tests for pelagos devcontainer support
#
# Drives the official `devcontainer` CLI using pelagos-docker as the Docker backend.
# No VS Code. No IDE. Scriptable, deterministic, non-interactive.
#
# Governing rule (DEVCONTAINER_REQUIREMENTS.md §Governing Rule):
#   Every devcontainer requirement must be verifiable outside VS Code.
#   This script IS that verification for R-DC-01 through R-DC-04.
#
# Test scenarios:
#   Suite A — Pre-built image    (R-DC-01, R-DC-04)         fixture: dc-prebuilt
#   Suite B — Custom Dockerfile  (R-DC-02, R-DC-04)         fixture: dc-dockerfile
#   Suite C — Features           (R-DC-03, R-DC-04)         fixture: dc-features
#   Suite D — postCreateCommand  (R-DC-01 lifecycle)        fixture: dc-postcreate
#   Suite E — Idempotency        (second up reuses container) fixture: dc-prebuilt
#
# Usage:
#   bash scripts/test-devcontainer-e2e.sh [--debug] [--suite A|B|C|D|E]
#
#   --debug        Dump full devcontainer output for every test, not just failures.
#   --suite <X>    Run only one suite (A, B, C, D, or E). Default: all.
#
# Prerequisites:
#   - devcontainer CLI installed: npm install -g @devcontainers/cli
#   - VM running and responsive (the script checks via pelagos ping)
#   - pelagos and pelagos-docker built and signed (scripts/sign.sh)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
SHIM="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos-docker"
FIXTURES="$REPO_ROOT/test/fixtures"

DEBUG=0
SUITE_FILTER=""
for arg in "$@"; do
    [ "$arg" = "--debug" ] && DEBUG=1
    [ "$arg" = "--suite" ] && NEXT_IS_SUITE=1 && continue
    [ "${NEXT_IS_SUITE:-0}" = "1" ] && SUITE_FILTER="$arg" && NEXT_IS_SUITE=0
done

PASS=0
FAIL=0
SKIP=0

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
    [ -n "${2:-}" ] && printf "         expected : %s\n" "$3" && printf "         got      : %s\n" "$2"
}
skip() { SKIP=$((SKIP+1)); printf "  ${YELLOW}[SKIP]${NC} %s\n" "$1"; }

dump() {
    local label="$1"; local content="$2"
    if [ "$DEBUG" -eq 1 ] || [ "${DUMP_ON_FAIL:-0}" -eq 1 ]; then
        printf "         --- %s ---\n" "$label"
        echo "$content" | sed 's/^/         /'
        printf "         ---\n"
    fi
}

# Run pelagos with VM flags.
pelagos() {
    "$BINARY" \
        --kernel  "$KERNEL" \
        --initrd  "$INITRD" \
        --disk    "$DISK" \
        --cmdline "console=hvc0" \
        "$@" 2>&1
}

# Wrapper around pelagos-docker that logs every invocation to a temp file.
# Lets us see exactly what the devcontainer CLI calls without process tracing.
DC_INVOCATION_LOG=$(mktemp /tmp/pelagos-dc-invocationsXXXXXX)
SHIM_WRAPPER=$(mktemp /tmp/pelagos-shim-wrapperXXXXXX)
chmod +x "$SHIM_WRAPPER"
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "%s"\nexec "%s" "$@"\n' \
    "$DC_INVOCATION_LOG" "$SHIM" > "$SHIM_WRAPPER"

print_invocations() {
    printf "  docker commands sent by devcontainer CLI:\n"
    sed 's/^/    /' "$DC_INVOCATION_LOG"
    : > "$DC_INVOCATION_LOG"
}

# Run the devcontainer CLI with pelagos-docker as the Docker backend.
# --docker-path goes after the subcommand (it is a per-subcommand flag).
# DOCKER_HOST must be unset so devcontainer doesn't try to dial a daemon socket.
dc() {
    local subcmd="$1"; shift
    DOCKER_HOST="" \
    devcontainer "$subcmd" --docker-path "$SHIM_WRAPPER" "$@" 2>&1
}

# Run devcontainer up and return just the JSON result line (last line of output).
# devcontainer up streams progress lines and ends with a JSON result object.
dc_up() {
    local workspace="$1"; shift
    dc up --workspace-folder "$workspace" "$@" | tail -1
}

# Extract a field from the devcontainer up JSON result.
# Usage: dc_result_field <json> <field>
dc_result_field() {
    local json="$1" field="$2"
    echo "$json" | python3 -c "import sys,json; print(json.load(sys.stdin).get('$field',''))" 2>/dev/null
}

# Run a command inside the container for a given workspace.
# Usage: dc_exec <workspace> [--] <cmd> [args...]
dc_exec() {
    local workspace="$1"; shift
    dc exec --workspace-folder "$workspace" -- "$@"
}

# Tear down (stop + rm) all containers for a workspace by label.
# devcontainer CLI 0.84.0 has no "down" subcommand; use the shim directly.
dc_down() {
    local workspace="$1"
    local names
    names=$("$SHIM" ps -q -a \
        --filter "label=devcontainer.local_folder=$workspace" 2>/dev/null)
    for name in $names; do
        "$SHIM" stop "$name" >/dev/null 2>&1 || true
        "$SHIM" rm   "$name" >/dev/null 2>&1 || true
    done
}

# Clean up all containers from a fixture before running its suite.
cleanup_fixture() {
    local workspace="$1"
    dc_down "$workspace" 2>/dev/null || true
}

suite_active() {
    local s="$1"
    [ -z "$SUITE_FILTER" ] || [ "$SUITE_FILTER" = "$s" ]
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

echo "=== preflight ==="

MISSING=0
for f in "$KERNEL" "$INITRD" "$DISK" "$BINARY" "$SHIM"; do
    [ -f "$f" ] || { echo "  [FAIL] missing: $f"; MISSING=1; }
done
[ "$MISSING" -eq 1 ] && echo "Build and sign first. See ONGOING_TASKS.md." && exit 1

if ! command -v devcontainer >/dev/null 2>&1; then
    echo "  [FAIL] devcontainer CLI not found. Install: npm install -g @devcontainers/cli"
    exit 1
fi
echo "  [OK]   devcontainer $(devcontainer --version 2>/dev/null)"

# Check VM is up.
PING_OUT=$("$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$DISK" ping 2>&1)
if echo "$PING_OUT" | grep -q pong; then
    echo "  [OK]   VM responsive"
else
    echo "  [FAIL] VM not responding. Run: pelagos vm start (or check socket_vmnet)"
    echo "         ping output: $PING_OUT"
    exit 1
fi

echo ""

# ---------------------------------------------------------------------------
# Suite A — Pre-built image (R-DC-01, R-DC-04)
# ---------------------------------------------------------------------------

if suite_active A; then
    echo "=== suite A: pre-built image (R-DC-01, R-DC-04) ==="
    WS_A="$FIXTURES/dc-prebuilt"
    cleanup_fixture "$WS_A"

    # TC-T2-01: devcontainer up exits 0, outcome=success
    printf "  Running devcontainer up (pre-built)...\n"
    A_UP_OUT=$(dc up --workspace-folder "$WS_A" 2>&1)
    A_UP_RC=$?
    A_RESULT=$(echo "$A_UP_OUT" | tail -1)
    print_invocations
    [ "$DEBUG" -eq 1 ] && dump "devcontainer up output" "$A_UP_OUT"

    if [ "$A_UP_RC" -eq 0 ]; then
        pass "TC-T2-01: devcontainer up exit 0"
    else
        DUMP_ON_FAIL=1 dump "up output" "$A_UP_OUT"; DUMP_ON_FAIL=0
        fail "TC-T2-01: devcontainer up exit code" "$A_UP_RC" "0"
    fi

    OUTCOME=$(dc_result_field "$A_RESULT" "outcome")
    if [ "$OUTCOME" = "success" ]; then
        pass "TC-T2-01: outcome=success"
    else
        DUMP_ON_FAIL=1 dump "result JSON" "$A_RESULT"; DUMP_ON_FAIL=0
        fail "TC-T2-01: outcome" "$OUTCOME" "success"
    fi

    # TC-T2-02: exec uname -s = Linux
    UNAME=$(dc_exec "$WS_A" uname -s 2>&1)
    if [ "$UNAME" = "Linux" ]; then
        pass "TC-T2-02: exec uname -s = Linux"
    else
        DUMP_ON_FAIL=1 dump "uname output" "$UNAME"; DUMP_ON_FAIL=0
        fail "TC-T2-02: exec uname -s" "$UNAME" "Linux"
    fi

    # TC-T2-03: exec cat /etc/os-release = Ubuntu
    OS_RELEASE=$(dc_exec "$WS_A" cat /etc/os-release 2>&1)
    if echo "$OS_RELEASE" | grep -qi "ubuntu"; then
        pass "TC-T2-03: exec /etc/os-release contains Ubuntu"
    else
        DUMP_ON_FAIL=1 dump "os-release" "$OS_RELEASE"; DUMP_ON_FAIL=0
        fail "TC-T2-03: exec /etc/os-release" "$OS_RELEASE" "Ubuntu"
    fi

    # TC-T2-04: devcontainer.local_folder label present
    CONT_NAME=$(dc_result_field "$A_RESULT" "containerId")
    if [ -n "$CONT_NAME" ]; then
        LABEL_VAL=$("$SHIM" inspect "$CONT_NAME" 2>/dev/null | \
            python3 -c "import sys,json; print(json.load(sys.stdin)[0]['Config']['Labels'].get('devcontainer.local_folder',''))" 2>/dev/null)
        if [ "$LABEL_VAL" = "$WS_A" ]; then
            pass "TC-T2-04: devcontainer.local_folder label = $WS_A"
        else
            fail "TC-T2-04: devcontainer.local_folder label" "$LABEL_VAL" "$WS_A"
        fi
    else
        skip "TC-T2-04: containerId not in result JSON (cannot verify label)"
    fi

    # TC-T2-05: second devcontainer up reuses container (idempotency)
    printf "  Running devcontainer up (second time, idempotency)...\n"
    A_UP2_OUT=$(dc up --workspace-folder "$WS_A" 2>&1)
    A_UP2_RC=$?
    A_RESULT2=$(echo "$A_UP2_OUT" | tail -1)
    [ "$DEBUG" -eq 1 ] && dump "second up output" "$A_UP2_OUT"

    OUTCOME2=$(dc_result_field "$A_RESULT2" "outcome")
    if [ "$A_UP2_RC" -eq 0 ] && [ "$OUTCOME2" = "success" ]; then
        pass "TC-T2-05: second devcontainer up: exit 0, outcome=success"
    else
        DUMP_ON_FAIL=1 dump "second up" "$A_UP2_OUT"; DUMP_ON_FAIL=0
        fail "TC-T2-05: second devcontainer up" "exit=$A_UP2_RC outcome=$OUTCOME2" "exit=0 outcome=success"
    fi

    CONT2=$(dc_result_field "$A_RESULT2" "containerId")
    if [ -n "$CONT_NAME" ] && [ -n "$CONT2" ] && [ "$CONT_NAME" = "$CONT2" ]; then
        pass "TC-T2-05: second up reuses same container (${CONT_NAME})"
    elif [ -z "$CONT_NAME" ] || [ -z "$CONT2" ]; then
        skip "TC-T2-05: containerId not in result JSON (cannot verify reuse)"
    else
        fail "TC-T2-05: container reuse" "$CONT2" "$CONT_NAME (same as first)"
    fi

    dc_down "$WS_A"
    echo ""
fi

# ---------------------------------------------------------------------------
# Suite B — Custom Dockerfile (R-DC-02, R-DC-04)
# ---------------------------------------------------------------------------

if suite_active B; then
    echo "=== suite B: custom Dockerfile (R-DC-02, R-DC-04) ==="
    WS_B="$FIXTURES/dc-dockerfile"
    cleanup_fixture "$WS_B"

    printf "  Running devcontainer up (custom Dockerfile)...\n"
    B_UP_OUT=$(dc up --workspace-folder "$WS_B" 2>&1)
    B_UP_RC=$?
    B_RESULT=$(echo "$B_UP_OUT" | tail -1)
    [ "$DEBUG" -eq 1 ] && dump "devcontainer up output" "$B_UP_OUT"

    # TC-T2-06: devcontainer up with Dockerfile exits 0
    if [ "$B_UP_RC" -eq 0 ] && [ "$(dc_result_field "$B_RESULT" "outcome")" = "success" ]; then
        pass "TC-T2-06: devcontainer up (custom Dockerfile): exit 0, outcome=success"
    else
        DUMP_ON_FAIL=1 dump "up output" "$B_UP_OUT"; DUMP_ON_FAIL=0
        fail "TC-T2-06: devcontainer up (custom Dockerfile)" \
             "exit=$B_UP_RC outcome=$(dc_result_field "$B_RESULT" "outcome")" "exit=0 outcome=success"
    fi

    # TC-T2-07: marker file from RUN step exists in container
    MARKER=$(dc_exec "$WS_B" cat /pelagos-marker.txt 2>&1)
    if echo "$MARKER" | grep -q "pelagos-dockerfile-build"; then
        pass "TC-T2-07: Dockerfile RUN step ran: /pelagos-marker.txt present"
    else
        DUMP_ON_FAIL=1 dump "marker" "$MARKER"; DUMP_ON_FAIL=0
        fail "TC-T2-07: Dockerfile marker in container" "$MARKER" "pelagos-dockerfile-build"
    fi

    # TC-T2-07b: curl installed by apt-get in Dockerfile RUN step
    # Requires DNS to work inside pelagos build RUN containers (pasta networking).
    # Blocked on pelagos issue #102 (DNS in build RUN steps).
    CURL_VER=$(dc_exec "$WS_B" curl --version 2>&1 | head -1)
    if echo "$CURL_VER" | grep -qi "curl"; then
        pass "TC-T2-07b: curl from Dockerfile apt-get: $CURL_VER"
    else
        fail "TC-T2-07b: curl installed by Dockerfile apt-get (blocked: pelagos#102 DNS in RUN)" "$CURL_VER" "curl ..."
    fi

    dc_down "$WS_B"
    echo ""
fi

# ---------------------------------------------------------------------------
# Suite C — Features (R-DC-03, R-DC-04)
# ---------------------------------------------------------------------------

if suite_active C; then
    echo "=== suite C: devcontainer features (R-DC-03, R-DC-04) ==="
    WS_C="$FIXTURES/dc-features"
    cleanup_fixture "$WS_C"

    printf "  Running devcontainer up (features: node:lts)...\n"
    printf "  (This builds a multi-stage feature Dockerfile — may be slow on first run)\n"
    C_UP_OUT=$(dc up --workspace-folder "$WS_C" 2>&1)
    C_UP_RC=$?
    C_RESULT=$(echo "$C_UP_OUT" | tail -1)
    [ "$DEBUG" -eq 1 ] && dump "devcontainer up output" "$C_UP_OUT"

    # TC-T2-10: devcontainer up with features exits 0
    if [ "$C_UP_RC" -eq 0 ] && [ "$(dc_result_field "$C_RESULT" "outcome")" = "success" ]; then
        pass "TC-T2-10: devcontainer up (node feature): exit 0, outcome=success"
    else
        DUMP_ON_FAIL=1 dump "up output" "$C_UP_OUT"; DUMP_ON_FAIL=0
        fail "TC-T2-10: devcontainer up (node feature)" \
             "exit=$C_UP_RC outcome=$(dc_result_field "$C_RESULT" "outcome")" "exit=0 outcome=success"
    fi

    # TC-T2-10b: node binary installed by feature
    NODE_VER=$(dc_exec "$WS_C" node --version 2>&1)
    if echo "$NODE_VER" | grep -qE "^v[0-9]+"; then
        pass "TC-T2-10b: node installed by feature: $NODE_VER"
    else
        DUMP_ON_FAIL=1 dump "node output" "$NODE_VER"; DUMP_ON_FAIL=0
        fail "TC-T2-10b: node installed by feature" "$NODE_VER" "v<N>.x.x"
    fi

    # TC-T2-10c: npm also available
    NPM_VER=$(dc_exec "$WS_C" npm --version 2>&1)
    if echo "$NPM_VER" | grep -qE "^[0-9]+\.[0-9]+"; then
        pass "TC-T2-10c: npm installed: $NPM_VER"
    else
        fail "TC-T2-10c: npm installed" "$NPM_VER" "N.N.N"
    fi

    dc_down "$WS_C"
    echo ""
fi

# ---------------------------------------------------------------------------
# Suite D — postCreateCommand (R-DC-01 lifecycle)
# ---------------------------------------------------------------------------

if suite_active D; then
    echo "=== suite D: postCreateCommand (R-DC-01 lifecycle) ==="
    WS_D="$FIXTURES/dc-postcreate"
    cleanup_fixture "$WS_D"

    printf "  Running devcontainer up (postCreateCommand)...\n"
    D_UP_OUT=$(dc up --workspace-folder "$WS_D" 2>&1)
    D_UP_RC=$?
    D_RESULT=$(echo "$D_UP_OUT" | tail -1)
    [ "$DEBUG" -eq 1 ] && dump "devcontainer up output" "$D_UP_OUT"

    # TC-T2-08: devcontainer up exits 0
    if [ "$D_UP_RC" -eq 0 ] && [ "$(dc_result_field "$D_RESULT" "outcome")" = "success" ]; then
        pass "TC-T2-08: devcontainer up (postCreateCommand): exit 0, outcome=success"
    else
        DUMP_ON_FAIL=1 dump "up output" "$D_UP_OUT"; DUMP_ON_FAIL=0
        fail "TC-T2-08: devcontainer up (postCreateCommand)" \
             "exit=$D_UP_RC outcome=$(dc_result_field "$D_RESULT" "outcome")" "exit=0 outcome=success"
    fi

    # TC-T2-08b: postCreateCommand ran (marker file exists)
    MARKER=$(dc_exec "$WS_D" test -f /tmp/pelagos-postcreate-ran '&&' echo exists 2>&1)
    if echo "$MARKER" | grep -q "exists"; then
        pass "TC-T2-08b: postCreateCommand ran: /tmp/pelagos-postcreate-ran exists"
    else
        # Also check if it appeared in devcontainer output
        if echo "$D_UP_OUT" | grep -q "postcreate-done"; then
            pass "TC-T2-08b: postCreateCommand ran: 'postcreate-done' in up output"
        else
            DUMP_ON_FAIL=1 dump "up output" "$D_UP_OUT"; DUMP_ON_FAIL=0
            fail "TC-T2-08b: postCreateCommand ran" "$MARKER" "exists (marker file)"
        fi
    fi

    # TC-T2-09: teardown removes container from ps -a
    dc_down "$WS_D"
    sleep 1
    REMAINING=$("$SHIM" ps -q -a \
        --filter "label=devcontainer.local_folder=$WS_D" 2>/dev/null)
    if [ -z "$REMAINING" ]; then
        pass "TC-T2-09: teardown: container removed from ps -a"
    else
        fail "TC-T2-09: teardown" "$REMAINING" "(empty)"
    fi

    echo ""
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo "========================================"
TOTAL=$((PASS + FAIL + SKIP))
if [ "$FAIL" -eq 0 ]; then
    printf "${GREEN}PASS${NC}  %d passed" "$PASS"
    [ "$SKIP" -gt 0 ] && printf ", %d skipped" "$SKIP"
    printf " / %d total\n" "$TOTAL"
    exit 0
else
    printf "${RED}FAIL${NC}  %d failed, %d passed" "$FAIL" "$PASS"
    [ "$SKIP" -gt 0 ] && printf ", %d skipped" "$SKIP"
    printf " / %d total\n" "$TOTAL"
    printf "\nRe-run with --debug for full devcontainer output.\n"
    exit 1
fi
