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
