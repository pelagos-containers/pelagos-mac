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

GUEST_BIN="$REPO_ROOT/target/aarch64-unknown-linux-musl/release/pelagos-guest"
DISK_IMG="$OUT/root.img"
INITRAMFS_OUT="$OUT/initramfs-custom.gz"
KERNEL_OUT="$OUT/vmlinuz"

PELAGOS_VERSION="0.24.0"
PELAGOS_BIN="$WORK/pelagos-aarch64-linux"
PELAGOS_URL="https://github.com/skeptomai/pelagos/releases/download/v${PELAGOS_VERSION}/pelagos-aarch64-linux"

# Mozilla CA bundle — needed by the statically-linked musl pelagos binary for TLS.
# Sourced from certs/cacert.pem in this repo (update with scripts/update-certs.sh).
CA_BUNDLE="$SCRIPT_DIR/../certs/cacert.pem"

# ---------------------------------------------------------------------------
echo "[1/8] Setting up output directories"
# ---------------------------------------------------------------------------
mkdir -p "$OUT" "$WORK"

# ---------------------------------------------------------------------------
echo "[2/8] Downloading Alpine virt ISO ($ALPINE_VERSION $ALPINE_ARCH)"
# ---------------------------------------------------------------------------
if [ ! -f "$WORK/$ALPINE_ISO" ]; then
    curl -L --progress-bar -o "$WORK/$ALPINE_ISO" "$ALPINE_URL"
else
    echo "  (cached: $WORK/$ALPINE_ISO)"
fi

# ---------------------------------------------------------------------------
echo "[3/8] Extracting kernel and initramfs from ISO"
# ---------------------------------------------------------------------------
if [ ! -f "$KERNEL_OUT" ] || [ ! -f "$WORK/initramfs-virt" ]; then
    # bsdtar (libarchive, ships with macOS) reads ISO 9660 natively — no mount needed.
    ISO_BOOT="$WORK/iso_boot"
    rm -rf "$ISO_BOOT"
    mkdir -p "$ISO_BOOT"
    bsdtar -xf "$WORK/$ALPINE_ISO" -C "$ISO_BOOT" boot/vmlinuz-virt boot/initramfs-virt

    rm -f "$WORK/initramfs-virt"
    cp "$ISO_BOOT/boot/initramfs-virt" "$WORK/initramfs-virt"
    RAW_VZ="$ISO_BOOT/boot/vmlinuz-virt"

    # Alpine 6.x kernels use arm64 zboot format: an EFI/PE stub ("MZ"+"zimg" magic)
    # that wraps a gzip-compressed arm64 Image. VZLinuxBootLoader on macOS 26+ does not
    # accept gzip-compressed kernels; extract the zboot payload and decompress to a raw
    # arm64 Image (starts with MZ magic + ARMd at 0x38).
    if python3 - "$RAW_VZ" "$KERNEL_OUT" <<'PY'
import struct, sys, shutil, gzip
src, dst = sys.argv[1], sys.argv[2]
with open(src, 'rb') as f:
    hdr = f.read(32)
if hdr[4:8] != b'zimg':
    # Not zboot; check if it's gzip-compressed and decompress if so.
    if hdr[:2] == b'\x1f\x8b':
        with open(src, 'rb') as f:
            raw = gzip.decompress(f.read())
        with open(dst, 'wb') as f:
            f.write(raw)
        print(f"  kernel format: gzip → raw arm64 Image ({len(raw)//1024//1024} MiB)")
    else:
        shutil.copy(src, dst)
        print(f"  kernel format: plain arm64 Image")
    sys.exit(0)
offset = struct.unpack_from('<I', hdr, 8)[0]
size   = struct.unpack_from('<I', hdr, 12)[0]
comp   = hdr[24:28].decode('ascii', errors='replace').rstrip('\x00')
print(f"  zboot kernel: {comp}-compressed payload at offset {offset}, {size} bytes")
with open(src, 'rb') as f:
    f.seek(offset)
    payload = f.read(size)
# Decompress the payload (gzip) to get the raw arm64 Image.
raw = gzip.decompress(payload)
with open(dst, 'wb') as f:
    f.write(raw)
print(f"  decompressed: {len(raw)//1024//1024} MiB raw arm64 Image")
PY
    then
        : # python3 handled the copy/extraction
    else
        echo "ERROR: kernel extraction failed" >&2; exit 1
    fi

    echo "  kernel:  $KERNEL_OUT"
    echo "  initrd:  $WORK/initramfs-virt"
else
    echo "  (cached)"
fi

# ---------------------------------------------------------------------------
echo "[4/8] Building pelagos-guest (cross-compile)"
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
            --target aarch64-unknown-linux-musl \
            --release
    echo "  Built: $GUEST_BIN"
else
    echo "  (cached: $GUEST_BIN)"
fi

# ---------------------------------------------------------------------------
echo "[5/8] Downloading pelagos runtime binary (v${PELAGOS_VERSION})"
# ---------------------------------------------------------------------------
if [ ! -f "$PELAGOS_BIN" ]; then
    curl -L --progress-bar -o "$PELAGOS_BIN" "$PELAGOS_URL"
    chmod 755 "$PELAGOS_BIN"
    echo "  Downloaded: $PELAGOS_BIN"
else
    echo "  (cached: $PELAGOS_BIN)"
fi

# ---------------------------------------------------------------------------
echo "[6/8] Staging Mozilla CA bundle (for TLS inside VM)"
# ---------------------------------------------------------------------------
if [ ! -f "$CA_BUNDLE" ]; then
    echo "ERROR: certs/cacert.pem not found. Run scripts/update-certs.sh to fetch it." >&2
    exit 1
fi
echo "  (using repo bundle: $CA_BUNDLE)"

# ---------------------------------------------------------------------------
echo "[7/8] Building custom initramfs"
# ---------------------------------------------------------------------------
if [ ! -f "$INITRAMFS_OUT" ]; then
    KVER="6.12.1-3-virt"

    # --- Extract vsock modules from the modloop squashfs ---
    MODLOOP="$WORK/modloop-virt"
    if [ ! -f "$MODLOOP" ]; then
        bsdtar -xf "$WORK/$ALPINE_ISO" -C "$WORK" boot/modloop-virt
        mv "$WORK/boot/modloop-virt" "$MODLOOP"
        rmdir "$WORK/boot" 2>/dev/null || true
    fi
    MODLOOP_DIR="$WORK/modloop_extracted"
    if [ ! -d "$MODLOOP_DIR/modules" ]; then
        rm -rf "$MODLOOP_DIR"
        unsquashfs -force -d "$MODLOOP_DIR" "$MODLOOP" 2>/dev/null || true
    fi
    VSOCK_SRC="$MODLOOP_DIR/modules/$KVER/kernel/net/vmw_vsock"

    # --- Extract the Alpine initramfs and patch it in-place ---
    # bsdtar handles all Alpine's special files (symlinks, device nodes, etc.)
    # macOS can't create /dev device nodes without root; that's fine — our init
    # mounts devtmpfs which recreates them from the kernel at boot.
    INITRD_TMP="$WORK/initramfs_tmp"
    rm -rf "$INITRD_TMP"
    mkdir -p "$INITRD_TMP"
    bsdtar -xpf "$WORK/initramfs-virt" -C "$INITRD_TMP" 2>/dev/null || true

    # Add vsock modules
    mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/net/vmw_vsock"
    for ko in vsock.ko vmw_vsock_virtio_transport_common.ko vmw_vsock_virtio_transport.ko; do
        if [ -f "$VSOCK_SRC/$ko" ]; then
            cp "$VSOCK_SRC/$ko" "$INITRD_TMP/lib/modules/$KVER/kernel/net/vmw_vsock/$ko"
        else
            echo "  WARNING: $ko not found — vsock may not work" >&2
        fi
    done

    # Add virtio-net and virtio-rng modules (not built into the Alpine virt kernel).
    # virtio-net load order: failover → net_failover → virtio_net
    # virtio-rng load order: rng-core → virtio-rng
    NETMOD_BASE="$MODLOOP_DIR/modules/$KVER/kernel"
    for src_path in \
        "$NETMOD_BASE/net/core/failover.ko" \
        "$NETMOD_BASE/drivers/net/net_failover.ko" \
        "$NETMOD_BASE/drivers/net/virtio_net.ko" \
        "$NETMOD_BASE/drivers/char/hw_random/rng-core.ko" \
        "$NETMOD_BASE/drivers/char/hw_random/virtio-rng.ko"
    do
        dst_dir="$INITRD_TMP/lib/modules/$KVER/$(dirname "${src_path#$NETMOD_BASE/}")"
        mkdir -p "$dst_dir"
        if [ -f "$src_path" ]; then
            cp "$src_path" "$dst_dir/"
        else
            echo "  WARNING: $(basename $src_path) not found" >&2
        fi
    done

    # Ensure kernel vfs mountpoints exist (Alpine initramfs may already have them).
    mkdir -p "$INITRD_TMP/proc" "$INITRD_TMP/sys" "$INITRD_TMP/dev"

    # Add guest daemon and pelagos runtime.
    mkdir -p "$INITRD_TMP/usr/local/bin"
    cp "$GUEST_BIN" "$INITRD_TMP/usr/local/bin/pelagos-guest"
    chmod 755 "$INITRD_TMP/usr/local/bin/pelagos-guest"
    cp "$PELAGOS_BIN" "$INITRD_TMP/usr/local/bin/pelagos"
    chmod 755 "$INITRD_TMP/usr/local/bin/pelagos"

    # Write a udhcpc default script so DHCP can configure the interface and default route.
    # Without this script, udhcpc obtains a lease but never applies it (no ip addr, no route).
    mkdir -p "$INITRD_TMP/usr/share/udhcpc"
    cat > "$INITRD_TMP/usr/share/udhcpc/default.script" << 'UDHCPC'
#!/bin/sh
case "$1" in
    bound|renew)
        busybox ip addr flush dev "$interface"
        busybox ip addr add "$ip/$mask" dev "$interface"
        [ -n "$router" ] && busybox ip route add default via "$router" dev "$interface"
        ;;
    deconfig)
        busybox ip addr flush dev "$interface"
        ;;
esac
UDHCPC
    chmod 755 "$INITRD_TMP/usr/share/udhcpc/default.script"

    # Install Mozilla CA bundle so the statically-linked musl pelagos binary
    # can verify TLS certificates when pulling OCI images from Docker Hub.
    mkdir -p "$INITRD_TMP/etc/ssl/certs"
    cp "$CA_BUNDLE" "$INITRD_TMP/etc/ssl/certs/ca-certificates.crt"

    # Replace /init: mounts vfs, loads vsock modules, execs pelagos-guest.
    # Without root= in cmdline the kernel uses the initramfs as root and runs /init.
    cat > "$INITRD_TMP/init" <<INIT_EOF
#!/bin/sh

# Mount kernel virtual filesystems — required by container namespaces and cgroups.
busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
busybox mount -t proc     proc     /proc 2>/dev/null || true
busybox mount -t sysfs    sysfs    /sys 2>/dev/null || true
busybox mkdir -p /sys/fs/cgroup
busybox mount -t cgroup2  cgroup2  /sys/fs/cgroup 2>/dev/null || true

# Load virtio-rng early so the kernel CSPRNG is seeded before TLS is attempted.
busybox insmod /lib/modules/$KVER/kernel/drivers/char/hw_random/rng-core.ko 2>/dev/null || true
busybox insmod /lib/modules/$KVER/kernel/drivers/char/hw_random/virtio-rng.ko 2>/dev/null || true

# Load vsock kernel modules.
busybox insmod /lib/modules/$KVER/kernel/net/vmw_vsock/vsock.ko 2>/dev/null || true
busybox insmod /lib/modules/$KVER/kernel/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko 2>/dev/null || true
busybox insmod /lib/modules/$KVER/kernel/net/vmw_vsock/vmw_vsock_virtio_transport.ko 2>/dev/null || true

# Load virtio-net modules (not built into the Alpine virt kernel).
busybox insmod /lib/modules/$KVER/kernel/net/core/failover.ko 2>/dev/null || true
busybox insmod /lib/modules/$KVER/kernel/drivers/net/net_failover.ko 2>/dev/null || true
busybox insmod /lib/modules/$KVER/kernel/drivers/net/virtio_net.ko 2>/dev/null || true

# Configure networking: static IP on the AVF NAT subnet (192.168.64.0/24).
# The Alpine virt kernel lacks AF_PACKET support (CONFIG_PACKET=n), so udhcpc
# cannot be used. AVF NAT always uses 192.168.64.0/24 with gateway .1.
busybox ip link set lo up
busybox ip link set eth0 up
busybox ip addr add 192.168.64.2/24 dev eth0
busybox ip route add default via 192.168.64.1
echo "[pelagos-init] network ready"
# Write a minimal resolv.conf so DNS resolution works inside the VM.
busybox mkdir -p /etc
echo 'nameserver 8.8.8.8' > /etc/resolv.conf
echo 'nameserver 8.8.4.4' >> /etc/resolv.conf

busybox mkdir -p /tmp /run /run/pelagos
busybox mount -t tmpfs tmpfs /tmp

# Gate on network readiness: loop until the first ping to 8.8.8.8 succeeds,
# then exit immediately. Exits in ~1-2s when the AVF NAT is ready on first
# attempt; waits up to 30s if it takes longer. Without this gate, pelagos's
# first outbound TCP connection races with NAT initialization and fails.
i=0
while [ \$i -lt 15 ]; do
    busybox ping -c 1 -W 3 -q 8.8.8.8 >/dev/null 2>&1 && break
    i=\$((i+1))
done

export PELAGOS_IMAGE_STORE=/run/pelagos

exec /usr/local/bin/pelagos-guest
INIT_EOF
    chmod 755 "$INITRD_TMP/init"

    # Repack as a single gzip'd newc cpio (no concatenation needed).
    (cd "$INITRD_TMP" && bsdtar --format=newc -cf - .) | gzip -9 > "$INITRAMFS_OUT"

    echo "  initramfs: $INITRAMFS_OUT"
else
    echo "  (cached: $INITRAMFS_OUT)"
fi

# ---------------------------------------------------------------------------
echo "[8/8] Creating placeholder disk image"
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
echo "Next: make build && make sign && make test-e2e"
echo "(kernel cmdline: console=hvc0  — no root=, initramfs is root, /init is pelagos)"
