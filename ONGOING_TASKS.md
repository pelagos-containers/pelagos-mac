# pelagos-mac — Ongoing Tasks

*Last updated: 2026-03-12, SHA dded184 (post-PR #66)*

---

## Current State

**Phase 2 + Phase 3 + Phase 4 COMPLETE.** The full container lifecycle, all three VM access
modes, and VS Code devcontainer CLI integration all work end-to-end on real hardware. All 31
e2e tests pass (`bash scripts/test-e2e.sh`).

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
| Persistent OCI image cache (`/dev/vda` ext2) | ✅ | PR #50 |
| ECR Public test image (no rate limits) | ✅ | PR #50 |
| devpts mount + PTY job control | ✅ | PR #38/#40 |
| `pelagos vm console` (hvc0 serial) | ✅ | PR #51 |
| `pelagos vm ssh` (dropbear + ed25519 key) | ✅ | PR #52 |
| `devcontainer up` (VS Code devcontainer CLI) | ✅ | PR #66 |

---

## Phase 3 — VM Access (Epic #41) ✅ COMPLETE

All three options for direct VM access are done (closed in PR #51, PR #52):

### Option A — `pelagos vm shell` (vsock) ✅ DONE (PR #45)

Interactive `/bin/sh` inside the VM over vsock. No container namespaces.
TTY and non-TTY modes both work.

### Option B — `pelagos vm console` (hvc0) ✅ DONE (PR #51)

Attaches to the VM's hvc0 serial console. Raw boot output visible; root shell
auto-spawns on hvc0. Ctrl-] detaches. Non-TTY/pipe mode with 2s drain for scripting.

### Option C — `pelagos vm ssh` (dropbear) ✅ DONE (PR #52)

Runs `dropbear` sshd in the VM. Key pair generated at `~/.local/share/pelagos/vm_key`
during `make image`; public key baked into initramfs as `root`'s `authorized_keys`.
`pelagos vm ssh [-- cmd args]` connects to `root@192.168.105.2` using the stored key.

---

## Phase 3 — NAT Reliability (issue #26) ✅ COMPLETE

socket_vmnet migration done (merged, branch `feat/socket-vmnet`).
VM gets a stable `192.168.105.2` IP via DHCP (socket_vmnet shared mode) or
static fallback. `pelagos vm ssh` depends on this stable IP.

---

## Phase 4 — VS Code Dev Container support (Epic #55)

Goal: make pelagos-mac a backend for the [devcontainer CLI](https://github.com/devcontainers/cli).

| Subtask | Issue | Status |
|---|---|---|
| Docker CLI shim (`pelagos-docker`) | #56 | ✅ Done (PR #62 + #63) |
| Native port forwarding (`-p host:container`) | #57 | ✅ Done (PR #59) |
| glibc/Ubuntu container image compatibility | #58 | ✅ Done (PR #61) |

- Port forwarding (PR #59): daemon binds `TcpListener` on host, proxies to `192.168.105.2:container_port`.
- Ubuntu/glibc (PR #61): resolv.conf bind-mount workaround in pelagos-guest; deeper fix tracked in issue #60.
- Docker shim (PR #62+#63): `pelagos-docker` binary mapping Docker CLI to `pelagos` subcommands;
  sidecar labels JSON; auto-detects pelagos binary. `-e/--env` wired end-to-end. Full e2e test suite added.

**Remaining Phase 4 work:**
- issue #60: make pelagos runtime handle DNS natively (no guest-side workaround)
- issue #64 closed (PR #65): docker exec, version, info, inspect auto-detect all done
- devcontainer up smoke test ✅ (PR #66): `devcontainer up --docker-path pelagos-docker` succeeds end-to-end (outcome="success"). All docker shim gaps filled: ps -q/filter, events polling, --mount bind syntax, auto-name, exec -u, ImageConfig.

**Phase 4 complete.** All 31 e2e tests pass. devcontainer CLI integration confirmed working.

See `docs/VM_LIFECYCLE.md` for the VM networking topology (socket_vmnet,
192.168.105.x subnet).

---

## Phase 4 — Signed Installer (not yet tracked)

`.pkg` installer for distribution. Requires:
- Developer ID Application signature + notarization
- Hardened runtime entitlement
- `com.apple.security.virtualization` in the signed entitlements

---

## Build Reference

| Step | Command |
|---|---|
| Host binary | `cargo build -p pelagos-mac --release` |
| Guest (cross) | `RUSTFLAGS="-C link-self-contained=no" RUSTC="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rustc" cargo build -p pelagos-guest --target aarch64-unknown-linux-musl --release` |
| VM image | `bash scripts/build-vm-image.sh` |
| Code-sign | `codesign --sign - --entitlements pelagos-mac/entitlements.plist --force target/aarch64-apple-darwin/release/pelagos` |
| All tests | `bash scripts/test-e2e.sh` |
| Cold-start test | `bash scripts/test-e2e.sh --cold` |
| Interactive container | `bash scripts/test-interactive.sh` |
| VM shell | `bash scripts/vm-shell.sh` |
