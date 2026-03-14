#!/usr/bin/env bash
# test-devcontainer-shim.sh — pelagos-docker devcontainer compatibility test harness
#
# Tests the pelagos-docker shim against the real lifecycle the devcontainer CLI
# uses, without requiring VS Code. Each test checks one specific contract from
# docs/DEVCONTAINER_REQUIREMENTS.md and prints full diagnostic output on failure.
#
# Lifecycle under test (correct post-keepalive-removal flow):
#   Phase 1  pre-flight commands (version, context, buildx, info)
#   Phase 2  volume CRUD
#   Phase 3  ps label filtering (empty — no containers yet)
#   Phase 4  probe run → container exits → ps -a finds it (R-SH-02, R-SH-03)
#   Phase 5  docker inspect on EXITED container (R-SH-04, R-SH-05)
#   Phase 6  docker start → container becomes RUNNING (R-VM-01 via pelagos start)
#   Phase 7  docker inspect on RUNNING container (State.Running=true)
#   Phase 8  docker exec into running container (R-SH-06)
#   Phase 9  interactive shell server pattern (R-SH-06)
#   Phase 10 system-config patching (R-SH-06)
#   Phase 11 timing: ps immediately after run (no sleep) (R-VM-02, R-SH-03)
#   Phase 12 multi-label AND filter (R-SH-03)
#   Phase 13 mount path translation (R-SH-05, host path not /mnt/...)
#   Phase 14 inspect field types (Env=array, Running=bool, Ports=object)
#   Phase 15 docker stop + rm lifecycle cleanup
#
# Usage:
#   bash scripts/test-devcontainer-shim.sh [--debug]
#
#   --debug   Dump full output for every test, not just failures.
#
# Prerequisites:
#   - VM running (the script checks and errors if not)
#   - pelagos and pelagos-docker built and signed (scripts/sign.sh)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
SHIM="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos-docker"

DEBUG=0
for arg in "$@"; do [ "$arg" = "--debug" ] && DEBUG=1; done

PASS=0
FAIL=0
SKIP=0

# Container name used throughout the session.
CNAME=""
WORKSPACE_FOLDER="$REPO_ROOT"
DC_CONFIG="$REPO_ROOT/.devcontainer/devcontainer.json"
DC_IMAGE="public.ecr.aws/docker/library/ubuntu:22.04"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Colours only if a terminal.
if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[0;33m'; NC='\033[0m'
else
    GREEN=''; RED=''; YELLOW=''; NC=''
fi

pass() {
    PASS=$((PASS + 1))
    printf "  ${GREEN}[PASS]${NC} %s\n" "$1"
}

fail() {
    FAIL=$((FAIL + 1))
    printf "  ${RED}[FAIL]${NC} %s\n" "$1"
    if [ -n "${2:-}" ]; then
        printf "         expected : %s\n" "$3"
        printf "         got      : %s\n" "$2"
    fi
}

skip() {
    SKIP=$((SKIP + 1))
    printf "  ${YELLOW}[SKIP]${NC} %s\n" "$1"
}

# Print a labelled block of output — always in debug mode, only on failure otherwise.
dump() {
    local label="$1"; local content="$2"
    if [ "$DEBUG" -eq 1 ] || [ "${DUMP_ON_FAIL:-0}" -eq 1 ]; then
        printf "         --- %s ---\n" "$label"
        echo "$content" | sed 's/^/         /'
        printf "         ---\n"
    fi
}

# Run via the shim, capturing combined stdout+stderr.
shim() { "$SHIM" "$@" 2>&1; }

# Run via the shim, stderr to /dev/null (for commands where we only want stdout).
shim_stdout() { "$SHIM" "$@" 2>/dev/null; }

# Run pelagos with VM flags.
pelagos() {
    "$BINARY" \
        --kernel  "$KERNEL" \
        --initrd  "$INITRD" \
        --disk    "$DISK" \
        --cmdline "console=hvc0" \
        "$@" 2>&1
}

# Assert json field equals expected value.  Usage: assert_json_field LABEL JSON PATH EXPECTED
assert_json_field() {
    local label="$1" json="$2" path="$3" expected="$4"
    local got
    got=$(echo "$json" | python3 -c "
import sys, json
data = json.load(sys.stdin)
parts = '$path'.split('.')
v = data
for p in parts:
    if isinstance(v, list): v = v[int(p)] if p.isdigit() else v[0]
    else: v = v[p]
print(json.dumps(v) if not isinstance(v, str) else v)
" 2>/dev/null)
    if [ "$got" = "$expected" ]; then
        pass "$label = $expected"
    else
        DUMP_ON_FAIL=1 dump "JSON" "$json"
        fail "$label" "$got" "$expected"
        DUMP_ON_FAIL=0
    fi
}

# ---------------------------------------------------------------------------
# Preflight checks
# ---------------------------------------------------------------------------

echo "=== preflight ==="

MISSING=0
for f in "$KERNEL" "$INITRD" "$DISK" "$BINARY" "$SHIM"; do
    if [ -f "$f" ]; then
        [ "$DEBUG" -eq 1 ] && echo "  [OK]   $(basename "$f")"
    else
        echo "  [FAIL] missing: $f"
        MISSING=1
    fi
done
[ "$MISSING" -eq 1 ] && echo "Build and sign first. See ONGOING_TASKS.md Build Reference." && exit 1

# Wait for the VM to respond.
# pelagos ping calls ensure_running() which boots the VM if needed (up to 60s).
# Run it in the background and print dots so the user knows it's working.
PING_TMP=$(mktemp /tmp/pelagos-ping-XXXXXX)
pelagos ping >"$PING_TMP" 2>&1 &
PING_PID=$!
printf "  Waiting for VM (first boot ~20s)"
while kill -0 "$PING_PID" 2>/dev/null; do
    printf "."
    sleep 1
done
printf "\n"
wait "$PING_PID"; PING_RC=$?
if [ "$PING_RC" -eq 0 ] && grep -q pong "$PING_TMP" 2>/dev/null; then
    echo "  [OK]   VM responding"
else
    echo "  [FAIL] VM did not respond (exit $PING_RC). Output: $(cat "$PING_TMP")"
    echo "         Check: sudo brew services list | grep socket_vmnet"
    rm -f "$PING_TMP"
    exit 1
fi
rm -f "$PING_TMP"

# Clean up any leftover test containers from a previous aborted run.
shim stop "dc-shimtest"   >/dev/null 2>&1 || true
shim rm   "dc-shimtest"   >/dev/null 2>&1 || true
shim stop "dc-timertest"  >/dev/null 2>&1 || true
shim rm   "dc-timertest"  >/dev/null 2>&1 || true
shim stop "dc-multilabel" >/dev/null 2>&1 || true
shim rm   "dc-multilabel" >/dev/null 2>&1 || true
shim stop "dc-pathlabel"  >/dev/null 2>&1 || true
shim rm   "dc-pathlabel"  >/dev/null 2>&1 || true
shim stop "dc-nolabel"    >/dev/null 2>&1 || true
shim rm   "dc-nolabel"    >/dev/null 2>&1 || true
shim volume rm "vsc-shimtest" >/dev/null 2>&1 || true
echo "  [OK]   cleaned up any leftovers"

# ---------------------------------------------------------------------------
# Phase 1 — pre-flight commands (R-SH-01)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 1: pre-flight commands ==="

# docker -v
OUT=$(shim -v)
if echo "$OUT" | grep -qiE "docker|version|pelagos"; then
    pass "docker -v: '$OUT'"
else
    fail "docker -v" "$OUT" "string containing docker/version/pelagos"
fi

# docker version → JSON with Client + Server
OUT=$(shim version)
if echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'Client' in d and 'Server' in d" 2>/dev/null; then
    pass "docker version: valid JSON with Client and Server"
else
    DUMP_ON_FAIL=1 dump "version output" "$OUT"; DUMP_ON_FAIL=0
    fail "docker version" "$OUT" "JSON {Client:..., Server:...}"
fi

# docker version --format {{.Server.Version}}
OUT=$(shim version --format '{{.Server.Version}}')
if echo "$OUT" | grep -qE '^[0-9]+\.[0-9]+'; then
    pass "docker version --format: '$OUT'"
else
    fail "docker version --format" "$OUT" "bare version like 20.10.0"
fi

# docker context ls
OUT=$(shim context ls --format '{{json .}}')
if echo "$OUT" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); assert d.get('Name')=='default'" 2>/dev/null; then
    pass "docker context ls: Name=default"
else
    DUMP_ON_FAIL=1 dump "context ls" "$OUT"; DUMP_ON_FAIL=0
    fail "docker context ls" "$OUT" "JSON {Name: default}"
fi

# docker context show
OUT=$(shim context show)
if [ "$OUT" = "default" ]; then
    pass "docker context show: default"
else
    fail "docker context show" "$OUT" "default"
fi

# docker buildx → must exit non-zero (signals no BuildKit; devcontainer falls back to plain build)
shim buildx version >/dev/null 2>&1; RC=$?
if [ "$RC" -ne 0 ]; then
    pass "docker buildx: exits $RC (non-zero — correct, forces plain build fallback)"
else
    fail "docker buildx: expected non-zero exit" "$RC" "non-zero"
fi

# docker info → JSON with ServerVersion and OSType=linux
OUT=$(shim info)
if echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('OSType')=='linux' and d.get('ServerVersion')" 2>/dev/null; then
    pass "docker info: OSType=linux, ServerVersion present"
else
    DUMP_ON_FAIL=1 dump "info" "$OUT"; DUMP_ON_FAIL=0
    fail "docker info" "$OUT" "JSON {OSType:linux, ServerVersion:...}"
fi

# ---------------------------------------------------------------------------
# Phase 2 — volume CRUD (R-SH-08)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 2: volume CRUD ==="

# create
OUT=$(shim volume create vsc-shimtest)
if echo "$OUT" | grep -q "vsc-shimtest"; then
    pass "volume create: '$OUT'"
else
    fail "volume create" "$OUT" "vsc-shimtest"
fi

# ls
OUT=$(shim volume ls)
if echo "$OUT" | grep -q "vsc-shimtest"; then
    pass "volume ls: contains vsc-shimtest"
else
    fail "volume ls" "$OUT" "output containing vsc-shimtest"
fi

# ls -q
OUT=$(shim volume ls -q)
if echo "$OUT" | grep -q "vsc-shimtest"; then
    pass "volume ls -q: contains vsc-shimtest"
else
    fail "volume ls -q" "$OUT" "output containing vsc-shimtest"
fi

# ---------------------------------------------------------------------------
# Phase 3 — ps label filter when no containers exist (R-SH-03 baseline)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 3: label filter baseline (no containers) ==="

OUT=$(shim ps -q -a \
    --filter "label=devcontainer.local_folder=$WORKSPACE_FOLDER" \
    --filter "label=devcontainer.config_file=$DC_CONFIG")
if [ -z "$OUT" ]; then
    pass "ps --filter (no containers): correctly empty"
else
    fail "ps --filter (no containers)" "$OUT" "(empty)"
fi

# Also test that a non-matching single label returns empty (not everything).
shim run --detach --name dc-nolabel public.ecr.aws/docker/library/alpine:latest sleep 30 >/dev/null 2>&1 || true
OUT=$(shim ps -q -a --filter "label=devcontainer.local_folder=$WORKSPACE_FOLDER")
if [ -z "$OUT" ]; then
    pass "ps --filter (unlabelled container not returned)"
else
    fail "ps --filter (unlabelled container must not match)" "$OUT" "(empty)"
fi
shim stop dc-nolabel >/dev/null 2>&1; shim rm dc-nolabel >/dev/null 2>&1 || true

# ---------------------------------------------------------------------------
# Phase 4 — probe run: correct lifecycle without keepalive (R-SH-02, R-VM-02)
#
# The probe run exits normally. devcontainer CLI then calls ps -a to find the
# exited container, inspect to read state, start to restart it, exec to attach.
# No keepalive injection. No special probe detection.
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 4: probe run and post-exit discovery ==="

# Run the exact probe command devcontainer CLI sends.
RUN_OUT=$(shim run \
    --sig-proxy=false \
    -a STDOUT -a STDERR \
    --name dc-shimtest \
    --mount "source=$WORKSPACE_FOLDER,target=/workspace,type=bind" \
    --mount "type=volume,src=vsc-shimtest,dst=/root/.vscode-server" \
    -l "devcontainer.local_folder=$WORKSPACE_FOLDER" \
    -l "devcontainer.config_file=$DC_CONFIG" \
    -e DEVCONTAINER=1 \
    -e DEBIAN_FRONTEND=noninteractive \
    --entrypoint /bin/sh \
    "$DC_IMAGE" \
    -c "echo Container started" 2>&1)

if echo "$RUN_OUT" | grep -q "Container started"; then
    pass "probe run: stdout contains 'Container started'"
else
    DUMP_ON_FAIL=1 dump "run output" "$RUN_OUT"; DUMP_ON_FAIL=0
    fail "probe run: missing 'Container started'" "$RUN_OUT" "line: Container started"
fi

CNAME="dc-shimtest"

# ps -q -a immediately after run (no sleep) — R-VM-02 timing test.
FOUND=$(shim ps -q -a \
    --filter "label=devcontainer.local_folder=$WORKSPACE_FOLDER" \
    --filter "label=devcontainer.config_file=$DC_CONFIG" | head -1)
if [ "$FOUND" = "$CNAME" ]; then
    pass "ps --filter immediately after run: found '$CNAME' (no sleep needed)"
else
    DUMP_ON_FAIL=1 dump "ps -q -a --filter output" "$(shim ps -a 2>&1)"; DUMP_ON_FAIL=0
    fail "ps --filter immediately after run" "$FOUND" "$CNAME"
fi

# Container should be EXITED (not running) — it ran 'echo' and exited.
STATE=$(shim ps -a --filter "name=$CNAME" | grep "$CNAME" | awk '{print $2}')
if [ "$STATE" = "exited" ]; then
    pass "container is exited after probe run (correct — no keepalive)"
else
    fail "container state after probe run" "$STATE" "exited"
fi

# ---------------------------------------------------------------------------
# Phase 5 — inspect EXITED container (R-SH-04, R-SH-05)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 5: inspect exited container ==="

INSPECT_JSON=$(shim inspect --type container "$CNAME")
EC=$?

if [ "$EC" -ne 0 ]; then
    fail "inspect exited container: exit $EC"
    dump "inspect output" "$INSPECT_JSON"
    echo "ABORT: cannot proceed without working inspect"
    echo "FAIL ($FAIL failed, $PASS passed)" && exit 1
fi

# Parse and validate required fields.
python3 - "$CNAME" "$WORKSPACE_FOLDER" <<PYEOF
import sys, json
name, workspace = sys.argv[1], sys.argv[2]
raw = """$INSPECT_JSON"""
try:
    data = json.loads(raw)
except json.JSONDecodeError as e:
    print(f"  [FAIL] inspect: invalid JSON: {e}")
    print(f"         raw: {raw[:300]}")
    sys.exit(1)
assert isinstance(data, list) and len(data) > 0, "not a non-empty array"
c = data[0]
errors = []

# Required fields
for field in ["Id", "Name", "Created", "State", "Config", "HostConfig", "Mounts", "NetworkSettings"]:
    if field not in c:
        errors.append(f"missing top-level field: {field}")

if "State" in c:
    for f in ["Running", "Status", "StartedAt"]:
        if f not in c["State"]:
            errors.append(f"State.{f} missing")
    # Must be boolean false (exited container)
    if c["State"].get("Running") is not False:
        errors.append(f"State.Running should be false for exited container, got: {c['State'].get('Running')!r}")

if "Config" in c:
    for f in ["Image", "Labels", "User", "Env", "Cmd"]:
        if f not in c["Config"]:
            errors.append(f"Config.{f} missing")
    env = c["Config"].get("Env", None)
    if not isinstance(env, list):
        errors.append(f"Config.Env must be list, got {type(env).__name__}: {env!r}")

ports = c.get("NetworkSettings", {}).get("Ports")
if not isinstance(ports, dict):
    errors.append(f"NetworkSettings.Ports must be object, got: {ports!r}")

# Labels round-trip
labels = c.get("Config", {}).get("Labels", {})
if labels.get("devcontainer.local_folder") != workspace:
    errors.append(f"Label devcontainer.local_folder mismatch: {labels.get('devcontainer.local_folder')!r} != {workspace!r}")

if errors:
    for e in errors:
        print(f"  [FAIL] inspect field: {e}")
    sys.exit(1)

# Workspace mount present
mounts = c.get("Mounts", [])
binds  = c.get("HostConfig", {}).get("Binds", [])
ws_found = any("/workspace" in str(m) for m in mounts) or any("/workspace" in b for b in binds)
if not ws_found:
    print(f"  [FAIL] workspace mount missing: Mounts={mounts}, Binds={binds}")
    sys.exit(1)

print(f"  [OK]   Id={c['Id']}, State.Running=false, Config.Env=list({len(c['Config'].get('Env',[]))})")
print(f"  [OK]   Labels: devcontainer.local_folder present")
print(f"  [OK]   Workspace mount found")
print(f"  [OK]   NetworkSettings.Ports is object")
PYEOF
PY_RC=$?
if [ "$PY_RC" -eq 0 ]; then
    pass "inspect exited: all required fields valid"
else
    FAIL=$((FAIL + 1))
fi

# Mount Source must be a host path, not a VM-internal /mnt/... path (R-SH-05).
MOUNT_SOURCES=$(echo "$INSPECT_JSON" | python3 -c "
import sys, json
data = json.load(sys.stdin)
mounts = data[0].get('Mounts', [])
for m in mounts:
    src = m.get('Source', '')
    if src:
        print(src)
" 2>/dev/null)
if echo "$MOUNT_SOURCES" | grep -q "^/mnt/"; then
    fail "inspect mount translation: Source contains /mnt/ (VM path, not host path)" \
         "$MOUNT_SOURCES" "host path (not /mnt/...)"
elif [ -n "$MOUNT_SOURCES" ]; then
    pass "inspect mount Source: host paths (no /mnt/... leakage)"
else
    skip "inspect mount Source: no bind mounts in output (volume-only mounts)"
fi

# ---------------------------------------------------------------------------
# Phase 6 — docker start → container becomes RUNNING (R-VM-01 / pelagos start)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 6: docker start (restart exited container) ==="

START_OUT=$(shim start "$CNAME" 2>&1)
EC=$?
if [ "$EC" -eq 0 ]; then
    pass "docker start: exit 0"
else
    DUMP_ON_FAIL=1 dump "start output" "$START_OUT"; DUMP_ON_FAIL=0
    fail "docker start" "exit $EC" "exit 0"
fi

# Give pelagos a moment to record the new running state.
sleep 1

STATE=$(shim ps --filter "name=$CNAME" | grep "$CNAME" | awk '{print $2}')
if [ "$STATE" = "running" ]; then
    pass "container is running after docker start"
else
    fail "container state after docker start" "$STATE" "running"
fi

# ---------------------------------------------------------------------------
# Phase 7 — inspect RUNNING container (R-SH-04)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 7: inspect running container ==="

INSPECT_JSON=$(shim inspect --type container "$CNAME")
RUNNING=$(echo "$INSPECT_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin)[0]['State']['Running'])" 2>/dev/null)
if [ "$RUNNING" = "True" ]; then
    pass "inspect running: State.Running=true (boolean)"
else
    DUMP_ON_FAIL=1 dump "inspect" "$INSPECT_JSON"; DUMP_ON_FAIL=0
    fail "inspect running: State.Running" "$RUNNING" "True"
fi

# Labels must survive restart.
LABEL_FOLDER=$(echo "$INSPECT_JSON" | python3 -c "
import sys, json
print(json.load(sys.stdin)[0]['Config']['Labels'].get('devcontainer.local_folder', ''))
" 2>/dev/null)
if [ "$LABEL_FOLDER" = "$WORKSPACE_FOLDER" ]; then
    pass "labels survive docker start: devcontainer.local_folder intact"
else
    fail "labels after docker start" "$LABEL_FOLDER" "$WORKSPACE_FOLDER"
fi

# ---------------------------------------------------------------------------
# Phase 8 — docker exec (R-SH-06)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 8: docker exec ==="

OUT=$(shim exec "$CNAME" /bin/sh -c "echo exec-ok" 2>&1)
if echo "$OUT" | grep -q "exec-ok"; then
    pass "exec: command ran in container"
else
    DUMP_ON_FAIL=1 dump "exec output" "$OUT"; DUMP_ON_FAIL=0
    fail "exec: echo exec-ok" "$OUT" "exec-ok"
fi

OUT=$(shim exec "$CNAME" /bin/sh -c "uname -s" 2>&1)
if [ "$OUT" = "Linux" ]; then
    pass "exec: uname -s = Linux (correct rootfs)"
else
    fail "exec: uname -s" "$OUT" "Linux"
fi

OUT=$(shim exec "$CNAME" /bin/sh -c "cat /etc/os-release" 2>&1)
if echo "$OUT" | grep -qi "ubuntu"; then
    pass "exec: /etc/os-release = Ubuntu (container rootfs, not Alpine/VM)"
else
    DUMP_ON_FAIL=1 dump "os-release" "$OUT"; DUMP_ON_FAIL=0
    fail "exec: container rootfs" "$OUT" "/etc/os-release containing 'ubuntu'"
fi

OUT=$(shim exec "$CNAME" /bin/sh -c "exit 42" 2>&1); EC=$?
if [ "$EC" -eq 42 ]; then
    pass "exec: exit code propagated (42)"
else
    fail "exec: exit code propagation" "$EC" "42"
fi

# ---------------------------------------------------------------------------
# Phase 9 — shell server pattern: interactive exec with sentinel tokens (R-SH-06)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 9: shell server pattern (interactive exec) ==="

S="sentinel-$$"
SHELL_OUT=$(printf \
    'echo -n %s; echo $PATH; echo -n %s\n
echo -n %s; getent passwd root; echo -n %s\n
echo -n %s; uname -m; echo -n %s\n
echo -n %s; cat /etc/os-release; echo -n %s\n
exit\n' \
    "$S" "$S" "$S" "$S" "$S" "$S" "$S" "$S" \
    | "$SHIM" exec -i -u root -e VSCODE_REMOTE_CONTAINERS_SESSION=test-session \
      "$CNAME" /bin/sh 2>&1)

[ "$DEBUG" -eq 1 ] && dump "shell-server output" "$SHELL_OUT"

if echo "$SHELL_OUT" | grep -q "/bin\|/usr"; then
    pass "shell-server: \$PATH non-empty"
else
    DUMP_ON_FAIL=1 dump "shell-server" "$SHELL_OUT"; DUMP_ON_FAIL=0
    fail "shell-server: \$PATH" "$SHELL_OUT" "path containing /bin or /usr"
fi

if echo "$SHELL_OUT" | grep -q "root:x:0:0"; then
    pass "shell-server: getent passwd root"
else
    fail "shell-server: getent passwd root" "${SHELL_OUT:0:200}" "root:x:0:0"
fi

if echo "$SHELL_OUT" | grep -qE "aarch64|x86_64|arm"; then
    pass "shell-server: uname -m = architecture"
else
    fail "shell-server: uname -m" "$SHELL_OUT" "aarch64|x86_64|arm"
fi

if echo "$SHELL_OUT" | grep -qi "ubuntu"; then
    pass "shell-server: /etc/os-release = Ubuntu"
else
    fail "shell-server: os-release" "$SHELL_OUT" "ubuntu"
fi

# env probe — /proc/self/environ may be unavailable (PID ns boundary); printenv is the fallback.
PROBE_UUID="probe-$$"
PROBE_OUT=$("$SHIM" exec -i -u root "$CNAME" \
    /bin/bash -l -i -c "echo -n $PROBE_UUID; cat /proc/self/environ; echo -n $PROBE_UUID" 2>&1)
if echo "$PROBE_OUT" | grep -q "$PROBE_UUID"; then
    pass "env probe: bash -lic ran (sentinel present)"
else
    DUMP_ON_FAIL=1 dump "env probe" "$PROBE_OUT"; DUMP_ON_FAIL=0
    fail "env probe: bash -lic" "$PROBE_OUT" "sentinel $PROBE_UUID in output"
fi

if echo "$PROBE_OUT" | tr '\0' '\n' | grep -q "="; then
    pass "env probe: /proc/self/environ returned K=V data"
else
    PRINTENV_OUT=$("$SHIM" exec -i -u root "$CNAME" /bin/sh -c 'printenv' 2>&1)
    if echo "$PRINTENV_OUT" | grep -q "="; then
        pass "env probe: printenv fallback works (PID ns boundary expected)"
    else
        fail "env probe: neither /proc/self/environ nor printenv" \
             "$PROBE_OUT" "K=V env data via /proc/self/environ or printenv"
    fi
fi

# ---------------------------------------------------------------------------
# Phase 10 — system-config patching (R-SH-06)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 10: system-config patching ==="

# /var/devcontainer marker
PATCH_OUT=$("$SHIM" exec -i -u root "$CNAME" /bin/sh -c \
    "mkdir -p /var/devcontainer && test ! -f /var/devcontainer/.envmarker && touch /var/devcontainer/.envmarker && echo patched-env" 2>&1)
if echo "$PATCH_OUT" | grep -q "patched-env"; then
    pass "system-config: /var/devcontainer marker created"
elif "$SHIM" exec "$CNAME" /bin/sh -c "test -f /var/devcontainer/.envmarker && echo exists" 2>&1 | grep -q "exists"; then
    pass "system-config: /var/devcontainer marker already exists (idempotent)"
else
    DUMP_ON_FAIL=1 dump "patch output" "$PATCH_OUT"; DUMP_ON_FAIL=0
    fail "system-config: /var/devcontainer marker" "$PATCH_OUT" "patched-env or marker-exists"
fi

# /etc/environment append
APPEND_OUT=$("$SHIM" exec -i -u root "$CNAME" /bin/sh -c \
    'printf "TEST_DC_VAR=\"test-value\"\n" >> /etc/environment && grep TEST_DC_VAR /etc/environment' 2>&1)
if echo "$APPEND_OUT" | grep -q "TEST_DC_VAR"; then
    pass "system-config: /etc/environment append"
else
    DUMP_ON_FAIL=1 dump "append output" "$APPEND_OUT"; DUMP_ON_FAIL=0
    fail "system-config: /etc/environment append" "$APPEND_OUT" "TEST_DC_VAR in /etc/environment"
fi

# /etc/profile sed (extended regex, in-place)
SED_OUT=$("$SHIM" exec -i -u root "$CNAME" /bin/sh -c \
    "sed -i -E 's/((^|\s)PATH=)([^\$]*)$/\1\${PATH:-\3}/g' /etc/profile || true && echo sed-ok" 2>&1)
if echo "$SED_OUT" | grep -q "sed-ok"; then
    pass "system-config: sed -i -E on /etc/profile"
else
    DUMP_ON_FAIL=1 dump "sed output" "$SED_OUT"; DUMP_ON_FAIL=0
    fail "system-config: sed /etc/profile" "$SED_OUT" "sed-ok"
fi

# ---------------------------------------------------------------------------
# Phase 11 — timing: ps immediately after a fresh run (R-VM-02, R-SH-03)
# Runs a new short-lived container and queries ps with NO sleep.
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 11: timing — ps immediately after run ==="

T11_LABEL="test.timing=$(date +%s)"
shim run \
    --name dc-timertest \
    --label "$T11_LABEL" \
    "$DC_IMAGE" /bin/sh -c "exit 0" >/dev/null 2>&1

# No sleep. ps must find it immediately.
FOUND=$(shim ps -q -a --filter "label=$T11_LABEL" 2>/dev/null | head -1)
if [ "$FOUND" = "dc-timertest" ]; then
    pass "timing: ps finds container immediately after run (no sleep)"
else
    fail "timing: ps immediately after run" "$FOUND" "dc-timertest"
fi
shim rm dc-timertest >/dev/null 2>&1 || true

# ---------------------------------------------------------------------------
# Phase 12 — multi-label AND filter: both labels must match (R-SH-03)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 12: multi-label AND filter ==="

# Container with both labels — must appear.
shim run --detach \
    --name dc-multilabel \
    --label "dc.test.k1=v1" \
    --label "dc.test.k2=v2" \
    "$DC_IMAGE" sleep 30 >/dev/null 2>&1

FOUND=$(shim ps -q -a --filter "label=dc.test.k1=v1" --filter "label=dc.test.k2=v2" | head -1)
if [ "$FOUND" = "dc-multilabel" ]; then
    pass "multi-label AND filter: both labels match → found"
else
    fail "multi-label AND filter (both match)" "$FOUND" "dc-multilabel"
fi

# Filter where second label doesn't match — must NOT appear.
FOUND=$(shim ps -q -a --filter "label=dc.test.k1=v1" --filter "label=dc.test.k2=WRONG" | head -1)
if [ -z "$FOUND" ]; then
    pass "multi-label AND filter: one mismatch → not found"
else
    fail "multi-label AND filter (one mismatch)" "$FOUND" "(empty)"
fi

# Label value containing path separators and dots (the devcontainer pattern).
shim stop dc-multilabel >/dev/null 2>&1; shim rm dc-multilabel >/dev/null 2>&1 || true

# ---------------------------------------------------------------------------
# Phase 13 — label with path value round-trip (R-VM-03, R-SH-03)
# devcontainer uses label values that are absolute paths like /Users/cb/Projects/foo
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 13: label path values ==="

PATH_LABEL="devcontainer.local_folder=$WORKSPACE_FOLDER"
shim run --detach \
    --name dc-pathlabel \
    --label "$PATH_LABEL" \
    "$DC_IMAGE" sleep 30 >/dev/null 2>&1

FOUND=$(shim ps -q -a --filter "label=$PATH_LABEL" | head -1)
if [ "$FOUND" = "dc-pathlabel" ]; then
    pass "label path value: filter matches '$WORKSPACE_FOLDER'"
else
    DUMP_ON_FAIL=1 dump "all containers" "$(shim ps -a 2>&1)"; DUMP_ON_FAIL=0
    fail "label path value filter" "$FOUND" "dc-pathlabel"
fi

STORED=$(shim inspect dc-pathlabel 2>/dev/null | python3 -c \
    "import sys,json; print(json.load(sys.stdin)[0]['Config']['Labels'].get('devcontainer.local_folder',''))" 2>/dev/null)
if [ "$STORED" = "$WORKSPACE_FOLDER" ]; then
    pass "label path value: round-trips through inspect verbatim"
else
    fail "label path value in inspect" "$STORED" "$WORKSPACE_FOLDER"
fi

shim stop dc-pathlabel >/dev/null 2>&1; shim rm dc-pathlabel >/dev/null 2>&1 || true

# ---------------------------------------------------------------------------
# Phase 14 — inspect field types (R-SH-04 type correctness)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 14: inspect field types ==="

TINSPECT=$(shim inspect --type container "$CNAME" 2>/dev/null)

# State.Running must be boolean true (not string "true" or "True")
RUNNING_TYPE=$(echo "$TINSPECT" | python3 -c "
import sys, json
c = json.load(sys.stdin)[0]
v = c['State']['Running']
print(type(v).__name__ + ':' + str(v))
" 2>/dev/null)
if [ "$RUNNING_TYPE" = "bool:True" ]; then
    pass "State.Running type: boolean true"
else
    fail "State.Running type" "$RUNNING_TYPE" "bool:True"
fi

# Config.Env must be list of strings
ENV_TYPE=$(echo "$TINSPECT" | python3 -c "
import sys, json
c = json.load(sys.stdin)[0]
env = c['Config']['Env']
print(type(env).__name__ + ':' + str(len(env)))
" 2>/dev/null)
if echo "$ENV_TYPE" | grep -q "^list:"; then
    pass "Config.Env type: list (${ENV_TYPE#list:} entries)"
else
    fail "Config.Env type" "$ENV_TYPE" "list:N"
fi

# NetworkSettings.Ports must be dict
PORTS_TYPE=$(echo "$TINSPECT" | python3 -c "
import sys, json
c = json.load(sys.stdin)[0]
ports = c['NetworkSettings']['Ports']
print(type(ports).__name__)
" 2>/dev/null)
if [ "$PORTS_TYPE" = "dict" ]; then
    pass "NetworkSettings.Ports type: dict"
else
    fail "NetworkSettings.Ports type" "$PORTS_TYPE" "dict"
fi

# Created and State.StartedAt must be non-empty ISO-8601 strings.
CREATED=$(echo "$TINSPECT" | python3 -c "import sys,json; print(json.load(sys.stdin)[0].get('Created',''))" 2>/dev/null)
if echo "$CREATED" | grep -qE "^[0-9]{4}-[0-9]{2}-[0-9]{2}T"; then
    pass "Created timestamp: ISO-8601 '$CREATED'"
else
    fail "Created timestamp" "$CREATED" "ISO-8601 datetime"
fi

STARTEDAT=$(echo "$TINSPECT" | python3 -c "import sys,json; print(json.load(sys.stdin)[0]['State'].get('StartedAt',''))" 2>/dev/null)
if echo "$STARTEDAT" | grep -qE "^[0-9]{4}-[0-9]{2}-[0-9]{2}T"; then
    pass "State.StartedAt timestamp: ISO-8601 '$STARTEDAT'"
else
    fail "State.StartedAt" "$STARTEDAT" "ISO-8601 datetime"
fi

# ---------------------------------------------------------------------------
# Phase 15 — stop + rm lifecycle (R-SH-09-adjacent)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 15: stop and rm ==="

OUT=$(shim stop "$CNAME" 2>&1); EC=$?
if [ "$EC" -eq 0 ]; then
    pass "docker stop: exit 0"
else
    fail "docker stop" "exit $EC" "exit 0"
fi

OUT=$(shim rm "$CNAME" 2>&1); EC=$?
if [ "$EC" -eq 0 ]; then
    pass "docker rm: exit 0"
else
    fail "docker rm" "exit $EC" "exit 0"
fi

# Container must no longer appear in ps -a.
FOUND=$(shim ps -q -a --filter "name=$CNAME" 2>/dev/null | head -1)
if [ -z "$FOUND" ]; then
    pass "post-rm: container gone from ps -a"
else
    fail "post-rm: container still in ps -a" "$FOUND" "(empty)"
fi

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

echo ""
echo "=== cleanup ==="
shim volume rm "vsc-shimtest" >/dev/null 2>&1 || true
shim rm -f dc-shimtest dc-timertest dc-multilabel dc-pathlabel >/dev/null 2>&1 || true
echo "  done"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
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
    printf "\nRe-run with --debug for full output on all tests.\n"
    exit 1
fi
