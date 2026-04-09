# pelagos-mac User Guide

This guide covers everyday usage: running containers, managing VMs, using the
build environment, and the devcontainer workflow.

---

## Table of contents

1. [Quick start](#quick-start)
2. [Running containers](#running-containers)
3. [VM management](#vm-management)
4. [The two-VM model](#the-two-vm-model)
5. [The build VM](#the-build-vm)
6. [VS Code devcontainers](#vs-code-devcontainers)
7. [Troubleshooting](#troubleshooting)

---

## Quick start

**Install via Homebrew** (see [INSTALL.md](INSTALL.md) for full details):

```bash
brew tap pelagos-containers/tap
brew install pelagos-containers/tap/pelagos-mac
pelagos vm init
```

**Verify it works:**

```bash
pelagos ping                             # → pong
pelagos run --rm alpine echo "hello from Linux"

# Check what is running
pelagos vm ls
pelagos ps
```

Cold boot takes 1–2 s. Every subsequent command reuses the running VM.

---

## Running containers

```bash
# One-shot, interactive
pelagos run --rm -it alpine /bin/sh

# Detached (background), named
pelagos run -d --name web nginx:alpine

# With a host directory mounted
pelagos run --rm -v $HOME/myproject:/work -w /work debian:bookworm-slim bash

# Port forwarding: host 8080 → container 80
pelagos run -d -p 8080:80 --name web nginx:alpine
curl http://localhost:8080/
```

### Container lifecycle

```bash
pelagos ps                   # list running containers
pelagos logs web             # tail logs from the "web" container
pelagos exec web -- ls /     # run a command in a running container
pelagos stop web             # stop a container
pelagos rm web               # remove a stopped container
pelagos container prune      # remove all exited containers
```

### Image management

```bash
pelagos image ls             # list cached images (human table)
pelagos image ls --json      # machine-readable
pelagos image pull nginx:alpine
pelagos image rm nginx:alpine
pelagos image inspect nginx:alpine
```

---

## VM management

pelagos-mac boots a Linux VM as a persistent background daemon. The VM is
started automatically on the first command that needs it; you rarely need to
manage it manually.

### Seeing what is running

```bash
pelagos vm ls
```

Example output:

```
PROFILE     ACCESS       MEMORY   CPUS  STATUS
----------  ----------  -------  -----  --------------------
default     vsock/shell  2048 MB      2  running (pid 6495)
build       ssh         4096 MB      4  running (pid 58661)
```

### Starting and stopping

```bash
pelagos vm start                       # start default VM (no-op if running)
pelagos vm stop                        # stop default VM; next command auto-reboots

pelagos vm start --profile build       # start the build VM
pelagos vm stop  --profile build       # stop the build VM cleanly
```

`vm stop` sends SIGTERM and waits up to 15 s. It is safe to run at any time.

### Getting a shell in the VM

```bash
pelagos vm shell                       # vsock shell into Alpine (default) VM
pelagos vm ssh                         # SSH into default VM (same machine)
pelagos vm ssh --profile build         # SSH into Ubuntu build VM
pelagos vm console                     # raw hvc0 serial console; Ctrl-] to detach
```

`vm shell` is fastest (vsock, no TCP stack). `vm ssh` supports full SSH
features (X11 forwarding, port tunnels, etc.). Both auto-start the VM.

### Status & profile flag

Every `pelagos` command accepts `--profile <name>` to target a non-default VM:

```bash
pelagos --profile build vm status
pelagos --profile build vm ssh
pelagos --profile build vm stop
```

---

## The two-VM model

pelagos-mac ships two distinct VM configurations:

| | `default` (Alpine container VM) | `build` (Ubuntu build VM) |
|---|---|---|
| Purpose | Run OCI containers | Native aarch64 builds |
| OS | Alpine Linux (initramfs) | Ubuntu 22.04 LTS |
| Kernel | Alpine linux-lts | Ubuntu 6.8 HWE |
| Control plane | vsock → pelagos-guest | SSH → openssh-server |
| Shell access | `vm shell` (vsock) or `vm ssh` | `vm ssh --profile build` |
| Container commands | `pelagos run/exec/ps/logs` | Not applicable |
| Toolchain persistence | No (containers are ephemeral) | Yes (apt installs survive reboots) |
| Memory / CPUs | 2 GB / 2 | 4 GB / 4 |
| Disk | ~300 MB ext4 (OCI image cache) | Several GB (full Ubuntu install) |

**Key point:** `vm shell` only works for the default (Alpine) VM, because it
uses the vsock control plane. To get a shell in the Ubuntu build VM, use
`vm ssh --profile build`.

Both VMs can run concurrently. They are fully independent — separate state
directories, separate network stacks, separate lifecycle. The build VM uses
more memory; stop it when not in use:

```bash
pelagos vm stop --profile build
```

### When each VM auto-starts

- Default VM: any `pelagos run/exec/ps/logs/ping/vm shell/vm console/vm ssh` command.
- Build VM: `pelagos --profile build vm start` or `pelagos --profile build ping`
  (and `scripts/build-vm-start.sh`). It does **not** auto-start for container
  commands — there are no containers in the build VM.

---

## The build VM

The `build` profile runs a full Ubuntu 22.04 aarch64 VM. It is used to compile
pelagos itself and run its integration test suite natively (no cross-compilation,
real Linux namespaces and cgroups).

### Setup (one-time)

```bash
bash scripts/build-build-image.sh   # provision out/build.img (~several GB, takes a while)
bash scripts/build-vm-start.sh      # start VM and wait for SSH-ready
```

### Usage

```bash
# SSH in
pelagos vm ssh --profile build

# Run a remote command
pelagos vm ssh --profile build -- cargo build --release

# Stop when done (frees 4 GB RAM)
pelagos vm stop --profile build
```

### What is installed

The build VM is provisioned by `build-build-image.sh` with:

- `build-essential`, `pkg-config`, `libssl-dev`, `libseccomp-dev`, `libcap-dev`
- `rustup` + stable toolchain
- `sudo` (required by some pelagos integration tests)
- `overlay` module in `/etc/modules` (Ubuntu 6.8 ships it as `=m`)

The disk is persistent — `apt install` and `cargo build` artifacts survive
VM restarts.

### Running the pelagos test suite

```bash
pelagos vm ssh --profile build
# Inside the VM:
cd /path/to/pelagos
cargo test 2>&1 | tee test-results.txt
```

Expected: 297/303 pass, 0 fail, 6 ignored (ignored tests require external
services: docker registry, Go toolchain).

---

## VS Code devcontainers

Set the Docker executable in VS Code user settings (not workspace settings):

```json
{
  "dev.containers.dockerPath": "/path/to/pelagos-docker"
}
```

Replace the path with the actual location of your `pelagos-docker` binary
(`target/aarch64-apple-darwin/release/pelagos-docker` for a local build, or
the Homebrew path for an installed version).

The default (Alpine) VM handles all devcontainer operations. The build VM is
unrelated to VS Code devcontainers.

See [DEVCONTAINER_GUIDE.md](DEVCONTAINER_GUIDE.md) for the full guide.

---

## Troubleshooting

### VM says "stopped" immediately after start

`cargo build` strips the `com.apple.security.virtualization` entitlement.
macOS silently kills the daemon on first AVF call. Re-sign:

```bash
bash scripts/sign.sh
```

Always run this after every `cargo build`.

### VM won't start / "different mount configuration" error

Stop the daemon and retry:

```bash
pelagos vm stop
pelagos ping        # cold-boots with the new config
```

### Need to see the VM boot log

```bash
pelagos vm console          # live hvc0 serial (replays ring buffer from boot)
# Ctrl-] to detach
```

Or read the log file:

```bash
cat ~/.local/share/pelagos/daemon.log
cat ~/.local/share/pelagos/profiles/build/daemon.log
```

### Build VM SSH hangs

The build VM uses `cpuidle.off=1` in its kernel cmdline to prevent PSCI idle
stalls. If SSH hangs, the VM may not have finished booting. Use
`scripts/build-vm-start.sh` which polls until SSH is ready, or check the
console:

```bash
pelagos vm console --profile build
```

### Check which VMs are consuming memory

```bash
pelagos vm ls
```

Stop any VM you are not actively using:

```bash
pelagos vm stop --profile build
```

### More detailed debugging

See [VM_DEBUGGING.md](VM_DEBUGGING.md) for common failures, log locations,
and recovery procedures.
