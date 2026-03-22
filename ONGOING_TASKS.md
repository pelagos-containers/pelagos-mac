# pelagos-mac — Ongoing Tasks

*Last updated: 2026-03-22 — **v0.4.0 released** (SHA f6433d3)*

---

## Current State

**v0.4.0 — all pilot goals met.** VS Code "Reopen in Container" works end-to-end on
Apple Silicon. 27/27 devcontainer e2e tests pass (suites A–F). Build VM boots cleanly
in ~16s; full console replay works.

### What works today

| Feature | Status | Merged |
|---|---|---|
| VM boot via AVF | ✅ | Phase 0 |
| vsock round-trip (ping/pong) | ✅ | Phase 0 |
| `pelagos run` (pull + exec) | ✅ | PR #18 |
| Persistent daemon (warm reuse) | ✅ | PR #27 |
| virtiofs bind mounts (`-v`) | ✅ | PR #28 |
| `pelagos exec` (piped + PTY) | ✅ | PR #38 |
| `pelagos ps / logs / stop / rm` | ✅ | PR #37 |
| `pelagos run --detach --name` | ✅ | PR #37 |
| `pelagos vm shell` | ✅ | PR #45 |
| Busybox applet symlinks in VM | ✅ | PR #47 |
| Persistent OCI image cache (`/dev/vda` ext4) | ✅ | PR #50/#107 |
| ECR Public test image (no rate limits) | ✅ | PR #50 |
| devpts mount + PTY job control | ✅ | PR #38/#40 |
| `pelagos vm console` (hvc0 serial, ring buffer replay) | ✅ | PR #51/#131 |
| `pelagos vm ssh` (dropbear + ed25519 key) | ✅ | PR #52 |
| smoltcp NAT relay (no external networking deps) | ✅ | PR #113 |
| `devcontainer up` (VS Code devcontainer CLI) | ✅ | PR #66 |
| `docker build` | ✅ | PR #70 |
| `docker volume create/ls/rm` | ✅ | PR #70 |
| `docker network create/ls/rm` | ✅ | PR #70 |
| `docker cp` (both directions) | ✅ | PR #71 |
| Ubuntu build VM (`--profile build`) | ✅ | PR #125/#129/#131 |
| Ubuntu 6.8 HWE kernel for container VM | ✅ | PR #131 |
| hvc0 console drain — no RCU stall on boot | ✅ | PR #131 |

---

## Phase 4 — VS Code Dev Container support (Epic #67)

| Subtask | Issue | Status |
|---|---|---|
| Docker CLI shim (`pelagos-docker`) | #56 | ✅ PR #62+#63 |
| Native port forwarding | #57 | ✅ PR #59 |
| glibc/Ubuntu compat | #58 | ✅ PR #61 |
| docker exec, version, info, inspect | #64 | ✅ PR #65 |
| devcontainer up smoke test | #66 | ✅ PR #66 |
| docker build (native via pelagos) | #68 | ✅ PR #70 |
| docker cp | #69 | ✅ PR #71 |
| overlayfs / Ubuntu 6.8 kernel | #89 | ✅ PR #90/#131 |
| docker build multi-stage + features test | #92 | ✅ PR #94+#100 |
| VS Code full extension integration test | #91 | ✅ verified 2026-03-19 |

---

## Epic #119 — pelagos builder VM ✅ COMPLETE (PR #125/#129/#131)

Ubuntu 22.04 aarch64 VM running as `--profile build`. Boots in ~16s, SSH-ready.
Full Rust build environment, can build and test pelagos natively.

**How it works:**
- `build-build-image.sh` provisions `out/build.img`, extracts Ubuntu 6.8.0-106-generic
  kernel + initrd, writes `~/.local/share/pelagos/profiles/build/vm.conf`
- Both build VM and container VM run Ubuntu 6.8 HWE kernel (`CONFIG_KVM_GUEST=y`)
- `ping_mode = ssh` in build profile vm.conf; default profile uses vsock ping

**RCU stall fix (issue #133):** hvc0 console socketpair buffer filled when no client
connected → guest `hvc_write()` blocked in printk path → CPUs couldn't pass RCU
quiescent states → stall. Fix: `console_relay_loop` drains into a 256 KB ring buffer.

**Console ring buffer (issue #134):** ring buffer also enables full boot log replay
to any client connecting at any time. `pelagos vm console [--profile build]` works.

---

## Remaining Work

### Next priorities

- **Port forwarding** — container port → VM port → macOS `localhost`. Currently
  workaround: direct VM IP is routable from macOS host via smoltcp NAT.
- **`docker volume inspect`** — `create/ls/rm` works; `inspect` not implemented.
- **Dynamic virtiofs shares** (#74) — current per-path shares require knowing all
  paths at VM start time.
- **Signed installer** — `.pkg` for distribution. Requires Developer ID + notarization
  + `com.apple.security.virtualization`. Not yet scoped.
- **Release CI workflow** — no binary artifacts on GitHub releases today; a
  `release.yml` that builds + attaches the signed binary on tag push would be useful
  when distributing to others.

---

## Key Architecture Notes

- **Networking:** pure smoltcp userspace NAT relay via `VZFileHandleNetworkDeviceAttachment`
  (SOCK_DGRAM socketpair). No socket_vmnet, no privileged helpers. VM IP: `192.168.105.2`.
- **hvc0 console:** AVF exposes the serial port as a Unix socket. `console_relay_loop`
  polls the relay fd continuously and drains into a 256 KB ring buffer. On client connect,
  ring is replayed then live I/O proxied. Critical: if relay fd is not drained, the
  socketpair buffer fills and guest `hvc_write()` blocks → RCU stall.
- **exec-into PID namespace:** `setns(CLONE_NEWPID)` in `pre_exec` only sets
  `pid_for_children`; a second fork is required. See `docs/GUEST_CONTAINER_EXEC.md`.
- **`pelagos build` uses `--network pasta`** inside the VM. `pasta` is staged into
  the initramfs. Bridge/veth kernel modules not required.
- **`pelagos network create` requires `--subnet <CIDR>`** explicitly; the shim
  auto-generates `10.88.<hash>.0/24` from the network name.
- **Network names max 12 chars** — bridge device name is `rm-<name>`, IFNAMSIZ=15.

---

## Build Reference

| Step | Command |
|---|---|
| Host binary | `cargo build -p pelagos-mac --release` |
| Re-sign (mandatory) | `bash scripts/sign.sh` |
| Guest (cross) | `cargo build -p pelagos-guest --target aarch64-unknown-linux-gnu --release` |
| VM image | `bash scripts/build-vm-image.sh` |
| Build VM image | `bash scripts/build-build-image.sh` |
| All tests | `bash scripts/test-e2e.sh` |
| Cold-start test | `bash scripts/test-e2e.sh --cold` |
| devcontainer e2e | `bash scripts/test-devcontainer-e2e.sh` |
