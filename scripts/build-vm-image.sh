#!/usr/bin/env bash
# build-vm-image.sh — Build a minimal Alpine Linux ARM64 disk image for pelagos-mac.
#
# Requirements:
#   - macOS host with qemu installed (brew install qemu)
#   - Completed guest binary: cargo zigbuild -p pelagos-guest --target aarch64-unknown-linux-gnu --release
#
# Output:
#   out/vmlinuz        — Linux kernel (uncompressed)
#   out/initramfs.gz   — initial ramdisk
#   out/root.img       — ext4 root disk (2 GiB)
#
# Usage: ./scripts/build-vm-image.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
OUT="$REPO_ROOT/out"
WORK="$OUT/work"
ALPINE_VERSION="3.21"
ALPINE_ARCH="aarch64"
ALPINE_ISO="alpine-virt-${ALPINE_VERSION}.0-${ALPINE_ARCH}.iso"
ALPINE_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/${ALPINE_ARCH}/${ALPINE_ISO}"
ALPINE_MIRROR="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main"

DISK_SIZE="2G"
DISK_IMG="$OUT/root.img"
GUEST_BIN="$REPO_ROOT/target/aarch64-unknown-linux-gnu/release/pelagos-guest"

# ---------------------------------------------------------------------------
echo "[1/7] Setting up output directories"
# ---------------------------------------------------------------------------
mkdir -p "$OUT" "$WORK"

# ---------------------------------------------------------------------------
echo "[2/7] Downloading Alpine ISO"
# ---------------------------------------------------------------------------
if [ ! -f "$WORK/$ALPINE_ISO" ]; then
    curl -L --progress-bar -o "$WORK/$ALPINE_ISO" "$ALPINE_URL"
else
    echo "  (cached)"
fi

# ---------------------------------------------------------------------------
echo "[3/7] Extracting kernel and initrd from ISO"
# ---------------------------------------------------------------------------
# The Alpine virt ISO has the kernel at /boot/vmlinuz-virt and initrd at /boot/initramfs-virt
ISO_MNT="$WORK/iso_mnt"
mkdir -p "$ISO_MNT"

if command -v 7z &>/dev/null; then
    7z x -o"$ISO_MNT" "$WORK/$ALPINE_ISO" boot/ -y >/dev/null 2>&1 || true
elif command -v hdiutil &>/dev/null; then
    # macOS: mount the ISO and copy files
    MOUNT_PT=$(hdiutil attach -readonly -nobrowse -mountpoint "$ISO_MNT" "$WORK/$ALPINE_ISO" 2>/dev/null \
        | awk '{print $NF}' | tail -1)
    echo "  Mounted at $MOUNT_PT"
    cp "$ISO_MNT/boot/vmlinuz-virt" "$OUT/vmlinuz"
    cp "$ISO_MNT/boot/initramfs-virt" "$OUT/initramfs.gz"
    hdiutil detach "$ISO_MNT" 2>/dev/null || true
else
    echo "ERROR: need 7z or hdiutil to extract ISO contents"
    exit 1
fi

if [ ! -f "$OUT/vmlinuz" ]; then
    cp "$ISO_MNT/boot/vmlinuz-virt" "$OUT/vmlinuz" 2>/dev/null || \
    find "$ISO_MNT" -name "vmlinuz*" | head -1 | xargs -I{} cp {} "$OUT/vmlinuz"
fi
if [ ! -f "$OUT/initramfs.gz" ]; then
    cp "$ISO_MNT/boot/initramfs-virt" "$OUT/initramfs.gz" 2>/dev/null || \
    find "$ISO_MNT" -name "initramfs*" | head -1 | xargs -I{} cp {} "$OUT/initramfs.gz"
fi

echo "  kernel: $OUT/vmlinuz"
echo "  initrd: $OUT/initramfs.gz"

# ---------------------------------------------------------------------------
echo "[4/7] Creating root disk image ($DISK_SIZE)"
# ---------------------------------------------------------------------------
if [ ! -f "$DISK_IMG" ]; then
    qemu-img create -f raw "$DISK_IMG" "$DISK_SIZE"
fi

# ---------------------------------------------------------------------------
echo "[5/7] Building a bootable Alpine rootfs via QEMU"
# ---------------------------------------------------------------------------
# We boot the Alpine ISO in QEMU, run setup-alpine non-interactively,
# and install to the virtio disk. This requires network access.
#
# For the pilot this step can be done manually:
#   1. qemu-system-aarch64 -m 1024 -cpu cortex-a57 -M virt ... (boot alpine ISO)
#   2. setup-alpine (use defaults, install to /dev/vda)
#   3. apk add ...
#   4. Copy pelagos-guest binary
#
# Automated installation (requires qemu-system-aarch64):
if ! command -v qemu-system-aarch64 &>/dev/null; then
    echo "  qemu-system-aarch64 not found — skipping automated rootfs install."
    echo "  To complete: install qemu (brew install qemu) and re-run this script."
    echo ""
    echo "  Manual steps:"
    echo "    1. Boot Alpine ISO in QEMU"
    echo "    2. run setup-alpine, install to /dev/vda"
    echo "    3. Copy pelagos-guest to /usr/local/bin/ inside the VM"
    echo "    4. Add /etc/init.d/pelagos-guest service"
    exit 0
fi

AUTOINSTALL_SCRIPT="$WORK/answerfile"
cat > "$AUTOINSTALL_SCRIPT" <<'ANSWERFILE'
KEYMAPOPTS="us us"
HOSTNAMEOPTS="-n pelagos-vm"
INTERFACEOPTS="eth0"
DNSOPTS="-d local -n 8.8.8.8"
TIMEZONEOPTS="-z UTC"
PROXYOPTS="none"
APKREPOSOPTS="-1"
SSHDOPTS="-d"
NTPOPTS="-c busybox"
DISKOPTS="-m sys /dev/vda"
ANSWERFILE

echo "  Booting Alpine installer (this takes a few minutes)..."
# Non-interactive install via serial console
qemu-system-aarch64 \
    -M virt \
    -cpu cortex-a57 \
    -m 1024 \
    -smp 2 \
    -nographic \
    -serial mon:stdio \
    -kernel "$OUT/vmlinuz" \
    -initrd "$OUT/initramfs.gz" \
    -append "console=ttyAMA0 alpine_dev=cdrom:vfat" \
    -drive file="$WORK/$ALPINE_ISO",format=raw,if=virtio,readonly=on \
    -drive file="$DISK_IMG",format=raw,if=virtio \
    -netdev user,id=net0 \
    -device virtio-net-pci,netdev=net0 \
    -no-reboot &

QEMU_PID=$!

# Wait for login prompt, then run setup-alpine
# (In practice, you may want to use expect or a more robust mechanism.)
echo "  QEMU PID: $QEMU_PID"
echo "  NOTE: automated Alpine setup requires interactive steps."
echo "  Kill QEMU (kill $QEMU_PID) after installation completes."
wait $QEMU_PID || true

# ---------------------------------------------------------------------------
echo "[6/7] Copying pelagos-guest into disk image"
# ---------------------------------------------------------------------------
if [ ! -f "$GUEST_BIN" ]; then
    echo "  ERROR: guest binary not found at $GUEST_BIN"
    echo "  Build it with: cargo zigbuild -p pelagos-guest --target aarch64-unknown-linux-gnu --release"
    exit 1
fi

# Mount the disk image and copy the binary.
# This requires the image to already have a Linux ext4 rootfs installed.
ROOTFS_MNT="$WORK/rootfs_mnt"
mkdir -p "$ROOTFS_MNT"

if command -v ext4fuse &>/dev/null; then
    ext4fuse "$DISK_IMG" "$ROOTFS_MNT" -o loop
    cp "$GUEST_BIN" "$ROOTFS_MNT/usr/local/bin/pelagos-guest"
    chmod +x "$ROOTFS_MNT/usr/local/bin/pelagos-guest"
    install_service "$ROOTFS_MNT"
    umount "$ROOTFS_MNT" 2>/dev/null || diskutil unmount "$ROOTFS_MNT" 2>/dev/null || true
else
    echo "  ext4fuse not found (brew install ext4fuse)."
    echo "  Copy the guest binary manually:"
    echo "    $GUEST_BIN → /usr/local/bin/pelagos-guest (inside VM)"
fi

# ---------------------------------------------------------------------------
echo "[7/7] Done"
# ---------------------------------------------------------------------------
echo ""
echo "VM image artifacts:"
echo "  kernel:  $OUT/vmlinuz"
echo "  initrd:  $OUT/initramfs.gz"
echo "  disk:    $DISK_IMG"
echo ""
echo "Test with:"
echo "  pelagos --kernel $OUT/vmlinuz --initrd $OUT/initramfs.gz --disk $DISK_IMG ping"

install_service() {
    local mnt="$1"
    cat > "$mnt/etc/init.d/pelagos-guest" <<'SERVICE'
#!/sbin/openrc-run
name="pelagos-guest"
description="pelagos guest daemon"
command="/usr/local/bin/pelagos-guest"
command_background=true
pidfile="/run/${RC_SVCNAME}.pid"
output_log="/var/log/pelagos-guest.log"
error_log="/var/log/pelagos-guest.log"
depend() {
    need net
}
SERVICE
    chmod +x "$mnt/etc/init.d/pelagos-guest"
    ln -sf /etc/init.d/pelagos-guest "$mnt/etc/runlevels/default/pelagos-guest" 2>/dev/null || true
}
