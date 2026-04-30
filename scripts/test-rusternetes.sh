#!/usr/bin/env bash
# End-to-end test script for the Rusternetes-on-Pelagos stack.
# Mirrors every step in docs/RUSTERNETES_ON_PELAGOS.md Manual Testing section.
#
# One-shot: starts the VM if needed, ensures swap is active, builds all
# rusternetes binaries, configures kubectl, starts the stack fresh, and runs
# every test.
#
# Only prerequisite: pelagos-mac installed and pelagos binaries already built
# inside the VM at /mnt/Projects/pelagos/target/debug/.
#
# Usage:
#   bash scripts/test-rusternetes.sh

set -uo pipefail

# ----------------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------------

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

PASS=0
FAIL=0
SECTION_FAILED=0

pass()  { echo -e "  ${GREEN}PASS${NC}  $1"; PASS=$((PASS + 1)); }
fail()  { echo -e "  ${RED}FAIL${NC}  $1"; FAIL=$((FAIL + 1)); SECTION_FAILED=1; }
step()  { SECTION_FAILED=0; echo -e "\n${BOLD}${YELLOW}=== $1 ===${NC}"; }
info()  { echo "  $1"; }
fatal() { echo -e "\n${RED}FATAL${NC}: $1"; exit 1; }

# Run a command inside the VM, capturing stdout+stderr.
vm() { pelagos --profile build vm ssh -- "$@" 2>&1; }

# Run a command inside the VM with live output (for long builds).
vm_live() { pelagos --profile build vm ssh -- "$@"; }

# Run a command inside the VM and return the first line of stdout only.
vm_out() { pelagos --profile build vm ssh -- "$@" 2>/dev/null | tr -d '\r' | head -1; }

# ----------------------------------------------------------------------------
# Paths inside the VM (all absolute)
# ----------------------------------------------------------------------------

PELAGOS_SRC=/mnt/Projects/pelagos
RUSTERNETES_SRC=/mnt/Projects/rusternetes
PELAGOS=$PELAGOS_SRC/target/debug/pelagos
DOCKERD=$PELAGOS_SRC/target/debug/pelagos-dockerd
API_SERVER=$RUSTERNETES_SRC/target/debug/api-server
KUBELET=$RUSTERNETES_SRC/target/debug/kubelet
SCHEDULER=$RUSTERNETES_SRC/target/debug/scheduler
DB=/tmp/rusternetes.db

VM_IP=192.168.106.2

# ----------------------------------------------------------------------------
# 0. VM
# ----------------------------------------------------------------------------

step "0. VM"

info "Starting build VM (or verifying it is already running)..."
if ! pelagos --profile build ping > /dev/null 2>&1; then
    fatal "build VM failed to start"
fi
pass "VM is running"

# ----------------------------------------------------------------------------
# 1. Preflight checks
# ----------------------------------------------------------------------------

step "1. Preflight"

# Verify pelagos source and binaries
for bin in "$PELAGOS" "$DOCKERD"; do
    if vm "test -f $bin" > /dev/null 2>&1; then
        pass "found $(basename $bin)"
    else
        fatal "$bin not found — build pelagos inside the VM first:
    pelagos --profile build vm ssh
    cd $PELAGOS_SRC && cargo build"
    fi
done

# Verify rusternetes source
if vm "test -d $RUSTERNETES_SRC" > /dev/null 2>&1; then
    pass "rusternetes source present"
else
    fatal "$RUSTERNETES_SRC not found — mount or clone rusternetes inside the VM"
fi

# Ensure swap is active (rustc OOMs on 4 GB RAM without it)
SWAP_MB=$(vm_out 'free -m | awk "/^Swap:/ {print \$2}"')
if [ "${SWAP_MB:-0}" -ge 1000 ]; then
    pass "swap active (${SWAP_MB} MB)"
else
    info "Swap not active — enabling 4 GB swapfile..."
    vm 'test -f /swapfile || fallocate -l 4G /swapfile; chmod 600 /swapfile; mkswap -f /swapfile > /dev/null 2>&1; swapon /swapfile 2>/dev/null || true' > /dev/null
    SWAP_MB=$(vm_out 'free -m | awk "/^Swap:/ {print \$2}"')
    if [ "${SWAP_MB:-0}" -ge 1000 ]; then
        pass "swap enabled (${SWAP_MB} MB)"
    else
        fail "could not enable swap — compilation may OOM"
    fi
fi

# ----------------------------------------------------------------------------
# 2. Build rusternetes
# ----------------------------------------------------------------------------

step "2. Build rusternetes"

info "Building rusternetes-api-server (--features sqlite)..."
if ! vm_live "cd $RUSTERNETES_SRC && cargo build -p rusternetes-api-server --features sqlite"; then
    fatal "cargo build rusternetes-api-server failed"
fi
pass "rusternetes-api-server built"

info "Building rusternetes-kubelet (--features sqlite)..."
if ! vm_live "cd $RUSTERNETES_SRC && cargo build -p rusternetes-kubelet --features sqlite"; then
    fatal "cargo build rusternetes-kubelet failed"
fi
pass "rusternetes-kubelet built"

info "Building rusternetes-scheduler (--features sqlite)..."
if ! vm_live "cd $RUSTERNETES_SRC && cargo build -p rusternetes-scheduler --features sqlite"; then
    fatal "cargo build rusternetes-scheduler failed"
fi
pass "rusternetes-scheduler built"

info "Building rusternetes-kubectl..."
if ! vm_live "cd $RUSTERNETES_SRC && cargo build -p rusternetes-kubectl"; then
    fatal "cargo build rusternetes-kubectl failed"
fi
pass "rusternetes-kubectl built"

# ----------------------------------------------------------------------------
# 3. kubectl context
# ----------------------------------------------------------------------------

step "3. kubectl context"

kubectl config set-cluster rusternetes \
    --server="https://${VM_IP}:6443" \
    --insecure-skip-tls-verify=true > /dev/null
kubectl config set-credentials rusternetes-admin --token=dev > /dev/null
kubectl config set-context rusternetes \
    --cluster=rusternetes --user=rusternetes-admin > /dev/null
kubectl config use-context rusternetes > /dev/null
pass "kubectl context set to rusternetes"

# ----------------------------------------------------------------------------
# 4. Stack startup (fresh)
# ----------------------------------------------------------------------------

step "4. Stack startup"

info "Stopping any existing stack processes..."
vm 'pkill -f pelagos-dockerd 2>/dev/null; pkill -f rusternetes.*api-server 2>/dev/null; pkill -f rusternetes.*kubelet 2>/dev/null; pkill -f rusternetes.*scheduler 2>/dev/null; true' > /dev/null 2>&1 || true
sleep 2
vm "rm -f $DB /var/run/pelagos-dockerd.sock" > /dev/null 2>&1 || true
info "Old state cleared"

info "Starting pelagos-dockerd..."
vm "nohup $DOCKERD --pelagos-bin $PELAGOS > /tmp/dockerd.log 2>&1 &" > /dev/null
sleep 2

if vm "test -S /var/run/pelagos-dockerd.sock" > /dev/null 2>&1; then
    pass "pelagos-dockerd running"
else
    fail "pelagos-dockerd socket not present"
    info "--- dockerd log ---"; vm 'tail -20 /tmp/dockerd.log' || true
    fatal "cannot proceed without pelagos-dockerd"
fi

info "Starting api-server..."
vm "nohup env DOCKER_HOST=unix:///var/run/pelagos-dockerd.sock $API_SERVER --storage-backend sqlite --data-dir $DB --skip-auth --tls --tls-self-signed --tls-san 'localhost,127.0.0.1,$VM_IP' > /tmp/apiserver.log 2>&1 &" > /dev/null
sleep 2

info "Starting kubelet..."
vm "nohup env DOCKER_HOST=unix:///var/run/pelagos-dockerd.sock RUST_MIN_STACK=8388608 $KUBELET --node-name pelagos-node --storage-backend sqlite --data-dir $DB --network bridge > /tmp/kubelet.log 2>&1 &" > /dev/null
sleep 2

info "Starting scheduler..."
vm "nohup $SCHEDULER --storage-backend sqlite --data-dir $DB > /tmp/scheduler.log 2>&1 &" > /dev/null
sleep 1

info "Waiting for pelagos-node to register (up to 30s)..."
READY=0
for i in $(seq 1 30); do
    if kubectl get nodes 2>/dev/null | grep -q "pelagos-node"; then
        READY=1; break
    fi
    sleep 1
done

if [ $READY -eq 1 ]; then
    pass "stack started — pelagos-node registered"
else
    fail "pelagos-node not ready after 30s"
    info "--- api-server log ---"; vm 'tail -20 /tmp/apiserver.log' || true
    info "--- kubelet log ---";    vm 'tail -20 /tmp/kubelet.log'    || true
    fatal "cannot proceed without a ready node"
fi

# ----------------------------------------------------------------------------
# 5. Smoke tests
# ----------------------------------------------------------------------------

step "5. Smoke tests"

NODES=$(kubectl get nodes 2>&1)
if echo "$NODES" | grep -q "pelagos-node"; then
    pass "kubectl get nodes: pelagos-node listed"
else
    fail "kubectl get nodes: pelagos-node not found"
    info "$NODES"
fi

if kubectl get pods -A > /dev/null 2>&1; then
    pass "kubectl get pods -A: no error"
else
    fail "kubectl get pods -A returned an error"
fi

# ----------------------------------------------------------------------------
# 6. Pod lifecycle (hello)
# ----------------------------------------------------------------------------

step "6. Pod lifecycle (hello)"

kubectl delete pod hello --wait=false > /dev/null 2>&1 || true
vm 'rm -rf /run/pelagos/containers/hello_*' > /dev/null 2>&1 || true
vm 'truncate -s 0 /tmp/scheduler.log' > /dev/null 2>&1 || true
sleep 1

kubectl apply -f - > /dev/null <<'YAML'
apiVersion: v1
kind: Pod
metadata:
  name: hello
  namespace: default
spec:
  restartPolicy: Never
  containers:
  - name: app
    image: alpine:latest
    command: ["sh", "-c", "echo hello-from-kubectl"]
YAML

info "Waiting for scheduler to bind pod (up to 20s)..."
BOUND=0
for i in $(seq 1 20); do
    if vm 'grep -q "Successfully bound" /tmp/scheduler.log 2>/dev/null' > /dev/null 2>&1; then
        BOUND=1; break
    fi
    sleep 1
done
if [ $BOUND -eq 1 ]; then
    pass "scheduler bound pod to pelagos-node"
else
    fail "scheduler did not log 'Successfully bound' within 20s"
    info "--- scheduler log ---"; vm 'cat /tmp/scheduler.log' || true
fi

if [ $SECTION_FAILED -eq 0 ]; then
    info "Waiting for container output (up to 30s)..."
    OUT=""
    for i in $(seq 1 30); do
        OUT=$(vm_out 'cat /run/pelagos/containers/hello_app/stdout.log 2>/dev/null')
        [ "$OUT" = "hello-from-kubectl" ] && break
        sleep 1
    done
    if [ "$OUT" = "hello-from-kubectl" ]; then
        pass "container output: '$OUT'"
    else
        fail "container output not found (got: '$OUT')"
        info "--- kubelet log ---"; vm 'tail -15 /tmp/kubelet.log' || true
    fi
fi

kubectl delete pod hello --wait=false > /dev/null 2>&1 || true
sleep 3
LEFTOVER=$(vm 'ls /run/pelagos/containers/ 2>/dev/null | grep "^hello" || true')
if [ -z "$LEFTOVER" ]; then
    pass "container removed after delete"
else
    fail "container still present after delete: $LEFTOVER"
fi

# ----------------------------------------------------------------------------
# 7. Multi-container pod — shared network namespace
# ----------------------------------------------------------------------------

step "7. Multi-container pod (shared netns)"

kubectl delete pod netns-pod --wait=false > /dev/null 2>&1 || true
vm 'rm -rf /run/pelagos/containers/netns-pod_*' > /dev/null 2>&1 || true
sleep 1

kubectl apply -f - > /dev/null <<'YAML'
apiVersion: v1
kind: Pod
metadata:
  name: netns-pod
  namespace: default
spec:
  restartPolicy: Never
  containers:
  - name: server
    image: alpine:latest
    command: ["sh", "-c", "nc -lp 8080 -e echo hello-from-server"]
  - name: client
    image: alpine:latest
    command: ["sh", "-c", "sleep 2 && nc localhost 8080"]
YAML

info "Waiting for client output (up to 30s)..."
OUT=""
for i in $(seq 1 30); do
    OUT=$(vm_out 'cat /run/pelagos/containers/netns-pod_client/stdout.log 2>/dev/null')
    [ "$OUT" = "hello-from-server" ] && break
    sleep 1
done
if [ "$OUT" = "hello-from-server" ]; then
    pass "shared netns: client received '$OUT'"
else
    fail "shared netns: client output not found (got: '$OUT')"
    info "--- kubelet log ---"; vm 'tail -15 /tmp/kubelet.log' || true
fi

kubectl delete pod netns-pod --wait=false > /dev/null 2>&1 || true

# ----------------------------------------------------------------------------
# 8. emptyDir volume
# ----------------------------------------------------------------------------

step "8. emptyDir volume"

kubectl delete pod emptydir-pod --wait=false > /dev/null 2>&1 || true
vm 'rm -rf /run/pelagos/containers/emptydir-pod_*' > /dev/null 2>&1 || true
sleep 1

kubectl apply -f - > /dev/null <<'YAML'
apiVersion: v1
kind: Pod
metadata:
  name: emptydir-pod
  namespace: default
spec:
  restartPolicy: Never
  volumes:
  - name: shared
    emptyDir: {}
  containers:
  - name: writer
    image: alpine:latest
    command: ["sh", "-c", "echo hello-from-writer > /shared/msg.txt && sleep 10"]
    volumeMounts:
    - name: shared
      mountPath: /shared
  - name: reader
    image: alpine:latest
    command: ["sh", "-c", "sleep 3 && cat /shared/msg.txt"]
    volumeMounts:
    - name: shared
      mountPath: /shared
YAML

info "Waiting for reader output (up to 30s)..."
OUT=""
for i in $(seq 1 30); do
    OUT=$(vm_out 'cat /run/pelagos/containers/emptydir-pod_reader/stdout.log 2>/dev/null')
    [ "$OUT" = "hello-from-writer" ] && break
    sleep 1
done
if [ "$OUT" = "hello-from-writer" ]; then
    pass "emptyDir: reader received '$OUT'"
else
    fail "emptyDir: reader output not found (got: '$OUT')"
    info "--- kubelet log ---"; vm 'tail -15 /tmp/kubelet.log' || true
fi

kubectl delete pod emptydir-pod --wait=false > /dev/null 2>&1 || true

# ----------------------------------------------------------------------------
# 9. hostPath volume
# ----------------------------------------------------------------------------

step "9. hostPath volume"

kubectl delete pod hostpath-pod --wait=false > /dev/null 2>&1 || true
vm 'rm -rf /run/pelagos/containers/hostpath-pod_* /tmp/hostpath-test' > /dev/null 2>&1 || true
sleep 1

vm 'mkdir -p /tmp/hostpath-test && printf "written-from-host\n" > /tmp/hostpath-test/file.txt' > /dev/null

kubectl apply -f - > /dev/null <<'YAML'
apiVersion: v1
kind: Pod
metadata:
  name: hostpath-pod
  namespace: default
spec:
  restartPolicy: Never
  volumes:
  - name: data
    hostPath:
      path: /tmp/hostpath-test
  containers:
  - name: app
    image: alpine:latest
    command: ["sh", "-c", "cat /data/file.txt && echo written-from-container > /data/out.txt"]
    volumeMounts:
    - name: data
      mountPath: /data
YAML

info "Waiting for container output (up to 30s)..."
OUT=""
for i in $(seq 1 30); do
    OUT=$(vm_out 'cat /run/pelagos/containers/hostpath-pod_app/stdout.log 2>/dev/null')
    [ "$OUT" = "written-from-host" ] && break
    sleep 1
done
if [ "$OUT" = "written-from-host" ]; then
    pass "hostPath: container read '$OUT' from host"
else
    fail "hostPath: container did not read host file (got: '$OUT')"
    info "--- kubelet log ---"; vm 'tail -15 /tmp/kubelet.log' || true
fi

HOST_OUT=$(vm_out 'cat /tmp/hostpath-test/out.txt 2>/dev/null')
if [ "$HOST_OUT" = "written-from-container" ]; then
    pass "hostPath: host received '$HOST_OUT' from container"
else
    fail "hostPath: host file not written by container (got: '$HOST_OUT')"
fi

kubectl delete pod hostpath-pod --wait=false > /dev/null 2>&1 || true

# ----------------------------------------------------------------------------
# Summary
# ----------------------------------------------------------------------------

echo ""
echo -e "${BOLD}================================${NC}"
if [ $FAIL -eq 0 ]; then
    echo -e "  ${GREEN}All $PASS tests passed${NC}"
else
    echo -e "  ${GREEN}Passed${NC}: $PASS"
    echo -e "  ${RED}Failed${NC}: $FAIL"
fi
echo -e "${BOLD}================================${NC}"
echo ""

[ $FAIL -eq 0 ]
