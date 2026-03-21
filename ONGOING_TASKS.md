# pelagos-mac — Ongoing Tasks

*Last updated: 2026-03-21 — PR #127 merged to master (SHA 5615278)*

---

## Current State

**v0.2.0 + PR #127 on master.** VS Code "Reopen in Container" works end-to-end.
All 27 devcontainer e2e tests (Suites A–F) pass. Ubuntu build VM boots in ~61s
and is SSH-accessible.

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
| Persistent OCI image cache (`/dev/vda` ext4) | ✅ | PR #50/#107 |
| `pelagos vm console` (hvc0 serial) | ✅ | PR #51 |
| `pelagos vm ssh` (dropbear + ed25519) | ✅ | PR #52 |
| smoltcp NAT relay (replaces socket_vmnet) | ✅ | PR #117 |
| `devcontainer up` | ✅ | PR #66 |
| `docker build` | ✅ | PR #70 |
| `docker volume create/ls/rm` | ✅ | PR #70 |
| `docker network create/ls/rm` | ✅ | PR #70 |
| `docker cp` (both directions) | ✅ | PR #71 |
| Per-profile `vm.conf` | ✅ | PR #127 |
| `--extra-disk` (secondary virtio-blk) | ✅ | PR #127 |
| Ubuntu build VM (build profile) | ✅ | PR #127 |
| ARP keepalive + inbound-pending TTL pruning | ✅ | PR #127 |

---

## Networking

smoltcp userspace NAT relay in `pelagos-vz/src/nat_relay.rs`. No external
dependencies — no socket_vmnet, no vmnet.framework, no passt.

- VM IP: `192.168.105.2/24`, gateway `192.168.105.1`
- UDP: raw frame interception (bypasses smoltcp)
- TCP: smoltcp with ARP keepalive every 45s (prevents 60s smoltcp cache expiry)
- Recovery: `pkill -KILL -f "pelagos.*vm-daemon-internal"` → `rm vm.{pid,sock}` → `pelagos ping`

---

## Ubuntu Build VM

Profile: `--profile build`

- **Disk:** `out/build.img` (20 GB sparse ext4, Ubuntu 22.04 + Rust stable)
- **Config:** `~/.local/share/pelagos/profiles/build/vm.conf`
- **Cold boot:** ~61s to pong
- **SSH:** `pelagos --profile build vm ssh -- <cmd>`

**Provisioning:** `bash scripts/build-build-image.sh` — takes several minutes,
requires Alpine VM running. Reprovisioning required if `build.img` is missing
or corrupted.

**Critical build.img masking (already applied to existing image):**
- `systemd-networkd.service → /dev/null` — networkd disrupts eth0 ~60s into boot
- `systemd-resolved.service → /dev/null` — prevents dead resolv.conf symlink
- `/etc/resolv.conf` is a plain file (`8.8.8.8 / 1.1.1.1`), not a symlink
- `serial-getty@hvc0.service → /dev/null` — prevents 8-min boot stall
- `/etc/udev/rules.d/80-net-setup-link.rules → /dev/null` — prevents eth0 rename

**Cmdline:** `console=hvc0 net.ifnames=0 nohz=off cpuidle.off=1`

**Repair procedure** (if build.img needs fixing without reprovisioning):
Kill build VM → start Alpine with build.img as `--extra-disk` → SSH to Alpine
→ mount `/dev/vdb` → apply fixes → umount → stop Alpine → restart build VM.

---

## Next: Issue #119 — Build pelagos inside the VM

Infrastructure is ready. Remaining work is to actually run the build:

```bash
pelagos --profile build vm ssh -- bash -c 'rustup update'
pelagos --profile build vm ssh -- bash -c 'cd /root/pelagos && git pull && cargo build --release'
pelagos --profile build vm ssh -- bash -c 'cd /root/pelagos && cargo test'
```

See issue #119 for full details.

---

## Open Issues

| # | Title | Notes |
|---|---|---|
| #119 | Build pelagos inside the VM | VM ready; actual build not yet run |
| #118 | Release workflow (self-hosted runner) | Future work |
| #110 | VM memory default | Build VM uses 4 GB; default VM still TBD |
| #93 | Named volumes | Not started |
| #74 | Dynamic virtiofs shares | Not started |

---

## Build Reference

| Step | Command |
|---|---|
| Host binary | `cargo build -p pelagos-mac --release` |
| Sign (MANDATORY after build) | `bash scripts/sign.sh` |
| Guest (cross) | `RUSTFLAGS="-C link-self-contained=no" RUSTC="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rustc" cargo build -p pelagos-guest --target aarch64-unknown-linux-musl --release` |
| VM image (Alpine) | `bash scripts/build-vm-image.sh` |
| Build VM image (Ubuntu) | `bash scripts/build-build-image.sh` |
| All tests | `bash scripts/test-devcontainer-e2e.sh` |
| Individual suite | `bash scripts/test-devcontainer-e2e.sh --suite A` |

---

## Key Architecture Notes

- **smoltcp ARP:** 60s hardcoded TTL. ARP keepalive fires every 45s to prevent expiry.
  If keepalive stops (daemon killed), the first SYN after 60s will stall for 40s (TTL prune).
- **inbound_pending TTL:** 40s — prunes stale pre-sshd TCP sockets to avoid sshd MaxStartups flood.
- **ping_ssh:** ConnectTimeout=30s, retry every 15s, deadline 10 minutes, progress printed to stderr.
- **exec-into PID namespace:** pelagos does NOT create a separate PID namespace (NSpid=1 level).
  Fix: `unshare(CLONE_NEWNS)` + fresh `/proc` remount in grandchild makes `/proc/self` valid.
- **vsock:** in-process via `VZVirtioSocketDevice::connectToPort`, NOT a filesystem socket.
- **All AVF calls** serialized through a private DispatchQueue in pelagos-vz.
- **`pelagos ps` does NOT start the daemon.** Use `pelagos ping` to boot.
- **`cargo build` breaks signing** — always run `bash scripts/sign.sh` after every host build.
