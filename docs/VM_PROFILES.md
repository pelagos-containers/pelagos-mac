# VM Profiles and Container Image Selection

This document covers the two VM profiles (`default` and `build`), what each
is for, the dividing lines between them, and the case for a lighter middle
ground.

---

## The Two Profiles

### `default` — Alpine initramfs VM

```
kernel:    out/vmlinuz           (Alpine linux-lts)
initrd:    out/initramfs-custom.gz
disk:      out/root.img          (overlay persistence layer)
memory:    2048 MB
cpus:      2
ping:      vsock
```

The VM IS the initramfs. The entire userspace is Alpine + pelagos-guest. At
boot, `/init` loads virtio modules, brings up the network, then `exec`s
pelagos-guest, which listens on AF_VSOCK port 1024. The disk (`root.img`) is
an ext4 volume used only for container state (overlay layers, images, volumes).
There is no persistent installed toolchain.

Suited for: running containers, everyday `pelagos run/ps/logs` usage.

### `build` — Ubuntu persistent VM

```
kernel:    out/ubuntu-vmlinuz    (Ubuntu 6.8 HWE)
initrd:    out/ubuntu-initrd.img
disk:      out/build.img         (LABEL=ubuntu-build, persistent)
memory:    4096 MB
cpus:      4
ping:      ssh
cmdline:   net.ifnames=0 cpuidle.off=1
```

A full persistent Ubuntu disk image. The `ping_mode = ssh` means pelagos
reaches it via SSH rather than vsock — there is no pelagos-guest daemon.
You SSH in directly and compile there, like a conventional remote Linux
machine. Packages installed via `apt` survive VM restarts.

The two kernel cmdline flags are necessary for Ubuntu + AVF stability:
- `net.ifnames=0`: prevents udev from renaming `eth0` → `enp0sN`, which
  would drop the IP configured by the initramfs before `switch_root`.
- `cpuidle.off=1`: disables PSCI deep idle states. Ubuntu 6.8 HWE parks
  vCPUs in PSCI CPU_SUSPEND; AVF does not reliably deliver hrtimers to
  parked vCPUs, causing `rcu_preempt` stalls. Disabling cpuidle fixes this
  at the cost of slightly higher idle CPU.

Suited for: long-running builds that need a persistent toolchain, workflows
where direct shell access (not container-mediated) is more natural.

---

## The Dividing Lines

| | `default` (Alpine VM) | `build` (Ubuntu VM) |
|---|---|---|
| Host↔guest | vsock + pelagos-guest | SSH |
| Toolchain persistence | no (container-local) | yes (VM disk) |
| Disk size | ~300 MB | several GB |
| Boot time | ~4–8 vsock retries | SSH-ready takes longer |
| Userspace | Alpine busybox | Full Ubuntu |
| libc in VM | musl | glibc |
| Kernel cmdline workarounds | none needed | `net.ifnames=0 cpuidle.off=1` |
| Memory / CPU | 2 GB / 2 | 4 GB / 4 |

The key architectural difference: `default` routes everything through the
pelagos vsock protocol; `build` bypasses it entirely and is a conventional
SSH-accessible VM. They solve different problems.

---

## `vm shell` vs `vm ssh`

Two commands provide interactive access; they work differently and apply
to different profiles.

### `pelagos vm shell`

```bash
pelagos vm shell                        # default profile
pelagos --profile myprofile vm shell    # any vsock profile
```

Sends a `Shell` command over vsock to pelagos-guest, which forks a shell
inside the VM. **Requires vsock + pelagos-guest** — works only for profiles
with `ping_mode = vsock` (i.e. the `default` Alpine profile).

Does **not** require an SSH key. No SSH involved at all.

### `pelagos vm ssh`

```bash
pelagos vm ssh                          # default profile
pelagos --profile build vm ssh          # build profile (most common use)
pelagos --profile build vm ssh -- "cmd" # run a non-interactive command
```

Runs a real SSH session routed through the smoltcp NAT relay proxy — the
host has no direct route to the VM's 192.168.105.2 address, so SSH is
tunnelled through a local proxy port that pelagos manages. Works for **any**
profile that has an SSH daemon, including the `default` Alpine profile
(dropbear) and the `build` Ubuntu profile (openssh).

**Requires `~/.local/share/pelagos/vm_key`** — an ed25519 key pair generated
by `build-vm-image.sh` and baked into the VM image as `root/.ssh/authorized_keys`.
The key is a global artifact shared by all profiles.

### The key after a brew install or upgrade

`vm_key` is **not shipped** in the Homebrew formula — it is generated once
locally by `build-vm-image.sh` and lives only on the developer's machine.
After a fresh install or a brew upgrade that replaces the VM image, the
key baked into the new initramfs was generated in CI and is not available.

To restore `vm ssh` access after a brew upgrade:

```bash
# Rebuild the default VM image locally — generates a new key pair and
# bakes the public half into the initramfs:
bash scripts/build-vm-image.sh

# Re-initialise so the daemon picks up the new initramfs:
pelagos vm stop
pelagos vm init --force
pelagos ping
```

The `build` profile's `build.img` has its `authorized_keys` updated by
`build-build-image.sh` from the same key, so running `build-vm-image.sh`
first (which sets the key) before `build-build-image.sh` keeps everything
in sync.

### Summary

| | `vm shell` | `vm ssh` |
|---|---|---|
| Transport | vsock → pelagos-guest | SSH → smoltcp relay → VM port 22 |
| Requires key | No | Yes (`~/.local/share/pelagos/vm_key`) |
| Works on `default` (Alpine) | Yes | Yes (dropbear) |
| Works on `build` (Ubuntu) | **No** (no pelagos-guest) | Yes (openssh) |
| Non-interactive commands | No | Yes (`-- "cmd"`) |
| Primary use case | Quick Alpine shell | Build VM access, scripted commands |

---

## Container Image Choice Inside the `default` VM

When you run `pelagos run <image>`, the container image's userspace determines
what tools are available — not the Alpine VM host. This is where most
"alpine vs ubuntu" friction actually happens.

### Alpine containers

- Base image: ~10 MB
- libc: **musl** — some Rust crates with C FFI dependencies link against
  libseccomp, libcap, or other glibc-linked system libraries. Those crates
  either need to be compiled from source against musl or will fail to link.
- Userspace: busybox — many GNU tool options are missing or behave differently.
- Init: Alpine's OpenRC does not run inside a container (no init system needed);
  this is not an issue in practice.
- Package manager: `apk` — smaller package selection than `apt`.

Good for: running applications that are statically linked or musl-compatible.
Not ideal for: compiling software with non-trivial C dependencies.

### Ubuntu containers

- Base image: ~700 MB+
- libc: glibc — broad compatibility.
- Userspace: full GNU coreutils, bash, apt.
- Suitable for any Rust project regardless of C dependency complexity.

Too large for: use cases where image pull time or disk usage matters.

---

## The Middle Ground: `debian:bookworm-slim`

For building software (including pelagos itself) inside the `default` VM, a
Debian slim container is the right balance.

**Why Debian slim over Alpine:**
- glibc — no musl surprises; libseccomp-dev, libcap-dev, and other C
  library dependencies install cleanly via `apt`.
- Full GNU toolchain available.
- `apt` package selection matches Ubuntu without the image size overhead.

**Why Debian slim over Ubuntu:**
- ~100 MB base vs ~700 MB+ — 7× smaller.
- Enough for `build-essential`, `rustup`, and project-specific dev deps.
- No Ubuntu-specific cruft.

**Persistence via virtiofs mount:** Rust's incremental build cache lives in the
source tree, which is bind-mounted from the host via virtiofs. The cache
persists on the macOS host across container restarts without any persistent VM
disk. The container itself is ephemeral; only the source and build artifacts
need to persist, and they already do.

**Example workflow:**
```bash
pelagos run --rm \
  -v $HOME/Projects/pelagos:/workspace \
  -w /workspace \
  debian:bookworm-slim \
  bash -c "apt-get install -y build-essential libseccomp-dev && cargo build"
```

(In practice, a custom image with the toolchain pre-installed avoids the
`apt-get` step on every run.)

---

## When to Use What

| Goal | Recommendation |
|---|---|
| Run an application container | `default` profile, any OCI image |
| Build a pure-Rust project (no C deps) | `default` profile, `alpine` or `rust:alpine` |
| Build a Rust project with C deps (libseccomp, libcap, etc.) | `default` profile, `debian:bookworm-slim` |
| Build pelagos itself | `default` profile, `debian:bookworm-slim` |
| Long-running build environment with persistent toolchain | `build` profile (Ubuntu SSH VM) |
| Direct Linux shell access without container indirection | `build` profile |

---

## Where VM Profile Definitions Live

All VM image build scripts and profile configuration belong in `pelagos-mac`:

- `scripts/build-vm-image.sh` — builds the `default` Alpine initramfs VM
- `scripts/build-build-image.sh` — builds the `build` Ubuntu persistent VM
- `~/.local/share/pelagos/profiles/<name>/vm.conf` — per-profile runtime config

**Why not `pelagos`?** The `pelagos` repo is the Linux container runtime. It
has no knowledge of Apple Virtualization Framework, VM image formats, or
macOS-side profile management. VM lifecycle is entirely `pelagos-mac`'s domain.

**Why not a separate repo?** The VM images are not independently useful
artifacts — they exist only to run pelagos inside AVF on Apple Silicon. A
separate repo would create synchronization overhead (guest binary version
matching, kernel config changes, etc.) with no benefit. If VM image recipes
ever become general-purpose (e.g., a base Debian dev image useful outside
pelagos), extracting them then is straightforward.

---

## Related

- `docs/VM_IMAGE_DESIGN.md` — Alpine initramfs internals, module loading, boot sequence
- `docs/VM_LIFECYCLE.md` — VM start/stop/status lifecycle and daemon model
- `scripts/build-vm-image.sh` — Alpine VM build script
- `scripts/build-build-image.sh` — Ubuntu build VM build script
