# Build VMs Agent

Rebuild the pelagos-mac VM stack, ensuring all constituent parts are up to date.

## When to use

Use this agent when:
- Rebuilding VMs after code changes (pelagos, pelagos-guest, pelagos-mac)
- Preparing for end-to-end testing that requires fresh binaries
- After switching branches that change VM image contents (init scripts, modules, guest daemon)
- When unsure what's stale and what needs rebuilding

## What this agent does

1. **Assess staleness** -- compare artifact timestamps against source to determine what needs rebuilding
2. **Run `scripts/full-rebuild.sh`** -- the deterministic build backbone
3. **Diagnose failures** -- if a build step fails, read logs, identify root cause, and attempt fixes
4. **Verify** -- confirm installed versions, VM boots, SSH works

## Build dependency chain

Design principle: **Linux binaries are built in Linux (the build VM).
macOS binaries are built on macOS.** No cross-compilation.

```
build VM (Ubuntu, natively on Linux via virtiofs)
    |
    +-- pelagos glibc  (~/Projects/pelagos/target/release/)
    +-- pelagos musl   (~/Projects/pelagos/target/aarch64-unknown-linux-musl/release/)
    +-- pelagos-guest musl (~/Projects/pelagos-mac/target/aarch64-unknown-linux-musl/release/)
    +-- pelagos-ui Linux .deb (--with-ui, ~/Projects/pelagos-ui/target/release/bundle/deb/)
    |
build-vm-image.sh (stages bridge modules, pelagos musl binary, pelagos-guest)
    |
    v
out/initramfs-custom.gz + out/vmlinuz
    |
    +-- default VM (Alpine) boots from these
    +-- build VM (Ubuntu) also uses the same kernel/initramfs for boot
    |
dev-reinstall.sh
    |
    +-- pelagos-mac (macOS binaries, signed, brew-installed)
    +-- pelagos-guest hot-swap (pre-built binary swapped into running build VM)
    |
pelagos-ui (--with-ui)
    |
    +-- macOS DMG (built on macOS, brew cask installed)
    +-- Linux .deb (built in build VM)
```

## Key paths

| Artifact | Path | Built where |
|----------|------|-------------|
| pelagos (musl) | `~/Projects/pelagos/target/aarch64-unknown-linux-musl/release/pelagos` | Build VM natively (musl-tools) |
| pelagos (glibc) | `~/Projects/pelagos/target/release/pelagos` | Build VM natively |
| pelagos-guest (musl) | `target/aarch64-unknown-linux-musl/release/pelagos-guest` | Build VM natively (musl-tools) |
| pelagos-mac | `target/aarch64-apple-darwin/release/pelagos` | macOS natively, signed, brew-installed |
| pelagos-ui macOS | `~/Projects/pelagos-ui/dist/pelagos-ui-*.dmg` | macOS natively (Tauri + npm) |
| pelagos-ui Linux | `~/Projects/pelagos-ui/target/release/bundle/deb/*.deb` | Build VM natively (Tauri + npm) |
| Alpine initramfs | `out/initramfs-custom.gz` | macOS (`scripts/build-vm-image.sh`, packaging only) |
| Ubuntu kernel | `out/ubuntu-vmlinuz` | Extracted by `scripts/build-build-image.sh` |

## Prerequisites

Before running the build:
- Build VM must be running: `pelagos --profile build vm start`
- Rust toolchain via rustup on macOS (not Homebrew's cargo) -- for pelagos-mac
- `unsquashfs` installed on macOS: `brew install squashfs-tools` -- for initramfs module extraction
- Build VM has musl-tools + rustup musl target -- provisioned automatically by `full-rebuild.sh` on first run

## Procedure

1. Check if the build VM is running. If not, report it and stop.
2. Check staleness: compare `stat -f "%Sm"` timestamps of key binaries against their source repos' latest commits.
3. Run `bash scripts/full-rebuild.sh` with appropriate skip flags based on what's stale.
4. If any step fails, read the error output, diagnose the cause, and attempt a fix.
5. After success, verify:
   - `pelagos --version` reports the expected version
   - `pelagos vm status` shows running
   - `pelagos vm ssh -- uname -a` succeeds
6. Report what was rebuilt and current versions.

## Common failure modes

| Symptom | Cause | Fix |
|---------|-------|-----|
| `musl-gcc: not found` in build VM | musl-tools not installed | `full-rebuild.sh` provisions automatically; or manually: `apt-get install musl-tools` in build VM |
| `error[E0463]: can't find crate` for musl target | rustup musl target missing | `full-rebuild.sh` provisions automatically; or manually: `rustup target add aarch64-unknown-linux-musl` in build VM |
| Build VM unreachable | VM not started or wrong profile | `pelagos --profile build vm start` |
| `ENETUNREACH` on build VM SSH | Stale utun / routing conflict | Stop all VMs, restart build VM |
| Initramfs not rebuilt despite fresh pelagos | Cached `out/work/pelagos-*` binary is stale | Delete `out/work/pelagos-*` and re-run |
| `vm status` says stopped after start | Missing entitlement signature | Run `bash scripts/sign.sh` |
| pelagos binary inside VM is old version | glibc build in build VM not run | SSH into build VM and `cargo build --release` |

## Skip flags (passed through to full-rebuild.sh)

- `--skip-pelagos` -- skip all Linux builds (pelagos glibc + musl, pelagos-guest musl)
- `--skip-vm-image` -- skip rebuilding the Alpine VM image
- `--skip-mac` -- skip rebuilding macOS binaries (pelagos-mac, pelagos-tui, pelagos-pfctl)
- `--skip-guest` -- skip hot-swapping pelagos-guest into the build VM
- `--no-restart` -- do not restart the default VM after rebuild
- `--with-ui` -- build pelagos-ui for macOS (DMG + brew cask) and Linux (.deb in build VM)
