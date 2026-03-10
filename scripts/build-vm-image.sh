#!/usr/bin/env bash
# build-vm-image.sh — Build a minimal Alpine Linux ARM64 initramfs image for pelagos-mac.
#
# Strategy: appended cpio initramfs — no QEMU, no ext4, no interactive install.
#
#   1. Download Alpine virt ISO for aarch64 (3.21).
#   2. Extract vmlinuz-virt and initramfs-virt via hdiutil (macOS).
#   3. Build pelagos-guest if the binary is missing.
#   4. Create an "additions" cpio archive containing our custom init and guest binary.
#   5. Concatenate Alpine's initramfs + our additions cpio.
#      The Linux kernel processes concatenated cpio archives sequentially; our
#      files are overlaid on top of Alpine's busybox environment.
#   6. Create a 64 MiB placeholder raw disk image (AVF requires a block device).
#
# Requirements:
#   - macOS with hdiutil (Xcode CLT) and bsdtar (libarchive, ships with macOS)
#   - cargo + cargo-zigbuild for the guest cross-compilation step
#
# Output (all idempotent — re-running skips completed steps):
#   out/vmlinuz               — Alpine aarch64 kernel
#   out/initramfs-custom.gz   — Alpine initramfs + pelagos additions
#   out/root.img              — 64 MiB placeholder disk
#
# Kernel cmdline to use:  console=hvc0 rdinit=/pelagos-init

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
OUT="$REPO_ROOT/out"
WORK="$OUT/work"

ALPINE_VERSION="3.21"
ALPINE_ARCH="aarch64"
ALPINE_ISO="alpine-virt-${ALPINE_VERSION}.0-${ALPINE_ARCH}.iso"
ALPINE_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/${ALPINE_ARCH}/${ALPINE_ISO}"

GUEST_BIN="$REPO_ROOT/target/aarch64-unknown-linux-gnu/release/pelagos-guest"
DISK_IMG="$OUT/root.img"
INITRAMFS_OUT="$OUT/initramfs-custom.gz"
KERNEL_OUT="$OUT/vmlinuz"

# ---------------------------------------------------------------------------
echo "[1/6] Setting up output directories"
# ---------------------------------------------------------------------------
mkdir -p "$OUT" "$WORK"

# ---------------------------------------------------------------------------
echo "[2/6] Downloading Alpine virt ISO ($ALPINE_VERSION $ALPINE_ARCH)"
# ---------------------------------------------------------------------------
if [ ! -f "$WORK/$ALPINE_ISO" ]; then
    curl -L --progress-bar -o "$WORK/$ALPINE_ISO" "$ALPINE_URL"
else
    echo "  (cached: $WORK/$ALPINE_ISO)"
fi

# ---------------------------------------------------------------------------
echo "[3/6] Extracting kernel and initramfs from ISO"
# ---------------------------------------------------------------------------
if [ ! -f "$KERNEL_OUT" ] || [ ! -f "$WORK/initramfs-virt" ]; then
    # bsdtar (libarchive, ships with macOS) reads ISO 9660 natively — no mount needed.
    bsdtar -xf "$WORK/$ALPINE_ISO" -C "$WORK" \
        boot/vmlinuz-virt boot/initramfs-virt
    mv "$WORK/boot/vmlinuz-virt"   "$KERNEL_OUT"
    mv "$WORK/boot/initramfs-virt" "$WORK/initramfs-virt"
    rmdir "$WORK/boot" 2>/dev/null || true
    echo "  kernel:  $KERNEL_OUT"
    echo "  initrd:  $WORK/initramfs-virt"
else
    echo "  (cached)"
fi

# ---------------------------------------------------------------------------
echo "[4/6] Building pelagos-guest (cross-compile)"
# ---------------------------------------------------------------------------
if [ ! -f "$GUEST_BIN" ]; then
    echo "  Cross-compiling pelagos-guest for aarch64-unknown-linux-gnu..."
    # Use the rustup-managed cargo so the Linux sysroot is available.
    RUSTUP_CARGO="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo"
    if [ ! -x "$RUSTUP_CARGO" ]; then
        # Fall back to whatever cargo is on PATH — user may have a working setup.
        RUSTUP_CARGO="cargo"
    fi
    PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:$PATH" \
        "$RUSTUP_CARGO" zigbuild \
            --manifest-path "$REPO_ROOT/Cargo.toml" \
            -p pelagos-guest \
            --target aarch64-unknown-linux-gnu \
            --release
    echo "  Built: $GUEST_BIN"
else
    echo "  (cached: $GUEST_BIN)"
fi

# ---------------------------------------------------------------------------
echo "[5/6] Building custom initramfs"
# ---------------------------------------------------------------------------
if [ ! -f "$INITRAMFS_OUT" ]; then
    ADDITIONS="$WORK/additions"
    rm -rf "$ADDITIONS"
    mkdir -p "$ADDITIONS/proc" \
             "$ADDITIONS/sys" \
             "$ADDITIONS/dev" \
             "$ADDITIONS/usr/local/bin"

    # Custom init script — runs as PID 1 when the kernel uses rdinit=/pelagos-init.
    cat > "$ADDITIONS/pelagos-init" <<'INIT_EOF'
#!/bin/sh
mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
sleep 1
exec /usr/local/bin/pelagos-guest
INIT_EOF
    chmod 755 "$ADDITIONS/pelagos-init"

    # Guest daemon binary.
    cp "$GUEST_BIN" "$ADDITIONS/usr/local/bin/pelagos-guest"
    chmod 755 "$ADDITIONS/usr/local/bin/pelagos-guest"

    # Create the additions as an uncompressed newc cpio archive.
    ADDITIONS_CPIO="$WORK/additions.cpio"
    (cd "$ADDITIONS" && bsdtar --format=newc -cf - .) > "$ADDITIONS_CPIO"

    # Concatenate: Alpine initramfs + our additions.
    # The kernel processes each cpio archive in the concatenated stream in order;
    # later files overwrite earlier ones if they have the same path.
    cat "$WORK/initramfs-virt" "$ADDITIONS_CPIO" > "$INITRAMFS_OUT"

    echo "  initramfs: $INITRAMFS_OUT"
else
    echo "  (cached: $INITRAMFS_OUT)"
fi

# ---------------------------------------------------------------------------
echo "[6/6] Creating placeholder disk image"
# ---------------------------------------------------------------------------
if [ ! -f "$DISK_IMG" ]; then
    # AVF requires at least one block device in the VM config.
    # Our init doesn't mount it, so 64 MiB of zeros is sufficient.
    dd if=/dev/zero of="$DISK_IMG" bs=1m count=64 2>/dev/null
    echo "  disk: $DISK_IMG (64 MiB placeholder)"
else
    echo "  (cached: $DISK_IMG)"
fi

# ---------------------------------------------------------------------------
echo ""
echo "Done. VM image artifacts:"
echo "  kernel:   $KERNEL_OUT"
echo "  initramfs: $INITRAMFS_OUT"
echo "  disk:      $DISK_IMG"
echo ""
echo "Run with:"
echo "  pelagos \\"
echo "    --kernel $KERNEL_OUT \\"
echo "    --initrd $INITRAMFS_OUT \\"
echo "    --disk   $DISK_IMG \\"
echo "    --cmdline 'console=hvc0 rdinit=/pelagos-init' \\"
echo "    ping"
