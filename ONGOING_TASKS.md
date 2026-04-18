# pelagos-mac — Ongoing Tasks

*Last updated: 2026-04-09 — distribution pipeline complete; v0.6.5 released; brew install + vm init + run verified end-to-end*

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
| `pelagos run -p HOST:CONTAINER` (port forwarding) | ✅ | PR #146 + this session |
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
| Build VM: full pelagos test suite (297/303, 0 fail) | ✅ | PR #136 + pelagos PRs |

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

## Epic #119 — pelagos builder VM + full test suite verified ✅ (PR #125/#129/#131/#136/#208)

Ubuntu 24.04 aarch64 VM running as `--profile build`. Boots in ~16s, SSH-ready.
`cargo build --release` verified: pelagos v0.60.8, ELF64 AArch64, 1m 50s.
`cargo test` (full suite): **313 passed, 0 failed, 7 ignored** on Ubuntu 24.04 + kernel 6.11.
All container, networking, cgroup, seccomp, namespace, and overlayfs integration tests pass.

Fixes required to reach full pass:
- pelagos#128: `SYS_chmod` → `SYS_fchmodat` in integration tests (aarch64 syscall table)
- pelagos PR: `call_credential_helper` PATH injection via `Command::env` (data race fix)
- pelagos PR: DNS label length typo in `test_parse_qname_labels`
- build VM provisioning: `overlay` added to `/etc/modules` (Ubuntu 6.8 HWE ships it as `=m`)
- build VM provisioning: `flash-kernel` removed before apt install (blocks post-install hooks in VMs)
- build VM provisioning: `sudo` added to apt install list (required by `test_rootless_bridge_error`)

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

### Completed this session (2026-04-09)

- **Distribution pipeline — issues #118/#137** ✅ (v0.6.5)
  - `pelagos vm init` subcommand: locates VM artifacts (Homebrew pkgshare or dev `out/`), copies `root.img` to writable state dir, writes `vm.conf`
  - `update-tap` job in `.github/workflows/release.yml`: on tag push, fetches sha256s of released tarballs, renders Homebrew formula, pushes to `pelagos-containers/homebrew-tap` via GitHub API
  - `veth.ko` staged in Alpine modloop fallback path (CI path); previously missing → `pelagos run` failed with "Unknown device type" after brew install
  - Explicit post-insmod diagnostic in init script: distinguishes `CONFIG_VETH=y` (built-in, silent pass) from genuinely absent (WARNING to /dev/console); consistent with virtio-rng pattern from #211
  - End-to-end verified: `brew install pelagos-containers/tap/pelagos-mac → pelagos vm init → pelagos ping → pelagos run alpine echo hello` all work from a clean Homebrew install
  - `docs/INSTALL.md` created; README updated to lead with Homebrew install, fix version (v0.4.0 → v0.6.5), remove stale `skeptomai/tap` references
  - Closed #118, #137

### Completed this session (2026-03-28)

- **`pelagos compose` proxy** ✅ (PR #198, open)
  - `pelagos compose up/down/ps/logs` proxies subcommands to the Linux `pelagos compose` binary via vsock
  - Host paths under `$HOME` auto-translated to `/mnt/share0/...` (virtiofs)
  - PortDispatcher registers macOS-side port listeners before stack starts
  - Bugfix: `compose down --volumes` clap conflict with global `-v/--volume` flag

- **Home monitoring stack compose config** — `~/Projects/home-monitoring/pelagos/`
  - `compose.reml` with all 8 services (snmp-exporter, mktxp, graphite-exporter, truenas-api-exporter, plex-exporter, alertmanager, prometheus, grafana)
  - Config files, grafana provisioning, start.sh, check.sh
  - Secrets via `.env` (gitignored)
  - Core images pulled; **blocked on pelagos#157**

- **Root-caused pelagos#157: compose fails for non-root-user images**
  - `pelagos compose` (and `pelagos run --security-opt seccomp=default --user N`) fails with
    "Invalid argument (os error 22)" for any image with a non-root `User` (prometheus=65534, grafana, etc.)
  - Root cause: `docker_default_filter()` in `seccomp.rs` incorrectly blocks `setuid`/`setgid`;
    pelagos installs seccomp at step 4.849, then calls `setuid` at step 8.5 → EPERM →
    `io::Error::other()` → Rust spawn reports EINVAL (via `raw_os_error().unwrap_or(EINVAL)`)
  - Fix needed in pelagos: remove `setuid`/`setgid` from blocked_syscalls (Docker's real profile allows them)
  - Fixed in pelagos PR #158 (`fix/seccomp-allow-setuid-setgid`); merged
  - Filed: https://github.com/pelagos-containers/pelagos/issues/157

- **`nft_masq.ko` added to VM initramfs** ✅
  - `masquerade` nftables expression requires `nft_masq.ko`; was absent from Alpine initramfs
  - Module extracted from `linux-modules-6.8.0-106-generic_6.8.0-106.106~22.04.1_arm64.deb` (Ubuntu base modules)
  - Added to `scripts/build-vm-image.sh` and `scripts/build-build-image.sh`; staged at boot via `modprobe nft_masq`
  - Without this, `pelagos compose up` with bridge networks failed: `Error: Could not process rule: No such file or directory`

- **`pelagos-dns` added to VM initramfs** ✅
  - Container hostname DNS resolution daemon; required for inter-container communication on bridge networks
  - Built as musl static binary (`aarch64-unknown-linux-musl`); staged to `/usr/local/bin/pelagos-dns`
  - Added to rebuild trigger and copy block in `scripts/build-vm-image.sh`
  - Without this, Prometheus could not scrape `alertmanager:9093`, `grafana:3000`, etc.

- **Core monitoring stack verified end-to-end** ✅
  - `pelagos compose up -f compose-core.reml` starts prometheus + alertmanager + grafana
  - All three scrape targets report `health: up` in Prometheus
  - Grafana v12.4.2 accessible at `http://localhost:3000`, database ok

### Completed in previous session (2026-03-25)

- **Epic #178 — OCI image management** ✅ (PR #192, merged to main)
  - Phase 1: `GuestCommand` variants `ImageLs|Pull|Rm|Tag|Inspect` added to vsock protocol in both `pelagos-mac` and `pelagos-guest`
  - Phase 2: `pelagos image ls|pull|rm|tag|inspect` CLI subcommands; `ls` defaults to human-readable table, `--json` for machine output; `inspect` filters client-side by reference
  - Phase 3: TUI image screen (`I`): browse, pull (`p`), delete with confirm (`d`), inspect JSON overlay (`Enter`), `R` pre-fills run palette with selected image

### Completed this session (2026-04-18)

- **IPv6 / dual-stack NAT relay — issue #229** (PR open, branch `feat/ipv6-relay`)
  - Phase 1: smoltcp dual-stack (`proto-ipv6`); dynamic gateway LL derived from VM MAC
    (`fe80::00ff:fe{MAC[3..5]}`) to work around VZ MLD snooping; ICMPv6 echo handler;
    NDP Neighbor Advertisement handler; 14 unit tests.
  - Phase 2: ULA addressing — relay holds `fd00::1/64`, VM assigned `fd00::2/64` in init script;
    manual NA handler for ULA (same VZ MLD snooping constraint as LL).
  - Phase 3: IPv6 UDP raw handler (`handle_udp_frame_v6`); `fd00::1` UDP echoed locally;
    external destinations proxied via `[::]:0` host socket.
  - Bug fix: `icmpv6_echo_reply` previously faked replies to ANY destination, including external
    internet addresses — silently lying about IPv6 reachability. Now restricted to relay's own
    addresses only.
  - Smoke test: `scripts/test-ipv6-smoke.sh` — 6/6 passing (Phase 1 LL echo, Phase 2 ULA echo,
    Phase 3 UDP round-trip).
  - **Phase 4 blocked**: outbound IPv6 TCP (VM→internet) and ICMPv6/UDP to external destinations
    require either pf NAT66, a tun interface with userspace SNAT, or per-protocol raw-socket
    proxying. Design decision needed before implementation. See `docs/NETWORK_OPTIONS.md §IPv6`.

### Next priorities

- **Phase 4 — IPv6 outbound (issue #229)**: design decision required. Options: pf NAT66 (kernel,
  privileged), tun+SNAT (userspace, no privilege), per-protocol raw socket proxy (no privilege,
  incremental). See `docs/NETWORK_OPTIONS.md §IPv6` for full analysis.
- **Home monitoring stack** — core stack (prometheus + alertmanager + grafana) running end-to-end. Full 8-service stack (`compose.reml`) needs `.env` with real credentials (MIKROTIK_PASSWORD, TRUENAS_API_KEY, PLEX_TOKEN, GF_SMTP_PASSWORD). Once credentials in place: verify all exporters up, import Grafana dashboards from k8s setup.
- **Epic #135 — pelagos-ui** — Tauri + Svelte macOS management GUI (new). M1: container list. Blocked on #98 (JSON ps output).
- **Port forwarding** ✅ — `pelagos run -p 8080:80 nginx:alpine` + `curl http://localhost:8080/`
  works end-to-end via smoltcp relay + DNAT. Two **pelagos bugs** remain that prevent it
  from working cleanly out of the box without manual intervention:
  - **pelagos#bug: ip_forward not set** — `enable_port_forwards` installs DNAT rules but
    does not enable `ip_forward`. DNAT'd packets can't traverse eth0→pelagos0 bridge
    without it. Workaround in pelagos-mac: init script sets `ip_forward=1` unconditionally.
  - **pelagos#bug: stale DNAT rules accumulate** — `enable_port_forwards` evicts stale
    entries by checking if `/run/netns/{name}` exists, but pelagos doesn't remove the
    netns file when a container dies uncleanly. Result: stale IPs from prior runs stay in
    PREROUTING and match before the current container's rule. Fix needed in pelagos:
    eviction should check if the container watcher process is alive, not just the netns file.
- **`docker volume inspect`** — `create/ls/rm` works; `inspect` not implemented.
- **Dynamic virtiofs shares** (#74) — current per-path shares require knowing all
  paths at VM start time.
- **Signed installer** — `.pkg` for distribution. Requires Developer ID + notarization
  + `com.apple.security.virtualization`. Not yet scoped.

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
