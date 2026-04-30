# Building and Running Rusternetes on Pelagos

Rusternetes is a Kubernetes implementation in Rust. This document covers how to
build and run the full rusternetes stack (api-server, scheduler, kubelet) inside
the pelagos build VM, using pelagos as the container runtime via `pelagos-dockerd`.

## Prerequisites

### Build VM

The build VM (profile `build`, IP `192.168.106.2`) must be running with the
pelagos source mounted via virtiofs at `/mnt/Projects`.

Rusternetes source is expected at `/mnt/Projects/rusternetes`.

### Swap

The build VM has 4GB RAM. Compiling large rusternetes crates (api-server
especially) will OOM without swap. A 4GB swapfile is required:

```bash
sudo fallocate -l 4G /swapfile
sudo chmod 600 /swapfile
sudo mkswap /swapfile
sudo swapon /swapfile
# Make persistent across reboots:
echo "/swapfile none swap sw 0 0" | sudo tee -a /etc/fstab
```

This only needs to be done once per VM image. Verify with `free -h`.

### pelagos-dockerd

`pelagos-dockerd` must be running before starting the kubelet:

```bash
sudo pelagos dockerd &
```

## Building

All three binaries require the `sqlite` feature flag. Without it, the
`--storage-backend sqlite` flag is silently ignored and the binary falls
through to etcd, failing with a DNS error.

```bash
cd /mnt/Projects/rusternetes

cargo build -p rusternetes-api-server --features sqlite
cargo build -p rusternetes-kubelet --features sqlite
cargo build -p rusternetes-scheduler --features sqlite
cargo build -p rusternetes-kubectl        # no sqlite feature needed
```

Incremental builds are fast after the first full build. If you hit an OOM
during compilation, check that swap is active (`free -h`).

## Running the Stack

All three components share one SQLite database file. Start them in order:

```bash
DB=/tmp/rusternetes.db

# 1. api-server
DOCKER_HOST=unix:///var/run/pelagos-dockerd.sock \
sudo -E /mnt/Projects/rusternetes/target/debug/api-server \
  --storage-backend sqlite --data-dir $DB \
  --skip-auth --tls --tls-self-signed \
  --tls-san "localhost,127.0.0.1,192.168.106.2" \
  > /tmp/apiserver.log 2>&1 &

# 2. kubelet
DOCKER_HOST=unix:///var/run/pelagos-dockerd.sock \
RUST_MIN_STACK=8388608 \
sudo -E /mnt/Projects/rusternetes/target/debug/kubelet \
  --node-name pelagos-node \
  --storage-backend sqlite --data-dir $DB \
  --network bridge \
  > /tmp/kubelet.log 2>&1 &

# 3. scheduler
/mnt/Projects/rusternetes/target/debug/scheduler \
  --storage-backend sqlite --data-dir $DB \
  > /tmp/scheduler.log 2>&1 &
```

`RUST_MIN_STACK=8388608` is required for the kubelet — without it the watch
loop overflows the default stack (rusternetes bug).

## Configuring kubectl on macOS

From the macOS host, port 6443 is reachable at `192.168.106.2` (no explicit
port-forward needed when the build VM is running):

```bash
kubectl config set-cluster rusternetes \
  --server=https://192.168.106.2:6443 \
  --insecure-skip-tls-verify=true
kubectl config set-credentials rusternetes-admin --token=dev
kubectl config set-context rusternetes \
  --cluster=rusternetes --user=rusternetes-admin
kubectl config use-context rusternetes
```

Verify with `kubectl get nodes` — should show `pelagos-node`.

## Applying Pods

With the scheduler running, `nodeName` is not required in pod specs:

```yaml
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
```

```bash
kubectl apply -f pod.yaml
kubectl get pods
```

## Known Limitations

### Volume mounts reference build VM paths

`hostPath` volumes in pod specs reference paths on the build VM (the
Kubernetes node), not on the macOS host. If you want macOS source files
available in a pod, use the virtiofs mount path (e.g. `/mnt/share0/myproject`)
rather than the macOS path (`/Users/cb/myproject`).

`emptyDir` volumes work without any special consideration.
