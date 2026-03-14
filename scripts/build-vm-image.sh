#!/usr/bin/env bash
# build-vm-image.sh — Build a minimal Alpine Linux ARM64 initramfs image for pelagos-mac.
#
# Strategy: appended cpio initramfs — no QEMU, no ext4, no interactive install.
#
#   1. Download Alpine LTS netboot artifacts for aarch64 (3.21):
#      vmlinuz-lts, initramfs-lts, modloop-lts (no ISO extraction needed).
#   2. Decompress vmlinuz-lts to a raw arm64 Image (handles zboot/gzip formats).
#   3. Build pelagos-guest if the binary is missing.
#   4. Extract vsock/virtio modules from the modloop squashfs.
#   5. Overlay our custom init + binaries on top of Alpine's initramfs.
#   6. Repack as a single gzip'd cpio archive.
#   7. Create a 512 MiB placeholder raw disk image (AVF requires a block device).
#
# Kernel flavor detection: if the kernel flavor (lts vs virt) has changed since
# the last build, stale kernel + initramfs artifacts are deleted automatically
# before rebuilding, so you never need to manually rm out/ after a flavor switch.
#
# Requirements:
#   - macOS with bsdtar (libarchive, ships with macOS) and unsquashfs (squashfs-tools)
#   - cargo for the guest cross-compilation step
#
# Output (all idempotent — re-running skips completed steps):
#   out/vmlinuz               — Alpine aarch64 LTS kernel (raw arm64 Image)
#   out/initramfs-custom.gz   — Alpine initramfs + pelagos additions
#   out/root.img              — 512 MiB placeholder disk
#
# Kernel cmdline to use:  console=hvc0
# (the kernel's default rdinit=/init picks up our /init from the initramfs)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
OUT="$REPO_ROOT/out"
WORK="$OUT/work"

ALPINE_VERSION="3.21"
ALPINE_ARCH="aarch64"
ALPINE_FLAVOR="lts"   # "lts" | "virt" — drives all flavor-specific paths
ALPINE_NETBOOT="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/${ALPINE_ARCH}/netboot"

VMLINUZ_DL="$WORK/vmlinuz-${ALPINE_FLAVOR}"
INITRAMFS_DL="$WORK/initramfs-${ALPINE_FLAVOR}"
MODLOOP_DL="$WORK/modloop-${ALPINE_FLAVOR}"

GUEST_BIN="$REPO_ROOT/target/aarch64-unknown-linux-musl/release/pelagos-guest"
DISK_IMG="$OUT/root.img"
INITRAMFS_OUT="$OUT/initramfs-custom.gz"
KERNEL_OUT="$OUT/vmlinuz"

PELAGOS_VERSION="0.29.0"
PELAGOS_BIN="$WORK/pelagos-${PELAGOS_VERSION}-aarch64-linux"
PELAGOS_URL="https://github.com/skeptomai/pelagos/releases/download/v${PELAGOS_VERSION}/pelagos-aarch64-linux"

PASST_PKG="passt-2025.01.21-r0"
PASST_APK="$WORK/${PASST_PKG}.apk"
PASST_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community/${ALPINE_ARCH}/${PASST_PKG}.apk"
PASTA_BIN="$WORK/pasta-bin"

DROPBEAR_PKG="dropbear-2024.86-r0"
DROPBEAR_APK="$WORK/${DROPBEAR_PKG}.apk"
DROPBEAR_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${DROPBEAR_PKG}.apk"
DROPBEAR_BIN="$WORK/dropbear-bin"

UTMPS_LIBS_PKG="utmps-libs-0.1.2.3-r2"
UTMPS_LIBS_APK="$WORK/${UTMPS_LIBS_PKG}.apk"
UTMPS_LIBS_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${UTMPS_LIBS_PKG}.apk"

SKALIBS_PKG="skalibs-libs-2.14.3.0-r0"
SKALIBS_APK="$WORK/${SKALIBS_PKG}.apk"
SKALIBS_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${SKALIBS_PKG}.apk"

ZLIB_PKG="zlib-1.3.1-r2"
ZLIB_APK="$WORK/${ZLIB_PKG}.apk"
ZLIB_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${ZLIB_PKG}.apk"

# SSH key for 'pelagos vm ssh': generated once per user, baked into the initramfs.
PELAGOS_STATE_DIR="$HOME/.local/share/pelagos"
SSH_KEY_FILE="$PELAGOS_STATE_DIR/vm_key"

# Mozilla CA bundle — needed by the statically-linked musl pelagos binary for TLS.
# Sourced from certs/cacert.pem in this repo (update with scripts/update-certs.sh).
CA_BUNDLE="$SCRIPT_DIR/../certs/cacert.pem"

# ---------------------------------------------------------------------------
echo "[1/8] Setting up output directories"
# ---------------------------------------------------------------------------
mkdir -p "$OUT" "$WORK"

# ---------------------------------------------------------------------------
# Kernel flavor change detection: if the previously built kernel used a
# different flavor (e.g. "virt"), delete stale kernel + initramfs artifacts
# so they are rebuilt with the current flavor.  The disk image is NOT deleted
# (it holds the persistent OCI image cache and is flavor-independent).
# ---------------------------------------------------------------------------
FLAVOR_STAMP="$OUT/.kernel-flavor"
if [ -f "$FLAVOR_STAMP" ]; then
    OLD_FLAVOR="$(cat "$FLAVOR_STAMP")"
    if [ "$OLD_FLAVOR" != "$ALPINE_FLAVOR" ]; then
        echo "  [!] Kernel flavor changed: $OLD_FLAVOR → $ALPINE_FLAVOR"
        echo "      Removing stale kernel, initramfs, and module cache..."
        rm -f "$KERNEL_OUT" "$INITRAMFS_OUT"
        rm -rf "$WORK/modloop_extracted"
        # Remove old flavor's downloaded netboot artifacts if present.
        rm -f "$WORK/vmlinuz-${OLD_FLAVOR}" \
              "$WORK/initramfs-${OLD_FLAVOR}" \
              "$WORK/modloop-${OLD_FLAVOR}"
        # Remove old virt ISO artifacts (legacy; no-ops if already gone).
        rm -f "$WORK"/alpine-virt-*.iso "$WORK/initramfs-virt" "$WORK/modloop-virt"
        rm -rf "$WORK/iso_boot" "$WORK/boot"
        # Remove old unversioned pelagos binary (legacy naming without version).
        rm -f "$WORK/pelagos-aarch64-linux"
        rm -f "$FLAVOR_STAMP"
        echo "      Done. Rebuilding with $ALPINE_FLAVOR kernel."
    fi
fi

# ---------------------------------------------------------------------------
echo "[2/8] Downloading Alpine ${ALPINE_FLAVOR} netboot artifacts"
# ---------------------------------------------------------------------------
# Download the three netboot files directly — no ISO extraction needed.
# These are cached in out/work/ after the first download.
for artifact in vmlinuz initramfs modloop; do
    dest="$WORK/${artifact}-${ALPINE_FLAVOR}"
    if [ ! -f "$dest" ]; then
        echo "  Downloading ${artifact}-${ALPINE_FLAVOR}..."
        curl -L --progress-bar -o "$dest" "${ALPINE_NETBOOT}/${artifact}-${ALPINE_FLAVOR}"
    else
        echo "  (cached: $dest)"
    fi
done

# ---------------------------------------------------------------------------
echo "[3/8] Decompressing/staging kernel"
# ---------------------------------------------------------------------------
if [ ! -f "$KERNEL_OUT" ]; then
    RAW_VZ="$VMLINUZ_DL"

    # Alpine kernels use arm64 zboot format (EFI/PE stub wrapping gzip-compressed
    # arm64 Image) or plain gzip.  VZLinuxBootLoader on macOS 26+ requires a raw
    # arm64 Image.  Decompress as needed.
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
        echo "ERROR: kernel decompression failed" >&2; exit 1
    fi
    echo "  kernel:  $KERNEL_OUT"
else
    echo "  (cached: $KERNEL_OUT)"
fi

# ---------------------------------------------------------------------------
echo "[4/8] Building pelagos-guest (cross-compile)"
# ---------------------------------------------------------------------------
if [ ! -f "$GUEST_BIN" ]; then
    echo "  Cross-compiling pelagos-guest for aarch64-unknown-linux-musl..."
    RUSTUP_CARGO="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo"
    if [ ! -x "$RUSTUP_CARGO" ]; then
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
echo "[5b/8] Generating SSH key pair (for pelagos vm ssh)"
# ---------------------------------------------------------------------------
mkdir -p "$PELAGOS_STATE_DIR"
if [ ! -f "$SSH_KEY_FILE" ]; then
    ssh-keygen -t ed25519 -N "" -f "$SSH_KEY_FILE" -C "pelagos-vm" -q
    echo "  Generated: $SSH_KEY_FILE"
else
    echo "  (cached: $SSH_KEY_FILE)"
fi

# ---------------------------------------------------------------------------
echo "[5c/8] Downloading dropbear SSH server (${DROPBEAR_PKG})"
# ---------------------------------------------------------------------------
extract_so() {
    local apk="$1" soname="$2" dest="$3"
    local tmpdir
    tmpdir=$(mktemp -d)
    bsdtar -xf "$apk" -C "$tmpdir" 2>/dev/null || true
    local found
    found=$(find "$tmpdir" -name "$soname" 2>/dev/null | head -1)
    if [ -n "$found" ]; then
        cp "$found" "$dest"
        rm -rf "$tmpdir"
        return 0
    fi
    rm -rf "$tmpdir"
    return 1
}

if [ ! -f "$DROPBEAR_BIN" ]; then
    if [ ! -f "$DROPBEAR_APK" ]; then
        curl -L --progress-bar -o "$DROPBEAR_APK" "$DROPBEAR_URL"
    fi
    DROPBEAR_EXTRACT="$WORK/dropbear-extract"
    rm -rf "$DROPBEAR_EXTRACT"
    mkdir -p "$DROPBEAR_EXTRACT"
    bsdtar -xf "$DROPBEAR_APK" -C "$DROPBEAR_EXTRACT" 2>/dev/null || true
    if [ -f "$DROPBEAR_EXTRACT/usr/sbin/dropbear" ]; then
        cp "$DROPBEAR_EXTRACT/usr/sbin/dropbear" "$DROPBEAR_BIN"
        chmod 755 "$DROPBEAR_BIN"
        echo "  Extracted dropbear: $DROPBEAR_BIN"
    else
        echo "ERROR: could not extract dropbear from $DROPBEAR_APK" >&2
        exit 1
    fi
else
    echo "  (cached: $DROPBEAR_BIN)"
fi

LIBUTMPS="$WORK/libutmps.so.0.1"
LIBSKARNET="$WORK/libskarnet.so.2.14"
LIBZ="$WORK/libz.so.1"

if [ ! -f "$LIBUTMPS" ]; then
    [ ! -f "$UTMPS_LIBS_APK" ] && curl -L --progress-bar -o "$UTMPS_LIBS_APK" "$UTMPS_LIBS_URL"
    extract_so "$UTMPS_LIBS_APK" "libutmps.so.0.1" "$LIBUTMPS" || \
        { echo "ERROR: libutmps.so.0.1 not found in $UTMPS_LIBS_APK" >&2; exit 1; }
    echo "  Extracted libutmps.so.0.1"
fi
if [ ! -f "$LIBSKARNET" ]; then
    [ ! -f "$SKALIBS_APK" ] && curl -L --progress-bar -o "$SKALIBS_APK" "$SKALIBS_URL"
    extract_so "$SKALIBS_APK" "libskarnet.so.2.14" "$LIBSKARNET" || \
        { echo "ERROR: libskarnet.so.2.14 not found in $SKALIBS_APK" >&2; exit 1; }
    echo "  Extracted libskarnet.so.2.14"
fi
if [ ! -f "$LIBZ" ]; then
    [ ! -f "$ZLIB_APK" ] && curl -L --progress-bar -o "$ZLIB_APK" "$ZLIB_URL"
    extract_so "$ZLIB_APK" "libz.so.1" "$LIBZ" || \
        { echo "ERROR: libz.so.1 not found in $ZLIB_APK" >&2; exit 1; }
    echo "  Extracted libz.so.1"
fi

# ---------------------------------------------------------------------------
echo "[5d/8] Downloading pasta (userspace networking for pelagos build)"
# ---------------------------------------------------------------------------
if [ ! -f "$PASTA_BIN" ]; then
    if [ ! -f "$PASST_APK" ]; then
        curl -L --progress-bar -o "$PASST_APK" "$PASST_URL"
    fi
    PASST_EXTRACT="$WORK/passt-extract"
    rm -rf "$PASST_EXTRACT"
    mkdir -p "$PASST_EXTRACT"
    bsdtar -xf "$PASST_APK" -C "$PASST_EXTRACT" 2>/dev/null || true
    if [ -f "$PASST_EXTRACT/usr/bin/pasta" ]; then
        cp "$PASST_EXTRACT/usr/bin/pasta" "$PASTA_BIN"
        chmod 755 "$PASTA_BIN"
        echo "  Extracted pasta: $PASTA_BIN"
    else
        echo "ERROR: pasta not found in $PASST_APK" >&2
        exit 1
    fi
else
    echo "  (cached: $PASTA_BIN)"
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

# --- Extract modloop squashfs and detect kernel version (cached after first run) ---
MODLOOP_DIR="$WORK/modloop_extracted"
if [ ! -d "$MODLOOP_DIR/modules" ]; then
    echo "  Extracting modloop-${ALPINE_FLAVOR} (this takes a moment)..."
    rm -rf "$MODLOOP_DIR"
    unsquashfs -force -d "$MODLOOP_DIR" "$MODLOOP_DL" 2>/dev/null || true
fi

# Detect the kernel version string from the extracted module tree.
# e.g. "6.6.71-0-lts" — baked into /init insmod paths at image build time.
KVER=$(ls "$MODLOOP_DIR/modules/" 2>/dev/null | grep -- "-${ALPINE_FLAVOR}$" | head -1)
if [ -z "$KVER" ]; then
    echo "ERROR: could not detect kernel version from modloop (looked for *-${ALPINE_FLAVOR} in $MODLOOP_DIR/modules/)" >&2
    exit 1
fi
echo "  kernel version: $KVER"

if [ ! -f "$INITRAMFS_OUT" ] \
    || [ "$GUEST_BIN"   -nt "$INITRAMFS_OUT" ] \
    || [ "$PELAGOS_BIN" -nt "$INITRAMFS_OUT" ] \
    || [ "$0"           -nt "$INITRAMFS_OUT" ]; then

    NETMOD_BASE="$MODLOOP_DIR/modules/$KVER/kernel"
    VSOCK_SRC="$NETMOD_BASE/net/vmw_vsock"

    # --- Extract the Alpine initramfs and patch it in-place ---
    INITRD_TMP="$WORK/initramfs_tmp"
    rm -rf "$INITRD_TMP"
    mkdir -p "$INITRD_TMP"
    bsdtar -xpf "$INITRAMFS_DL" -C "$INITRD_TMP" 2>/dev/null || true

    # Create busybox applet symlinks in /bin.
    echo "  creating busybox applet symlinks"
    for applet in \
        [ awk basename cat chgrp chmod chown chroot clear cmp cp cut date dd \
        df diff dirname dmesg du echo env expr false find grep egrep fgrep \
        gunzip gzip head hostname id ifconfig install kill killall ln ls \
        md5sum mkdir mkfifo mke2fs mktemp more mount mv nc netstat nslookup od \
        paste ping ping6 pkill pgrep printenv printf ps pwd readlink \
        realpath renice reset rm rmdir route sed seq sha256sum sleep sort \
        split stat strings stty su sync tail tar tee test timeout top touch \
        tr true tty umount uname uniq uptime vi watch wc wget which xargs \
        yes zcat free blkid mknod ntpd
    do
        target="$INITRD_TMP/bin/$applet"
        [ -e "$target" ] || ln -sf busybox "$target"
    done

    # Add vsock modules
    mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/net/vmw_vsock"
    for ko in vsock.ko vmw_vsock_virtio_transport_common.ko vmw_vsock_virtio_transport.ko; do
        if [ -f "$VSOCK_SRC/$ko" ]; then
            cp "$VSOCK_SRC/$ko" "$INITRD_TMP/lib/modules/$KVER/kernel/net/vmw_vsock/$ko"
        else
            echo "  WARNING: $ko not found in modloop — vsock may not work" >&2
        fi
    done

    # Add virtio-net and virtio-rng modules.
    # virtio-net load order: failover → net_failover → virtio_net
    # virtio-rng load order: rng-core → virtio-rng
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
            echo "  WARNING: $(basename $src_path) not found in modloop" >&2
        fi
    done

    # virtio core modules: depended upon by vsock, virtio-net, virtio-console.
    # In linux-lts these are modules (built-in in linux-virt).  Stage from the
    # base Alpine initramfs (which already includes them); also copy from the
    # modloop to ensure we always have the version that matches this kernel.
    for ko in virtio_ring.ko virtio.ko; do
        src="$NETMOD_BASE/drivers/virtio/$ko"
        if [ -f "$src" ]; then
            mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/virtio"
            cp "$src" "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/virtio/$ko"
        fi
    done
    # virtio_console.ko provides /dev/hvc0 as a char device.
    VC_KO="$NETMOD_BASE/drivers/char/virtio_console.ko"
    if [ -f "$VC_KO" ]; then
        mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/char"
        cp "$VC_KO" "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/char/virtio_console.ko"
        echo "  staged virtio_console.ko"
    fi

    # overlayfs: add overlay.ko if present as a module.
    OVERLAY_KO="$NETMOD_BASE/fs/overlayfs/overlay.ko"
    if [ -f "$OVERLAY_KO" ]; then
        mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/fs/overlayfs"
        cp "$OVERLAY_KO" "$INITRD_TMP/lib/modules/$KVER/kernel/fs/overlayfs/overlay.ko"
        echo "  staged overlay.ko (module)"
    else
        echo "  overlay.ko not in modloop — assuming CONFIG_OVERLAY_FS=y (built-in)"
    fi

    mkdir -p "$INITRD_TMP/proc" "$INITRD_TMP/sys" "$INITRD_TMP/dev"

    # Add guest daemon and pelagos runtime.
    mkdir -p "$INITRD_TMP/usr/local/bin"
    cp "$GUEST_BIN" "$INITRD_TMP/usr/local/bin/pelagos-guest"
    chmod 755 "$INITRD_TMP/usr/local/bin/pelagos-guest"
    cp "$PELAGOS_BIN" "$INITRD_TMP/usr/local/bin/pelagos"
    chmod 755 "$INITRD_TMP/usr/local/bin/pelagos"

    # Add dropbear SSH server and its runtime library dependencies.
    mkdir -p "$INITRD_TMP/usr/sbin"
    cp "$DROPBEAR_BIN" "$INITRD_TMP/usr/sbin/dropbear"
    chmod 755 "$INITRD_TMP/usr/sbin/dropbear"
    cp "$LIBUTMPS"   "$INITRD_TMP/lib/libutmps.so.0.1"
    cp "$LIBSKARNET" "$INITRD_TMP/lib/libskarnet.so.2.14"
    cp "$LIBZ"       "$INITRD_TMP/lib/libz.so.1"

    # Add pasta — userspace networking for `pelagos build` RUN steps.
    mkdir -p "$INITRD_TMP/usr/bin"
    cp "$PASTA_BIN" "$INITRD_TMP/usr/bin/pasta"
    chmod 755 "$INITRD_TMP/usr/bin/pasta"

    # Stage the host's public key as the VM's authorized_keys.
    mkdir -p "$INITRD_TMP/root/.ssh"
    cp "${SSH_KEY_FILE}.pub" "$INITRD_TMP/root/.ssh/authorized_keys"
    chmod 700 "$INITRD_TMP/root/.ssh"
    chmod 600 "$INITRD_TMP/root/.ssh/authorized_keys"

    # udhcpc default script so DHCP can configure the interface and default route.
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

    # Mozilla CA bundle for TLS inside the VM.
    mkdir -p "$INITRD_TMP/etc/ssl/certs"
    cp "$CA_BUNDLE" "$INITRD_TMP/etc/ssl/certs/ca-certificates.crt"

    # Replace /init.
    # $KVER is expanded here (build-time variable); \$ inside the heredoc are
    # runtime shell variables that must NOT be expanded at build time.
    cat > "$INITRD_TMP/init" <<INIT_EOF
#!/bin/sh

# Bootstrap /dev FIRST.  On linux-lts, virtio_console is a module so the
# kernel cannot write to hvc0 before init runs.  We must ensure /dev/null
# exists before any 2>/dev/null redirection, otherwise the redirect fails
# and prevents the redirected command from executing at all.
#
# Strategy (belt+suspenders):
#   1. mknod /dev/null without any redirect (always works on Linux with root)
#   2. Try devtmpfs — if supported, overlays /dev with a full set of devices
# IMPORTANT: do NOT use 2>/dev/null before step 1 succeeds.
busybox mkdir -p /dev
busybox mknod /dev/null    c 1 3
busybox mknod /dev/console c 5 1
busybox mknod /dev/zero    c 1 5
busybox mount -t devtmpfs devtmpfs /dev || true

# Mount /proc — needed for the rootfs detection check below.
busybox mount -t proc proc /proc || true

# Pass 1: if we are still on the initramfs rootfs, load kernel modules and
# switch_root to a tmpfs so that pivot_root(2) works for container spawns.
if busybox grep -q '^rootfs / rootfs' /proc/mounts 2>/dev/null; then
    echo "[pelagos-init] pass 1: loading modules"

    # virtio core + PCI transport.
    # In linux-lts all of these are modules (built-in in linux-virt).
    # virtio_pci MUST be loaded before any device driver — AVF presents
    # virtio devices via PCIe; without virtio_pci, no device is probed.
    busybox insmod /lib/modules/$KVER/kernel/drivers/virtio/virtio_ring.ko
    busybox insmod /lib/modules/$KVER/kernel/drivers/virtio/virtio.ko
    busybox insmod /lib/modules/$KVER/kernel/drivers/virtio/virtio_pci_legacy_dev.ko || true
    busybox insmod /lib/modules/$KVER/kernel/drivers/virtio/virtio_pci_modern_dev.ko || true
    busybox insmod /lib/modules/$KVER/kernel/drivers/virtio/virtio_pci.ko

    # virtio-console: provides /dev/hvc0 as a character device.
    # Must be loaded after virtio_pci so the device can be probed.
    busybox insmod /lib/modules/$KVER/kernel/drivers/char/virtio_console.ko

    # virtio-rng: seed the CSPRNG early so TLS works after switch_root.
    busybox insmod /lib/modules/$KVER/kernel/drivers/char/hw_random/rng-core.ko || true
    busybox insmod /lib/modules/$KVER/kernel/drivers/char/hw_random/virtio-rng.ko || true

    # vsock: host↔guest communication channel.
    busybox insmod /lib/modules/$KVER/kernel/net/vmw_vsock/vsock.ko
    busybox insmod /lib/modules/$KVER/kernel/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko
    busybox insmod /lib/modules/$KVER/kernel/net/vmw_vsock/vmw_vsock_virtio_transport.ko

    # overlay: required by pelagos for container rootfs overlay mounts.
    busybox insmod /lib/modules/$KVER/kernel/fs/overlayfs/overlay.ko || true

    # virtio-net: load order: failover → net_failover → virtio_net.
    busybox insmod /lib/modules/$KVER/kernel/net/core/failover.ko || true
    busybox insmod /lib/modules/$KVER/kernel/drivers/net/net_failover.ko || true
    busybox insmod /lib/modules/$KVER/kernel/drivers/net/virtio_net.ko || true

    echo "[pelagos-init] pass 1: modules loaded"

    busybox mkdir -p /newroot
    busybox mount -t tmpfs -o size=512m tmpfs /newroot
    for d in bin sbin usr lib etc root mnt var; do
        [ -d "/\$d" ] && busybox cp -a "/\$d" /newroot/ 2>/dev/null || true
    done
    busybox cp /init /newroot/init
    busybox mkdir -p /newroot/proc /newroot/sys /newroot/dev /newroot/dev/pts \
                     /newroot/tmp /newroot/run /newroot/run/pelagos \
                     /newroot/sys/fs/cgroup /newroot/newroot

    exec busybox switch_root /newroot /init

    echo "[pelagos-init] FATAL: switch_root failed" >/dev/console 2>&1
    exec busybox sh
fi

# Pass 2: root is tmpfs. Kernel modules already loaded.
# Mount devtmpfs WITHOUT 2>/dev/null — /dev is empty here, so the redirect
# would fail and skip the mount entirely.
busybox mkdir -p /dev
busybox mount -t devtmpfs devtmpfs /dev || true
busybox mkdir -p /dev/pts
busybox mount -t devpts   devpts   /dev/pts 2>/dev/null || true
busybox mount -t sysfs    sysfs    /sys 2>/dev/null || true
busybox mkdir -p /sys/fs/cgroup
busybox mount -t cgroup2  cgroup2  /sys/fs/cgroup 2>/dev/null || true

busybox ip link set lo up
busybox ip link set eth0 up
if busybox udhcpc -i eth0 -s /usr/share/udhcpc/default.script -q -t 5 -T 3 >/dev/null 2>&1; then
    echo "[pelagos-init] network: DHCP OK"
else
    echo "[pelagos-init] network: DHCP failed, using static 192.168.105.2/24"
    busybox ip addr add 192.168.105.2/24 dev eth0
    busybox ip route add default via 192.168.105.1
fi
echo "[pelagos-init] network ready"
busybox mkdir -p /etc
echo 'nameserver 8.8.8.8' > /etc/resolv.conf
echo 'nameserver 8.8.4.4' >> /etc/resolv.conf

busybox mkdir -p /tmp /run /run/pelagos
busybox mount -t tmpfs tmpfs /tmp

# Gate on network readiness before pelagos-guest starts pulling images.
i=0
while [ \$i -lt 15 ]; do
    busybox ping -c 1 -W 3 -q 8.8.8.8 >/dev/null 2>&1 && break
    i=\$((i+1))
done

# Sync clock via NTP.  The VM starts at epoch; TLS cert validation will fail
# until the clock is correct.  Run ntpd in one-shot (-q) mode; timeout 10s.
busybox ntpd -n -q -p pool.ntp.org >/dev/null 2>&1 || true
echo "[pelagos-init] clock: \$(busybox date -u)"

# Mount virtiofs shares from the kernel cmdline (virtiofs.tags=tag0,tag1,...).
CMDLINE=\$(busybox cat /proc/cmdline)
PELAGOS_VOLUMES_PRESENT=0
for kv in \$CMDLINE; do
    case "\$kv" in
        virtiofs.tags=*)
            TAGS="\${kv#virtiofs.tags=}"
            OLD_IFS="\$IFS"
            IFS=","
            for TAG in \$TAGS; do
                IFS="\$OLD_IFS"
                if [ "\$TAG" = "pelagos-volumes" ]; then
                    PELAGOS_VOLUMES_PRESENT=1
                else
                    busybox mkdir -p "/mnt/\$TAG"
                    busybox mount -t virtiofs "\$TAG" "/mnt/\$TAG" && \
                        echo "[pelagos-init] mounted virtiofs tag \$TAG at /mnt/\$TAG" || \
                        echo "[pelagos-init] WARNING: failed to mount virtiofs tag \$TAG" >&2
                fi
                IFS=","
            done
            IFS="\$OLD_IFS"
            ;;
    esac
done

busybox mkdir -p /var/lib/pelagos
if busybox blkid /dev/vda 2>/dev/null | busybox grep -q ext2; then
    busybox mount -t ext2 /dev/vda /var/lib/pelagos 2>/dev/null || true
else
    echo "[pelagos-init] formatting /dev/vda as ext2 for image cache..."
    mke2fs -F /dev/vda 2>/dev/null && \
        busybox mount -t ext2 /dev/vda /var/lib/pelagos 2>/dev/null || true
fi

if [ "\$PELAGOS_VOLUMES_PRESENT" = "1" ]; then
    busybox mkdir -p /var/lib/pelagos/volumes
    busybox mount -t virtiofs pelagos-volumes /var/lib/pelagos/volumes && \
        echo "[pelagos-init] mounted pelagos-volumes virtiofs at /var/lib/pelagos/volumes" || \
        echo "[pelagos-init] WARNING: failed to mount pelagos-volumes virtiofs" >&2
fi

export PELAGOS_IMAGE_STORE=/var/lib/pelagos

busybox chown -R 0:0 /root 2>/dev/null || true
mkdir -p /etc/dropbear
dropbear -s -R -p 22 2>/dev/null || true

(while true; do /bin/sh </dev/hvc0 >/dev/hvc0 2>/dev/hvc0; sleep 1; done) &

exec /usr/local/bin/pelagos-guest
INIT_EOF
    chmod 755 "$INITRD_TMP/init"

    (cd "$INITRD_TMP" && bsdtar --format=newc -cf - .) | gzip -9 > "$INITRAMFS_OUT"
    echo "  initramfs: $INITRAMFS_OUT"

    # Record the flavor so future runs can detect if it changes.
    echo "$ALPINE_FLAVOR" > "$FLAVOR_STAMP"
else
    echo "  (cached: $INITRAMFS_OUT)"
    # Ensure stamp is present even on cache-hit rebuilds.
    echo "$ALPINE_FLAVOR" > "$FLAVOR_STAMP"
fi

# ---------------------------------------------------------------------------
echo "[8/8] Creating placeholder disk image"
# ---------------------------------------------------------------------------
if [ ! -f "$DISK_IMG" ]; then
    dd if=/dev/zero of="$DISK_IMG" bs=1m count=0 seek=512 2>/dev/null
    echo "  disk: $DISK_IMG (512 MiB sparse, formatted on first boot)"
else
    echo "  (cached: $DISK_IMG)"
fi

# ---------------------------------------------------------------------------
echo ""
echo "Done. VM image artifacts:"
echo "  kernel:    $KERNEL_OUT  (linux-${ALPINE_FLAVOR} $KVER)"
echo "  initramfs: $INITRAMFS_OUT"
echo "  disk:      $DISK_IMG"
echo ""
echo "Next: make build && make sign && make test-e2e"
echo "(kernel cmdline: console=hvc0  — no root=, initramfs is root, /init is pelagos)"
