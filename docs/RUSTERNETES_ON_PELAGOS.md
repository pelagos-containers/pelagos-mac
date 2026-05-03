# Building and Running Rusternetes on Pelagos

Rusternetes is a Kubernetes implementation in Rust. This document covers how to
build and run the full rusternetes stack (api-server, scheduler, kubelet) inside
the pelagos build VM, using pelagos as the container runtime via `pelagos-dockerd`.

## Automated testing

The script `scripts/test-rusternetes.sh` is the recommended way to verify the
full stack. It is one-shot: it starts the VM, ensures swap is active, builds all
rusternetes binaries, configures kubectl, starts the stack fresh, and runs every
test in the Manual Testing section below.

```bash
bash scripts/test-rusternetes.sh
```

The only manual prerequisite is that the pelagos binaries must already be built
inside the VM:

```bash
pelagos --profile build vm ssh
cd /mnt/Projects/pelagos && cargo build
```

Everything else — rusternetes builds, kubectl context, stack startup, and all
tests — is handled by the script.

---

## One-time console build (required for pelagos-ui console)

The rusternetes web console is a React/Vite app at `~/Projects/rusternetes/console/`.
It must be built on **macOS** — npm is not installed in the build VM.

```bash
cd ~/Projects/rusternetes/console && npm install && npm run build
```

This produces `~/Projects/rusternetes/console/dist/`. Because `~/Projects/` is
virtiofs-mounted inside the build VM at `/mnt/Projects/`, the build output is
immediately visible in the VM at `/mnt/Projects/rusternetes/console/dist/` with no
copying required.

The pelagos-guest kubernetes start handler checks for that path at startup and
passes `--console-dir /mnt/Projects/rusternetes/console/dist` to api-server when
it exists. The console is then served at `https://192.168.106.2:6443/console/`.

Re-run the build whenever the console source changes. The built `dist/` is in
`.gitignore` in the rusternetes repo and is never committed.

---

## One-time TLS setup (required for pelagos-ui console)

The rusternetes api-server runs inside the build VM at `https://192.168.106.2:6443`.
The build VM is directly routable from macOS via the utun interface that pelagos-pfctl
sets up, so the api-server is reachable at that address from the macOS host.

The pelagos-ui Kubernetes tab embeds the rusternetes web console in a
`WebviewWindow` pointing at `https://192.168.106.2:6443/console/`. WKWebView
rejects self-signed certificates by default, so a locally-trusted CA must be
set up once before the console will load.

### What the setup does

`scripts/setup-kubernetes-tls.sh` performs the following steps:

1. Generates a local CA (`ca.key` + `ca.crt`) valid for 10 years.
2. Generates a server certificate (`server.key` + `server.crt`) signed by
   that CA, with SANs covering `localhost`, `127.0.0.1`, and `192.168.106.2`.
3. Stores all four files at **`~/Projects/pelagos-mac/tls/`** on the macOS host.
4. Adds `ca.crt` to the macOS System keychain as a trusted root (requires `sudo`).

### Where the files live

| Path (macOS host) | Path (inside build VM via virtiofs) | Purpose |
|---|---|---|
| `~/Projects/pelagos-mac/tls/ca.crt` | _(not needed in VM)_ | CA cert — added to macOS keychain |
| `~/Projects/pelagos-mac/tls/ca.key` | _(not needed in VM)_ | CA private key — keep secret |
| `~/Projects/pelagos-mac/tls/server.crt` | `/mnt/Projects/pelagos-mac/tls/server.crt` | api-server TLS cert |
| `~/Projects/pelagos-mac/tls/server.key` | `/mnt/Projects/pelagos-mac/tls/server.key` | api-server TLS key |

The `tls/` directory is in `.gitignore` — these files are never committed.

The virtiofs mount makes the macOS `~/Projects/pelagos-mac/tls/` directory visible
inside the build VM at `/mnt/Projects/pelagos-mac/tls/` automatically, with no extra
configuration. The pelagos-guest kubernetes start handler checks for the cert files
at that path and uses them if present; if absent it falls back to `--tls-self-signed`
(stack starts but pelagos-ui will show a certificate error).

### Running the setup

```bash
bash scripts/setup-kubernetes-tls.sh
```

macOS will prompt for your password to add the CA to the System keychain.

After running, restart the rusternetes stack so the api-server picks up the cert:

```bash
pelagos --profile build kubernetes stop
pelagos --profile build kubernetes start
```

### Regenerating certificates

If the build VM IP changes, or the cert expires, re-run with `--force`:

```bash
bash scripts/setup-kubernetes-tls.sh --force
```

Then remove the old CA from the macOS System keychain (Keychain Access app,
search for "Pelagos Rusternetes Local CA", delete it), and restart the stack.

---

## Prerequisites (manual workflow)

### Build VM

The build VM (profile `build`, IP `192.168.106.2`) must be running with the
pelagos source mounted via virtiofs at `/mnt/Projects`. See
[VM_LIFECYCLE.md](VM_LIFECYCLE.md) for full details on starting, stopping, and
managing VMs.

The key commands for the build VM:

```bash
# On macOS — start the VM (or verify it is running)
pelagos --profile build ping

# On macOS — SSH into the build VM
pelagos --profile build vm ssh

# On macOS — run a single command inside the VM
pelagos --profile build vm ssh -- <command>
```

`ping` auto-starts the VM if it is not already running and returns `pong` when
ready. `vm ssh` auto-starts the VM if needed and drops into an interactive
shell.

Rusternetes source is expected at `/mnt/Projects/rusternetes` (inside the VM).

### pelagos-dockerd

`pelagos-dockerd` must be running inside the build VM before starting the
kubelet. SSH in interactively and run:

```bash
# Inside the VM
/mnt/Projects/pelagos/target/debug/pelagos-dockerd \
  --pelagos-bin /mnt/Projects/pelagos/target/debug/pelagos &
```

The `--pelagos-bin` flag is required — without it pelagos-dockerd defaults to
looking for `pelagos` on PATH, which is not in the default root environment.

Note: the build VM SSH session connects as root, so `sudo` is not needed.

## Building

SSH into the build VM first:

```bash
pelagos --profile build vm ssh
```

All commands in this section run inside the VM. All three binaries require
the `sqlite` feature flag. Without it, the `--storage-backend sqlite` flag
is silently ignored and the binary falls through to etcd, failing with a DNS
error.

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

All commands in this section run inside the VM (via `pelagos --profile build vm ssh`).

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

With the scheduler running, `nodeName` is not required in pod specs. On the
macOS host, save the following as `pod.yaml` in a working directory of your
choice (e.g. `~/rusternetes-test/`):

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

Then apply it:

```bash
kubectl apply -f pod.yaml
kubectl get pods
```

## Manual Testing

This section walks through verifying the full stack end-to-end from the macOS
host. Run these in order after completing the setup steps above.

All commands and YAML files are on the macOS host unless prefixed with
`(in VM)`. Create all YAML files in a working directory on macOS (e.g.
`~/rusternetes-test/`) and run `kubectl` from that directory.

### 1. Smoke tests

```bash
kubectl get nodes
```

Expected: `pelagos-node` listed with an age.

```bash
kubectl get pods -A
```

Expected: returns without error. May be empty or show pods from previous
sessions.

### 2. Pod lifecycle

Create a file `hello.yaml`:

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

Apply it and verify:

```bash
kubectl apply -f hello.yaml
```

Check the scheduler picked it up (in VM):

```bash
(in VM) grep "Successfully bound" /tmp/scheduler.log
```

Expected: `Successfully bound pod to node pelagos-node`

Wait a few seconds, then check container output (in VM):

```bash
(in VM) cat /run/pelagos/containers/hello_app/stdout.log
```

Expected: `hello-from-kubectl`

Note: `kubectl logs` hangs indefinitely — pelagos-dockerd holds the streaming
connection open even after the container exits. Use the VM-side log file
above instead.

Clean up:

```bash
kubectl delete pod hello --wait=false
```

Note: `--wait=false` is required throughout — without it `kubectl delete`
hangs waiting for a watch termination signal that rusternetes never sends.

Verify the container was removed (in VM):

```bash
(in VM) sudo ls /run/pelagos/containers/ | grep hello
```

Expected: no output.

### 3. Multi-container pod with shared network namespace

Create `netns-pod.yaml`:

```yaml
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
```

```bash
kubectl apply -f netns-pod.yaml
```

Wait ~10 seconds, then check client output (in VM):

```bash
(in VM) sudo cat /run/pelagos/containers/netns-pod_client/stdout.log
```

Expected: `hello-from-server`

```bash
kubectl delete pod netns-pod --wait=false
```

### 4. emptyDir volume (shared between containers)

Create `emptydir-pod.yaml`:

```yaml
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
```

```bash
kubectl apply -f emptydir-pod.yaml
```

Wait ~10 seconds, then check reader output (in VM):

```bash
(in VM) sudo cat /run/pelagos/containers/emptydir-pod_reader/stdout.log
```

Expected: `hello-from-writer`

```bash
kubectl delete pod emptydir-pod --wait=false
```

### 5. hostPath volume

Create a test file on the build VM first:

```bash
(in VM) mkdir -p /tmp/hostpath-test && echo written-from-host > /tmp/hostpath-test/file.txt
```

Create `hostpath-pod.yaml`:

```yaml
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
```

```bash
kubectl apply -f hostpath-pod.yaml
```

Wait ~10 seconds, then verify container read the host file (in VM):

```bash
(in VM) sudo cat /run/pelagos/containers/hostpath-pod_app/stdout.log
```

Expected: `written-from-host`

Verify container wrote back to the host (in VM):

```bash
(in VM) cat /tmp/hostpath-test/out.txt
```

Expected: `written-from-container`

```bash
kubectl delete pod hostpath-pod --wait=false
```

## Known Limitations

### Volume mounts reference build VM paths

`hostPath` volumes in pod specs reference paths on the build VM (the
Kubernetes node), not on the macOS host. If you want macOS source files
available in a pod, use the virtiofs mount path (e.g. `/mnt/share0/myproject`)
rather than the macOS path (`/Users/cb/myproject`).

`emptyDir` volumes work without any special consideration.
