# VM Image Design

This document explains how the pelagos-mac VM image is built, how modules and
devices are initialized at boot, and why the approach was chosen.

---

## Kernel Choice: linux-lts over linux-virt

Alpine ships two kernel flavors relevant here:

| Flavor | Overlayfs | Virtio drivers | Use case |
|---|---|---|---|
| `linux-virt` | `CONFIG_OVERLAY_FS=n` | built-in | minimal cloud VMs |
| `linux-lts` | `CONFIG_OVERLAY_FS=m` | modules | general purpose |

`linux-virt` lacks `CONFIG_OVERLAY_FS` entirely. pelagos uses overlayfs for
container rootfs mounts, so `linux-virt` cannot run pelagos containers.
`linux-lts` has overlayfs as a module and ships a complete virtio driver set —
also as modules. This is why the guest uses `linux-lts`.

---

## Netboot Artifacts

The image is built from three artifacts downloaded directly from the Alpine CDN:

```
vmlinuz-lts      the kernel image
initramfs-lts    Alpine's base initramfs (minimal module subset)
modloop-lts      squashfs containing the complete linux-lts module tree
```

These are always version-matched: they are built from the same kernel source
tree by the same Alpine release. There are no ABI mismatches between the
running kernel and the modules staged from the modloop.

The kernel version (`KVER`) is detected dynamically at build time by inspecting
the modloop directory (`lib/modules/*/`), so the build script stays correct
across Alpine point releases without manual updates.

---

## Why Module Loading Was Hard

Three independent problems had to be solved before module loading worked
reliably.

### Problem 1: `/dev/null` did not exist

macOS `bsdtar` cannot create device nodes without root, so the initramfs `/dev`
directory was empty at boot. In POSIX sh, `2>/dev/null` when `/dev/null` does
not exist causes the shell to skip the **entire command** — not just suppress
stderr. Paired with `|| true`, the failure is silently swallowed. Every
`insmod ... 2>/dev/null || true` was a no-op. Zero modules loaded.

**Fix:** Rely on `CONFIG_DEVTMPFS_MOUNT=y`. Alpine linux-lts has this set. The
kernel itself mounts devtmpfs at `/dev` before executing the first userspace
process. `/dev/null`, `/dev/console`, `/dev/zero` etc. always exist when init
starts, regardless of what the build host could create at image-build time.

### Problem 2: `insmod` requires manual dependency ordering

`insmod` loads exactly one `.ko` file. Every transitive dependency must be
enumerated in the correct topological order by the caller. The virtio stack
has a non-trivial dependency graph:

```
virtio_ring → virtio → virtio_pci_legacy_dev
                     → virtio_pci_modern_dev
                     → virtio_pci → virtio_console
                                  → virtio_net → net_failover → failover
                                  → virtio-rng
                                  → vmw_vsock_virtio_transport → vsock
```

This is fragile. It breaks whenever the kernel's internal dependency graph
changes across Alpine releases.

**Fix:** Use `modprobe`. It reads `modules.dep` and resolves the full
transitive dependency chain automatically. One call per logical module; the
kernel figures out the rest.

### Problem 3: Wrong `modules.dep`

Alpine's base initramfs ships a `modules.dep` that covers only its own bundled
module subset. Manually-staged virtio/vsock modules are not in it. `modprobe`
fails to resolve them, and the guest daemon exits because vsock bind fails,
triggering a kernel panic: `Attempted to kill init! exitcode=0x00006500`.

**Fix:** At image-build time, replace the initramfs `modules.dep` with the one
from the modloop. The modloop's `modules.dep` covers the complete linux-lts
module tree. All companion files are replaced as well:

```
modules.dep          modules.dep.bin
modules.alias        modules.alias.bin
modules.builtin      modules.builtin.bin
modules.builtin.modinfo
modules.builtin.alias.bin
modules.symbols.bin  modules.devname
```

---

## Boot Sequence

```
kernel boots
  └── CONFIG_DEVTMPFS_MOUNT=y: kernel mounts devtmpfs at /dev
        /dev/null, /dev/console, /dev/zero etc. always present
  └── exec /init

/init (pass 1 — module loading)
  ├── mount proc
  ├── modprobe virtio_pci           AVF presents virtio over PCIe;
  │                                 without this, no virtio device is probed
  ├── modprobe virtio_console       /dev/hvc0 console
  ├── modprobe virtio-rng           entropy for TLS
  ├── modprobe vmw_vsock_virtio_transport   vsock for host↔guest comms
  ├── modprobe overlay              overlayfs for container rootfs
  └── modprobe virtio_net           network

/init (pass 2 — network + clock)
  ├── load virtio_net dependencies (failover, net_failover)
  ├── ip link up eth0
  ├── udhcpc -t 5 || static 192.168.105.2/24 via 192.168.105.1
  │     (CONFIG_PACKET=n in linux-lts-virt: udhcpc may fail;
  │      static fallback is the socket_vmnet subnet)
  ├── ping 8.8.8.8                  warms up AVF NAT before first TCP connect
  └── busybox timeout 10 ntpd -n -q -p pool.ntp.org
        VM clock starts at Unix epoch; TLS cert validation fails until synced
        10s timeout: DNS failure does not hang boot

exec pelagos-guest
  └── AF_VSOCK listener on port 1024
```

`virtio_pci` is loaded first because AVF presents every virtio device over PCIe.
Without the PCI transport driver, the kernel never probes any virtio device —
no console, no network, no vsock, no block storage. The VM runs at 99% CPU
with no output and no vsock response until this module is loaded.

---

## Image Build: What Gets Staged

At build time (`scripts/build-vm-image.sh`):

1. **Download** `vmlinuz-lts`, `initramfs-lts`, `modloop-lts` from Alpine CDN.
2. **Mount** the modloop squashfs; detect `KVER` from `lib/modules/*/`.
3. **Unpack** Alpine's base initramfs with `gunzip | cpio`.
4. **Stage virtio modules** from the modloop into `lib/modules/$KVER/kernel/`:
   - `virtio_pci_legacy_dev.ko`, `virtio_pci_modern_dev.ko`, `virtio_pci.ko`
   - `virtio_console.ko`, `virtio-rng.ko`
   - `vsock.ko`, `vmw_vsock_virtio_transport.ko`
   - `overlay.ko`
   - `virtio_net.ko`, `net_failover.ko`, `failover.ko`
5. **Replace `modules.dep`** and all companion files with the modloop's
   complete versions.
6. **Stage `modprobe`** from Alpine's `kmod` package.
7. **Stage `pelagos-guest`** binary (cross-compiled `aarch64-unknown-linux-musl`).
8. **Write init script** to `/init`; `chmod 755`.
9. **Repack** the initramfs with `find | cpio | gzip`.

**Kernel flavor stamping:** `out/.kernel-flavor` records the last-built flavor.
If it changes (e.g. `lts` → `virt`), the build script deletes stale kernel,
initramfs, and modloop artifacts before rebuilding. This prevents mismatches
where the running kernel and staged modules came from different source trees.

---

## Reliability and Repeatability

**Deterministic:**
- Modloop and kernel are always version-matched (same Alpine release).
- `KVER` is detected from the modloop, not hardcoded.
- Module loading is strictly sequential; there are no races in the init script.
- `modprobe` either succeeds or fails cleanly — no partial loads.

**Variable:**
- **NTP**: if DNS is unreachable, the VM boots with a Unix-epoch clock. OCI
  image pulls fail with TLS certificate errors. The 10-second timeout prevents
  a hang but does not fix DNS. Recovery: wait for DNS, then `pelagos vm stop`
  and restart.
- **socket_vmnet degradation**: vmnet.framework NAT state can degrade inside
  the socket_vmnet process (observed at ~round 18/40 in stress tests under
  macOS 26 beta). Recovery: `sudo brew services restart socket_vmnet` then
  `pelagos vm stop`. This is an Apple/Homebrew issue.
- **Boot time**: `ensure_running()` polls the vsock socket and retries.
  "Pong at try 6" is normal — it reflects kernel boot + module loading +
  network + NTP. Typical range is try 4–8. The polling loop is intentional;
  `UnixStream::connect` is NOT used for the readiness check because a
  connection attempt before the guest is ready blocks the guest's accept loop.

---

## Security

**Current posture (development signing):**
- Ad-hoc code signing with `com.apple.security.virtualization`. No
  notarization. macOS Gatekeeper blocks this for external users; a Developer ID
  signature + notarization is required for distribution.
- Host↔guest communication is exclusively over vsock — a hypervisor-mediated
  point-to-point channel. No external network entity can reach the guest daemon.
- The guest daemon listens on vsock port 1024. Today only `pelagos-mac` runs
  on the host, so there is no authentication on the vsock protocol. If
  multi-user or multi-tenant scenarios become relevant, mTLS is the fix
  (DESIGN.md: "If a persistent socket is ever needed, mTLS mandatory from day
  one").
- The guest runs as root inside the VM. This is standard for container
  runtimes: the runtime needs root to set up Linux namespaces and cgroups.
  Container processes run as the user specified by the image.

**Not yet hardened:**
- No mTLS on the vsock protocol.
- No seccomp filter on the guest daemon itself.
- Netboot artifact SHA256s are not pinned (Alpine HTTPS + implicit package
  manager verification).

---

## Comparison to Other Runtimes

| | Docker Desktop | Lima | OrbStack | pelagos-mac |
|---|---|---|---|---|
| VM layer | HyperKit / QEMU / AVF | QEMU / AVF | AVF (proprietary) | AVF via objc2-virtualization |
| Kernel | Custom LinuxKit | User-selected distro | Custom | Alpine linux-lts |
| Module loading | LinuxKit: built-in only | Full distro, standard modprobe | Proprietary | modprobe from modloop |
| `/dev` bootstrap | LinuxKit handles it | Standard distro init | Proprietary | CONFIG_DEVTMPFS_MOUNT=y |
| Host↔guest transport | gRPC over vsock | SSH + Lima-specific APIs | Proprietary vsock | Raw vsock, custom binary protocol |
| Subsystem dependency | Docker daemon (large) | Lima (large) | Proprietary | None |
| Go in the stack | Yes (dockerd, containerd, runc) | Yes | Unknown | Zero |
| Distribution signing | Notarized .dmg | None needed (QEMU) | Notarized | Ad-hoc dev; Developer ID for release |

The fundamental difference: Docker Desktop and Lima are **subsystem
dependencies** — you build around their lifecycle and conventions. pelagos-mac
owns its entire stack from the AVF call sites to the guest daemon protocol.

The tradeoff is that problems like module loading, `/dev` bootstrap, and clock
sync must be solved explicitly rather than inherited from an upstream project.
The solutions used here — `modprobe` + modloop `modules.dep` +
`CONFIG_DEVTMPFS_MOUNT=y` — are exactly what a proper embedded Linux init does.
Alpine's own initramfs-init uses the same approach; we are doing it in a
stripped-down context rather than a full Alpine install.

---

## Related

- `docs/DESIGN.md` — full architecture rationale and AVF binding approach
- `docs/VM_LIFECYCLE.md` — VM start/stop/status lifecycle and daemon model
- `docs/NETWORK_OPTIONS.md` — network attachment options and socket_vmnet setup
- `scripts/build-vm-image.sh` — the authoritative image build script
- `pelagos-guest/` — the guest daemon source
