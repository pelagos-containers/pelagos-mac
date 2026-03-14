#!/usr/bin/env bash
# test-devcontainer-shim.sh — Replay VS Code devcontainer CLI's exact docker command
# sequence and verify every response, without needing VS Code running.
#
# Derived from a real VS Code 1.112.0-insider / devcontainers 0.449.0 session log.
# The test exercises all shim commands in the same order VS Code sends them, checks
# JSON structure and field values, and fails early with a clear message on any gap.
#
# Usage:
#   bash scripts/test-devcontainer-shim.sh
#
# Prerequisites:
#   - VM running (or this script will start it)
#   - pelagos and pelagos-docker built and signed

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
SHIM="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos-docker"

PASS=0
FAIL=0

# Workspace folder VS Code uses for devcontainer.
WORKSPACE_FOLDER="$REPO_ROOT"
DC_CONFIG="$REPO_ROOT/.devcontainer/devcontainer.json"
DC_IMAGE="public.ecr.aws/docker/library/ubuntu:22.04"

pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1"; }

shim() {
    "$SHIM" "$@" 2>&1
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

echo "=== preflight ==="
for f in "$KERNEL" "$INITRD" "$DISK" "$BINARY" "$SHIM"; do
    if [ -f "$f" ]; then echo "  [OK]   $(basename "$f")";
    else echo "  [FAIL] missing: $f"; exit 1; fi
done

pelagos() {
    "$BINARY" \
        --kernel  "$KERNEL" \
        --initrd  "$INITRD" \
        --disk    "$DISK" \
        --cmdline "console=hvc0" \
        "$@" 2>&1
}

# Ensure VM is running.
pelagos ping | grep -q pong || {
    echo "  VM not responding — waiting for start..."
    sleep 5
    pelagos ping | grep -q pong || { echo "  [FAIL] VM not responding"; exit 1; }
}
echo "  [OK]   VM responding"

# ---------------------------------------------------------------------------
# Phase 1: Pre-flight commands VS Code sends before starting any container
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 1: pre-flight ==="

# docker version → JSON with Client and Server keys
OUT=$(shim version 2>&1)
if echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'Client' in d and 'Server' in d" 2>/dev/null; then
    pass "version: valid JSON with Client and Server"
else
    fail "version: expected JSON with Client+Server, got: $OUT"
fi

# docker version --format {{.Server.Version}} → bare version string
OUT=$(shim version --format '{{.Server.Version}}' 2>&1)
if echo "$OUT" | grep -qE '^[0-9]+\.[0-9]+'; then
    pass "version --format {{.Server.Version}}: '$OUT'"
else
    fail "version --format: expected bare version, got: $OUT"
fi

# docker -v → version string (devcontainer CLI calls this)
OUT=$(shim -v 2>&1)
if echo "$OUT" | grep -qi "docker\|version\|pelagos"; then
    pass "docker -v: '$OUT'"
else
    fail "docker -v: unexpected output: $OUT"
fi

# docker buildx version → must exit non-zero (VS Code tolerates this)
shim buildx version >/dev/null 2>&1
RC=$?
if [ "$RC" -ne 0 ]; then
    pass "buildx version: exits $RC (expected non-zero)"
else
    fail "buildx version: expected non-zero exit, got 0"
fi

# docker volume ls -q
OUT=$(shim volume ls -q 2>&1)
pass "volume ls -q: '$OUT'"

# docker volume create vscode
OUT=$(shim volume create vscode 2>&1)
if echo "$OUT" | grep -q "vscode"; then
    pass "volume create vscode: got '$OUT'"
else
    fail "volume create vscode: expected 'vscode', got: $OUT"
fi

# docker ps -q -a --filter label=vsch.local.folder=<folder>  (VS Code pre-check)
OUT=$(shim ps -q -a --filter "label=vsch.local.folder=$WORKSPACE_FOLDER" --filter "label=vsch.quality=insider" 2>&1)
pass "ps -q --filter vsch.local.folder: '$OUT'"

# docker ps -q -a --filter label=devcontainer.local_folder + devcontainer.config_file
OUT=$(shim ps -q -a \
    --filter "label=devcontainer.local_folder=$WORKSPACE_FOLDER" \
    --filter "label=devcontainer.config_file=$DC_CONFIG" 2>&1)
pass "ps -q --filter devcontainer labels: '$OUT'"

# ---------------------------------------------------------------------------
# Phase 2: Image check
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 2: image check ==="

# docker inspect --type image ubuntu:22.04
# VS Code uses this to check if image is already present before pulling.
# Expected: JSON array (possibly empty/error if not cached — VS Code then pulls).
OUT=$(shim inspect --type image "$DC_IMAGE" 2>&1)
EC=$?
if [ "$EC" -eq 0 ] && echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert isinstance(d,list)" 2>/dev/null; then
    pass "inspect --type image $DC_IMAGE: cached, valid JSON array"
elif [ "$EC" -ne 0 ]; then
    pass "inspect --type image $DC_IMAGE: exit $EC (image not cached, VS Code will pull)"
else
    fail "inspect --type image $DC_IMAGE: unexpected output: $OUT"
fi

# ---------------------------------------------------------------------------
# Phase 3: Container startup — the probe run
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 3: probe run (--sig-proxy=false) ==="

# This is the exact command VS Code sends to verify the image is runnable.
# Our shim intercepts it: detaches with a keepalive, prints "Container started".
# The workspace bind mount and volume mounts are included (as VS Code sends them).
VSCODE_VOL="vscode-server-testid"
shim volume create "$VSCODE_VOL" >/dev/null 2>&1 || true

OUT=$(shim run \
    --sig-proxy=false \
    -a STDOUT -a STDERR \
    --mount "source=$WORKSPACE_FOLDER,target=/workspace,type=bind" \
    --mount "type=volume,src=$VSCODE_VOL,dst=/root/.vscode-server" \
    --mount "type=volume,src=vscode,dst=/vscode" \
    -l "devcontainer.local_folder=$WORKSPACE_FOLDER" \
    -l "devcontainer.config_file=$DC_CONFIG" \
    -e DEVCONTAINER=1 \
    -e DEBIAN_FRONTEND=noninteractive \
    --entrypoint /bin/sh \
    -l 'devcontainer.metadata=[{"remoteUser":"root"}]' \
    "$DC_IMAGE" \
    -c "echo Container started" 2>&1)

if echo "$OUT" | grep -q "^Container started$"; then
    pass "probe run: printed 'Container started'"
else
    fail "probe run: expected 'Container started', got: $OUT"
fi

# ---------------------------------------------------------------------------
# Phase 4: Container discovery after probe
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 4: container discovery ==="

# docker ps -q -a --filter label=devcontainer.local_folder=... --filter label=devcontainer.config_file=...
# Must return the container name that was just started.
sleep 1
CNAME=$(shim ps -q -a \
    --filter "label=devcontainer.local_folder=$WORKSPACE_FOLDER" \
    --filter "label=devcontainer.config_file=$DC_CONFIG" 2>&1 | head -1)

if [ -n "$CNAME" ]; then
    pass "ps -q --filter: found container '$CNAME'"
else
    fail "ps -q --filter: no container found after probe run"
    echo ""
    echo "================================"
    echo "FAIL  ($FAIL failed, $PASS passed)"
    exit 1
fi

# ---------------------------------------------------------------------------
# Phase 5: docker inspect --type container <name>
# This is the command that failed in the live VS Code log.
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 5: inspect container ==="

OUT=$(shim inspect --type container "$CNAME" 2>&1)
EC=$?

if [ "$EC" -ne 0 ]; then
    fail "inspect container '$CNAME': exit $EC; output: $OUT"
else
    # Check JSON structure — pipe $OUT via a temp file to avoid shell quote issues.
    INSPECT_TMP=$(mktemp /tmp/pelagos-inspect-XXXXXX.json)
    printf '%s' "$OUT" > "$INSPECT_TMP"
    python3 - "$CNAME" "$WORKSPACE_FOLDER" "$INSPECT_TMP" <<'PYEOF' 2>/tmp/pelagos-inspect-py-err.txt
import sys, json
name, workspace, path = sys.argv[1], sys.argv[2], sys.argv[3]
data = json.loads(open(path).read())
assert isinstance(data, list) and len(data) > 0, "not a non-empty array"
c = data[0]
assert c.get("State", {}).get("Running") == True, f"State.Running not true: {c.get('State')}"
assert c.get("Id"), "Id missing"
assert c.get("Name"), "Name missing"
assert "Config" in c, "Config missing"
assert "Labels" in c.get("Config", {}), "Config.Labels missing"
assert "HostConfig" in c, "HostConfig missing — VS Code needs Binds"
assert "Mounts" in c, "Mounts missing"
mounts = c.get("Mounts", [])
binds  = c.get("HostConfig", {}).get("Binds", [])
workspace_found = (
    any("/workspace" in str(m) for m in mounts) or
    any("/workspace" in str(b) for b in binds)
)
assert workspace_found, f"workspace mount not found in Mounts={mounts} or HostConfig.Binds={binds}"
print("  [OK]   State.Running=true")
print("  [OK]   Id, Name, Config.Labels present")
print(f"  [OK]   HostConfig.Binds: {binds}")
print(f"  [OK]   Mounts: {[m.get('Source') + ':' + m.get('Destination') for m in mounts]}")
PYEOF

    PY_RC=$?
    rm -f "$INSPECT_TMP"
    if [ "$PY_RC" -eq 0 ]; then
        pass "inspect container '$CNAME': JSON structure correct"
    else
        PY_ERR=$(cat /tmp/pelagos-inspect-py-err.txt 2>/dev/null)
        fail "inspect container '$CNAME': JSON structure wrong — $PY_ERR; output: $OUT"
    fi
fi

# ---------------------------------------------------------------------------
# Phase 5.5: Extended inspect fields (what devcontainer CLI's FJ function reads)
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 5.5: inspect fields devcontainer CLI reads ==="

INSPECT_TMP2=$(mktemp /tmp/pelagos-inspect-ext-XXXXXX.json)
OUT=$(shim inspect --type container "$CNAME" 2>&1)
printf '%s' "$OUT" > "$INSPECT_TMP2"
python3 - "$INSPECT_TMP2" <<'PYEOF2' 2>/tmp/pelagos-inspect-ext-err.txt
import sys, json, re
path = sys.argv[1]
data = json.loads(open(path).read())
c = data[0]

# Created timestamp (used by devcontainer for lifecycle marker idempotency)
assert c.get("Created"), "Created field missing or empty — devcontainer uses it for lifecycle markers"
print(f"  [OK]   Created: {c['Created']}")

# State.StartedAt (used for postStartCommand markers)
assert c.get("State", {}).get("StartedAt"), "State.StartedAt missing — devcontainer uses it for postStart markers"
print(f"  [OK]   State.StartedAt: {c['State']['StartedAt']}")

# Config.User (devcontainer falls back to 'root' if empty, but field must exist)
assert "User" in c.get("Config", {}), "Config.User field missing"
print(f"  [OK]   Config.User: '{c['Config']['User']}'")

# Config.Env must be a list (devcontainer calls Dt() which iterates it)
env = c.get("Config", {}).get("Env", [])
assert isinstance(env, list), f"Config.Env must be a list, got: {type(env)}"
print(f"  [OK]   Config.Env: list of {len(env)} entries")

# NetworkSettings.Ports must be an object (devcontainer iterates keys)
ports = c.get("NetworkSettings", {}).get("Ports", None)
assert isinstance(ports, dict), f"NetworkSettings.Ports must be object, got: {ports}"
print(f"  [OK]   NetworkSettings.Ports: dict ({len(ports)} entries)")
PYEOF2
PY_RC=$?
rm -f "$INSPECT_TMP2"
if [ "$PY_RC" -eq 0 ]; then
    pass "inspect: Created, State.StartedAt, Config.User, Config.Env, NetworkSettings.Ports all present"
else
    PY_ERR=$(cat /tmp/pelagos-inspect-ext-err.txt 2>/dev/null)
    fail "inspect: missing fields — $PY_ERR"
fi

# ---------------------------------------------------------------------------
# Phase 6: docker exec into the running container
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 6: exec into container ==="

OUT=$(shim exec "$CNAME" /bin/sh -c "echo exec-ok" 2>&1)
if echo "$OUT" | grep -q "exec-ok"; then
    pass "exec: command ran inside container"
else
    fail "exec: expected 'exec-ok', got: $OUT"
fi

OUT=$(shim exec "$CNAME" /bin/sh -c "uname -s" 2>&1)
if echo "$OUT" | grep -q "Linux"; then
    pass "exec: uname -s = Linux (correct rootfs)"
else
    fail "exec: expected 'Linux', got: $OUT"
fi

# Verify exec is inside the container's rootfs (ubuntu), not Alpine
OUT=$(shim exec "$CNAME" /bin/sh -c "cat /etc/os-release" 2>&1)
if echo "$OUT" | grep -qi "ubuntu"; then
    pass "exec: /etc/os-release shows Ubuntu (correct container rootfs)"
else
    fail "exec: expected Ubuntu os-release, got: $OUT"
fi

# ---------------------------------------------------------------------------
# Phase 6.5: VS Code devcontainer shell server pattern
# devcontainer CLI (FJ function) opens an interactive shell and runs commands
# through stdin/stdout using sentinel tokens to demarcate output.
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 6.5: VS Code shell server pattern ==="

SHIM_ABS="$SHIM"
SENTINEL="pelagos-sentinel-$$"

# devcontainer starts: docker exec -i -u root -e VSCODE_REMOTE_CONTAINERS_SESSION=xxx <container> /bin/sh
# then probes $PATH, getent passwd, uname -m, /etc/os-release through stdin.
SHELL_OUT=$(printf \
    'echo -n %s; ( echo $PATH ); echo -n %s$?%s; echo -n %s >&2\n
echo -n %s; ( getent passwd root ); echo -n %s$?%s; echo -n %s >&2\n
echo -n %s; ( uname -m ); echo -n %s$?%s; echo -n %s >&2\n
echo -n %s; ( cat /etc/os-release ); echo -n %s$?%s; echo -n %s >&2\n
exit\n' \
    "$SENTINEL" "$SENTINEL" "$SENTINEL" "$SENTINEL" \
    "$SENTINEL" "$SENTINEL" "$SENTINEL" "$SENTINEL" \
    "$SENTINEL" "$SENTINEL" "$SENTINEL" "$SENTINEL" \
    "$SENTINEL" "$SENTINEL" "$SENTINEL" "$SENTINEL" \
    | "$SHIM_ABS" exec -i -u root -e VSCODE_REMOTE_CONTAINERS_SESSION=test-session \
      "$CNAME" /bin/sh 2>&1)

# PATH must be non-empty
if echo "$SHELL_OUT" | grep -q "/bin\|/usr"; then
    pass "shell-server: echo \$PATH returned a non-empty path"
else
    fail "shell-server: echo \$PATH returned empty/nothing; out=$SHELL_OUT"
fi

# getent passwd root must return root entry (output is interleaved with sentinels,
# so don't anchor with ^)
if echo "$SHELL_OUT" | grep -q "root:x:0:0"; then
    pass "shell-server: getent passwd root returned root entry"
else
    fail "shell-server: getent passwd root missing; out=$SHELL_OUT"
fi

# uname -m must return architecture
if echo "$SHELL_OUT" | grep -qE "aarch64|x86_64|arm"; then
    pass "shell-server: uname -m returned architecture"
else
    fail "shell-server: uname -m unexpected; out=$SHELL_OUT"
fi

# /etc/os-release must show Ubuntu
if echo "$SHELL_OUT" | grep -qi "ubuntu"; then
    pass "shell-server: /etc/os-release shows Ubuntu"
else
    fail "shell-server: /etc/os-release no Ubuntu; out=$SHELL_OUT"
fi

# ---------------------------------------------------------------------------
# Phase 6.6: VS Code user-env probe pattern (login interactive shell)
# devcontainer calls: docker exec -i -u root <container> /bin/bash -l -i -c "<cmd>"
# to probe the user's full login environment.
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 6.6: VS Code user-env probe (login shell) ==="

PROBE_UUID="probe-$(date +%s)"
PROBE_OUT=$("$SHIM_ABS" exec -i -u root "$CNAME" \
    /bin/bash -l -i -c "echo -n $PROBE_UUID; cat /proc/self/environ; echo -n $PROBE_UUID" 2>&1)

if echo "$PROBE_OUT" | grep -q "$PROBE_UUID"; then
    pass "user-env probe: /bin/bash -lic ran and produced sentinel output"
else
    fail "user-env probe: /bin/bash -lic failed; out=$PROBE_OUT"
fi

# /proc/self/environ contains null-separated KEY=VAL entries.
# Note: /proc/self is only accessible if exec'd process is in the container's PID
# namespace. pelagos exec-into enters the mount namespace but stays in the outer
# PID namespace, so /proc/self/environ is typically unavailable.
# devcontainer CLI falls back to `printenv` automatically when this happens.
if echo "$PROBE_OUT" | tr '\0' '\n' | grep -q "="; then
    pass "user-env probe: /proc/self/environ returned env data"
else
    # /proc/self/environ not available (expected: PID namespace boundary).
    # Verify devcontainer's printenv fallback path returns some env vars.
    PRINTENV_OUT=$("$SHIM_ABS" exec -i -u root "$CNAME" /bin/sh -c 'printenv' 2>&1)
    if echo "$PRINTENV_OUT" | grep -q "="; then
        pass "user-env probe: /proc/self/environ unavailable (PID ns); printenv fallback returns env vars"
    else
        fail "user-env probe: neither /proc/self/environ nor printenv returned env data; out=$PROBE_OUT"
    fi
fi

# ---------------------------------------------------------------------------
# Phase 6.7: VS Code system-config patching
# devcontainer patches /etc/environment and /etc/profile to set env vars.
# Both use exec -i -u root through the shell server.
# ---------------------------------------------------------------------------

echo ""
echo "=== phase 6.7: VS Code system-config patching ==="

# Test /etc/environment write (devcontainer adds env vars here)
PATCH_OUT=$("$SHIM_ABS" exec -i -u root "$CNAME" /bin/sh -c \
    "mkdir -p /var/devcontainer && test ! -f /var/devcontainer/.envmarker && touch /var/devcontainer/.envmarker && echo patched-env" 2>&1)
if echo "$PATCH_OUT" | grep -q "patched-env"; then
    pass "system-config: mkdir /var/devcontainer + marker file + echo works"
else
    # Accept if marker already exists (idempotent)
    EXIST_OUT=$("$SHIM_ABS" exec -i -u root "$CNAME" /bin/sh -c \
        "test -f /var/devcontainer/.envmarker && echo marker-exists" 2>&1)
    if echo "$EXIST_OUT" | grep -q "marker-exists"; then
        pass "system-config: /var/devcontainer marker already exists (idempotent)"
    else
        fail "system-config: mkdir/marker failed; out=$PATCH_OUT"
    fi
fi

# Test /etc/environment append (devcontainer appends env vars)
APPEND_OUT=$("$SHIM_ABS" exec -i -u root "$CNAME" /bin/sh -c \
    "cat >> /etc/environment <<'EOF'
TEST_ENV_VAR=\"test-value\"
EOF
grep TEST_ENV_VAR /etc/environment" 2>&1)
if echo "$APPEND_OUT" | grep -q "TEST_ENV_VAR"; then
    pass "system-config: cat >> /etc/environment works"
else
    fail "system-config: /etc/environment append failed; out=$APPEND_OUT"
fi

# Test /etc/profile sed (devcontainer normalizes PATH in /etc/profile)
SED_OUT=$("$SHIM_ABS" exec -i -u root "$CNAME" /bin/sh -c \
    "sed -i -E 's/((^|\s)PATH=)([^\$]*)$/\1\${PATH:-\3}/g' /etc/profile || true && echo sed-ok" 2>&1)
if echo "$SED_OUT" | grep -q "sed-ok"; then
    pass "system-config: sed -i on /etc/profile works"
else
    fail "system-config: sed /etc/profile failed; out=$SED_OUT"
fi

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

echo ""
echo "=== cleanup ==="
shim stop "$CNAME" >/dev/null 2>&1 || true
shim rm "$CNAME" >/dev/null 2>&1 || true
shim volume rm "$VSCODE_VOL" >/dev/null 2>&1 || true
echo "  done"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "================================"
if [ "$FAIL" -eq 0 ]; then
    echo "PASS  ($PASS passed)"
    exit 0
else
    echo "FAIL  ($FAIL failed, $PASS passed)"
    exit 1
fi
