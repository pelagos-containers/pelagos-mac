# VM Debugging Guide

Common failure modes, recovery procedures, and test tooling.

---

## Scripts

| Script | Purpose |
|---|---|
| `scripts/vm-ping.sh` | Boot VM (if not running) and verify it responds — prints `pong` |
| `scripts/vm-restart.sh` | Kill stale daemon, clean up socket/pid, boot fresh |
| `scripts/vm-preload.sh` | Pull all test images into root.img once (run after any rebuild or recreation) |
| `scripts/vm-console.sh` | Attach to the VM serial console (hvc0) — shows kernel + guest logs |
| `scripts/test-devcontainer-e2e.sh` | Full devcontainer e2e suite (22 tests, suites A–E) |
| `scripts/test-vscode-exec.sh` | Reproduces the VS Code attach exec pattern (3 concurrent execs) |
| `scripts/test-concurrent-exec.sh` | Reproduces concurrent exec-into to check for VM crash/hang |

---

## Fresh environment setup

After `build-vm-image.sh` or after recreating `out/root.img`:

```bash
bash scripts/vm-restart.sh    # boot fresh VM
bash scripts/vm-preload.sh    # pull test images into root.img (one-time)
```

Test images are now cached in root.img and no remote registry is hit during test runs.

Note: `pelagos` has no `image pull` subcommand — `vm-preload.sh` uses `pelagos-docker pull`, which triggers an implicit pull via a probe container.

---

## Common failure modes

### VM daemon unresponsive (`no response from guest`)

**Symptoms:** `pelagos ping` hangs or returns `no response from guest`. `vm-ping.sh` times out.

**Cause:** Stale daemon process holding the socket but the guest is not accepting vsock connections.
This happens when the VM was killed mid-session (root.img write interrupted) or after a macOS sleep/wake cycle.

**Fix:**
```bash
bash scripts/vm-restart.sh
```

If it still hangs after restart, check `~/.local/share/pelagos/daemon.log` — if the log timestamp is stale (not updated since the restart), the VM booted but the guest is not accepting. Attach to the console to see why:

```bash
bash scripts/vm-console.sh
```

---

### Multiple stale daemon processes

**Symptoms:** `vm-restart.sh` doesn't help; `pelagos` commands still hang. `daemon.log` shows repeated boot attempts.

**Diagnosis:**
```bash
ps aux | grep "pelagos.*vm-daemon-internal"
```

**Fix:** Kill all of them, not just one:
```bash
pkill -KILL -f "pelagos.*vm-daemon-internal"
rm -f ~/.local/share/pelagos/vm.pid ~/.local/share/pelagos/vm.sock
bash scripts/vm-ping.sh
```

Multiple daemon processes compete for the socket_vmnet connection and corrupt NAT state.

---

### ext2 corruption (`deleted inode referenced`)

**Symptoms:** Console flooded with:
```
EXT2-fs (vda): error: ext2_lookup: deleted inode referenced: NNNNNN
```

**Cause:** `out/root.img` was killed mid-write (daemon killed while pelagos was writing container layers). The ext2 filesystem is inconsistent.

**Fix:** Recreate the image — the init script formats it fresh on next boot:
```bash
pkill -KILL -f "pelagos.*vm-daemon-internal"
rm -f ~/.local/share/pelagos/vm.pid ~/.local/share/pelagos/vm.sock
rm out/root.img && dd if=/dev/zero of=out/root.img bs=1m count=0 seek=8192
bash scripts/vm-restart.sh
bash scripts/vm-preload.sh    # re-pull test images into the fresh image
```

---

### socket_vmnet NAT degradation (image pull I/O errors)

**Symptoms:** Image pulls fail with `I/O error (os error 5)` or `error sending request`. VM otherwise appears healthy (ping works).

**Cause:** vmnet.framework NAT state degrades over time or after macOS sleep/wake. Restarting socket_vmnet reinitializes it.

**Fix:**
```bash
pkill -KILL -f "pelagos.*vm-daemon-internal"
rm -f ~/.local/share/pelagos/vm.pid ~/.local/share/pelagos/vm.sock
sudo brew services restart socket_vmnet
# Kill any daemon that started BEFORE the socket_vmnet restart:
pkill -KILL -f "pelagos.*vm-daemon-internal"
rm -f ~/.local/share/pelagos/vm.pid ~/.local/share/pelagos/vm.sock
bash scripts/vm-ping.sh
```

**Note:** `pfctl` has no effect on vmnet.framework NAT — do not use it.

---

### Registry rate limits (ECR / Docker Hub)

**Symptom:** Pull fails with `Rate exceeded` from `public.ecr.aws`.

**Root fix:** Run `vm-preload.sh` once after every root.img creation. After that, test scripts use the cached image and never hit the registry.

```bash
bash scripts/vm-preload.sh
```

If root.img was just recreated and you're waiting on a rate limit reset (~5 min), you can override the image source for one-off use:
```bash
IMAGE=docker.io/library/ubuntu:22.04 bash scripts/test-vscode-exec.sh
```

---

### Binary missing virtualization entitlement (VM silently killed)

**Symptoms:** `vm status` says `stopped`. `daemon.log` shows `VM running` then nothing. No errors.

**Cause:** `cargo build` replaces the binary with a linker-signed copy that lacks `com.apple.security.virtualization`. macOS silently kills the VM daemon when it calls into Virtualization.framework.

**Fix:** Always re-sign after every host build:
```bash
cargo build -p pelagos-mac --release
bash scripts/sign.sh
```

---

## Enabling guest-side logging

`pelagos-guest` logs are written to `/tmp/guest.log` inside the VM (tmpfs — lost on reboot).
`RUST_LOG=debug` is baked into the initramfs init script.

**Important:** logs go to a file, not to `/dev/hvc0` directly. Writing debug output to the
virtio console causes flow-control blocking that stalls exec-into operations under load
(e.g. when `docker events` is polling in parallel). File writes never block.

To watch logs live, use the console shell:
```bash
bash scripts/vm-console.sh
# then in the shell:
tail -f /tmp/guest.log
```

To change verbosity, edit `scripts/build-vm-image.sh` and change `RUST_LOG=debug` to
`RUST_LOG=trace` or `RUST_LOG=warn`, then rebuild:
```bash
bash scripts/build-vm-image.sh
bash scripts/vm-restart.sh
```

---

## Diagnosing exec-into failures

The VM console shows pelagos-guest log lines for every exec-into attempt. Key log points in `handle_exec_into`:

| Log message | Meaning |
|---|---|
| `get_container_pid` error | Container exited before exec arrived |
| `open_ns_fds` / `open_root_fd` error | Namespace FD open failed (EMFILE = too many open FDs) |
| `send_response(Ready)` EPIPE | Host closed vsock before guest could ack — exec timed out on host side |
| No log at all | Guest crashed or panicked — rebuild with `RUST_LOG=debug` |

To reproduce the VS Code attach exec pattern specifically:
```bash
# Terminal 1
bash scripts/vm-console.sh

# Terminal 2
bash scripts/test-vscode-exec.sh 2>&1 | tee suite-vscode.out
```
