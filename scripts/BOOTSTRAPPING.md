# pelagos-mac — First-Time Setup

This guide walks a new developer through getting a fully working build and
running the VM for the first time on Apple Silicon macOS.

---

## Prerequisites

### Homebrew packages

```bash
brew install squashfs zig
```

- **squashfs** — `unsquashfs` is required to extract kernel modules from the Alpine
  modloop during `build-vm-image.sh`
- **zig** — used as a cross-linker for the `aarch64-unknown-linux-musl` guest binary

### socket_vmnet (privileged helper, required for VM networking)

```bash
brew install socket_vmnet
sudo brew services start socket_vmnet
```

socket_vmnet runs as root and holds the `vmnet` handle that gives the VM a stable
NAT address (`192.168.105.x`). The pelagos daemon connects to it over a Unix socket
at `/opt/homebrew/var/run/socket_vmnet`.

### Rust toolchain

Install via [rustup](https://rustup.rs) if not already present:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Add the cross-compilation targets:

```bash
rustup target add aarch64-unknown-linux-musl   # guest binary (static musl)
rustup target add aarch64-unknown-linux-gnu    # alternative gnu target
```

Install `cargo-zigbuild` (cross-linker wrapper):

```bash
cargo install cargo-zigbuild
```

---

## Build

### 1. Build the host binary

```bash
cargo build -p pelagos-mac --release
```

### 2. Code-sign the host binary (mandatory after every build)

```bash
bash scripts/sign.sh
```

The `pelagos` binary must carry the `com.apple.security.virtualization` entitlement
or macOS will silently kill the daemon the moment it tries to use
Virtualization.framework. The VM will appear to start but will immediately stop.
Always re-sign after rebuilding.

### 3. Build the VM image (kernel + initramfs)

```bash
bash scripts/build-vm-image.sh
```

This downloads Alpine netboot artifacts, cross-compiles `pelagos-guest`, and
assembles a custom initramfs. The script is idempotent — re-running it skips
steps whose outputs are already up to date.

Outputs in `out/`:

| File | Description |
|---|---|
| `out/vmlinuz` | Alpine aarch64 LTS kernel |
| `out/initramfs-custom.gz` | Custom initramfs (guest daemon + pelagos binary + modules) |
| `out/root.img` | 8 GiB sparse ext4 disk (OCI image cache, formatted on first boot) |

> **Note:** `build-vm-image.sh` will use your local pelagos build from
> `~/Projects/pelagos/target/aarch64-unknown-linux-musl/release/pelagos` if it
> exists and is newer than the cached download. Otherwise it fetches the release
> binary matching `PELAGOS_VERSION` in the script.

---

## First boot

```bash
bash scripts/vm-ping.sh
```

Expected output: `pong`

Cold boot takes 1–3 seconds for the Alpine VM. The daemon persists in the
background; subsequent commands reuse the running VM with ~100 ms latency.

---

## Smoke tests

Run the full end-to-end test suite:

```bash
bash scripts/test-e2e.sh
```

For a cold-start test (kills the daemon first):

```bash
bash scripts/test-e2e.sh --cold
```

---

## Ubuntu build VM (optional)

A standalone Ubuntu 22.04 aarch64 VM is available as a named profile for building
and testing pelagos natively. It has `gcc`, `build-essential`, and Rust stable
pre-installed.

### Provision the build image (one-time, takes several minutes)

The Alpine VM image must already be built (step 3 above), as the provisioning script
boots the Alpine VM and installs Ubuntu into a secondary disk image via chroot.

```bash
bash scripts/build-build-image.sh
```

Creates `out/build.img` (20 GiB sparse ext4) and writes
`~/.local/share/pelagos/profiles/build/vm.conf`.

> **Path note:** `vm.conf` is written with absolute paths. If you provisioned on a
> different machine or username, update the `disk`/`kernel`/`initrd` paths in
> `~/.local/share/pelagos/profiles/build/vm.conf` to match your `$HOME`.

### Boot the build VM

```bash
bash scripts/vm-restart.sh --profile build
```

Expected output: `pong`. First boot takes 2–3 minutes (full systemd init).
Subsequent boots are faster once the ext4 journal is warmed up.

### SSH into the build VM

```bash
target/aarch64-apple-darwin/release/pelagos --profile build vm ssh
```

Or run a command directly:

```bash
target/aarch64-apple-darwin/release/pelagos --profile build vm ssh -- "source /root/.cargo/env && rustc --version"
```

> The Rust toolchain is installed at `/root/.cargo/`. SSH sessions do not source
> `~/.bashrc` automatically — either `source /root/.cargo/env` inline or add it
> to `/root/.profile` on the VM.

> **git SSL note:** The CA bundle is present at `/etc/ssl/certs/ca-certificates.crt`
> but git may not pick it up automatically. If you see "server certificate
> verification failed. CAfile: none", run once on the VM:
> ```bash
> git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt
> ```

---

## Troubleshooting

### VM starts but immediately stops; `vm status` says "stopped"

The binary was rebuilt without re-signing. Run `bash scripts/sign.sh`.

### `build-vm-image.sh` fails at "Extracting modloop"

`unsquashfs` is missing. Install it:
```bash
brew install squashfs
```

### `vm-ping.sh` times out on the build VM

The Ubuntu VM takes 2–3 minutes on first boot. Run `vm-ping.sh` again — it
will retry for up to 5 minutes total. You can watch boot progress via the
console:
```bash
target/aarch64-apple-darwin/release/pelagos --profile build vm console
```

### "I/O error (os error 5)" on image pulls

socket_vmnet degraded. Restart it, kill stale VM processes, and remove stale
state:

```bash
sudo brew services restart socket_vmnet
pkill -f 'pelagos.*daemon' || true
rm -f ~/.local/share/pelagos/vm.pid ~/.local/share/pelagos/vm.sock
```

If the error persists, the root disk may be corrupt (AVF: "storage device attachment
invalid"). Delete `out/root.img` and re-run `build-vm-image.sh`.

### "The boot loader is invalid" in daemon.log

The kernel or initramfs path in `vm.conf` is wrong or the files don't exist.
Verify `out/vmlinuz` and `out/initramfs-custom.gz` exist (run `build-vm-image.sh`),
then check the paths in `~/.local/share/pelagos/profiles/<profile>/vm.conf`.
