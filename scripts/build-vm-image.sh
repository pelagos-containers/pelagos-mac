#!/usr/bin/env bash
# build-vm-image.sh — Build a minimal Alpine Linux ARM64 initramfs image for pelagos-mac.
#
# Strategy: appended cpio initramfs — no QEMU, no interactive install.
#
#   1. Download Alpine LTS netboot artifacts for aarch64 (3.21):
#      vmlinuz-lts, initramfs-lts, modloop-lts (no ISO extraction needed).
#   2. Decompress vmlinuz-lts to a raw arm64 Image (handles zboot/gzip formats).
#   3. Build pelagos-guest if the binary is missing.
#   4. Extract vsock/virtio modules from the modloop squashfs.
#   5. Overlay our custom init + binaries on top of Alpine's initramfs.
#   6. Repack as a single gzip'd cpio archive.
#   7. Create an 8192 MiB placeholder raw disk image (AVF requires a block device).
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
#   out/root.img              — 8192 MiB placeholder disk
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

PELAGOS_VERSION="0.59.0"
PELAGOS_BIN="$WORK/pelagos-${PELAGOS_VERSION}-aarch64-linux"
PELAGOS_URL="https://github.com/skeptomai/pelagos/releases/download/v${PELAGOS_VERSION}/pelagos-aarch64-linux"
# If a local build exists (from /Users/cb/Projects/pelagos), use it instead of downloading.
PELAGOS_LOCAL_BUILD="/Users/cb/Projects/pelagos/target/aarch64-unknown-linux-musl/release/pelagos"

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

# e2fsprogs: mke2fs binary + companion libraries for formatting /dev/vda on first boot.
E2FSPROGS_PKG="e2fsprogs-1.47.1-r1"
E2FSPROGS_APK="$WORK/${E2FSPROGS_PKG}.apk"
E2FSPROGS_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${E2FSPROGS_PKG}.apk"
E2FSPROGS_BIN="$WORK/mke2fs-bin"
E2FSPROGS_LIBS_PKG="e2fsprogs-libs-1.47.1-r1"
E2FSPROGS_LIBS_APK="$WORK/${E2FSPROGS_LIBS_PKG}.apk"
E2FSPROGS_LIBS_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${E2FSPROGS_LIBS_PKG}.apk"
LIBCOM_ERR_PKG="libcom_err-1.47.1-r1"
LIBCOM_ERR_APK="$WORK/${LIBCOM_ERR_PKG}.apk"
LIBCOM_ERR_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${LIBCOM_ERR_PKG}.apk"

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

# ---------------------------------------------------------------------------
echo "[5/8] Downloading pelagos runtime binary (v${PELAGOS_VERSION})"
# ---------------------------------------------------------------------------
if [ -f "$PELAGOS_LOCAL_BUILD" ]; then
    cp "$PELAGOS_LOCAL_BUILD" "$PELAGOS_BIN"
    chmod 755 "$PELAGOS_BIN"
    echo "  Using local build: $PELAGOS_LOCAL_BUILD"
elif [ ! -f "$PELAGOS_BIN" ]; then
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
echo "[5e/8] Downloading e2fsprogs (mke2fs for formatting /dev/vda on first boot)"
# ---------------------------------------------------------------------------
if [ ! -f "$E2FSPROGS_BIN" ]; then
    [ ! -f "$E2FSPROGS_APK" ] && curl -L --progress-bar -o "$E2FSPROGS_APK" "$E2FSPROGS_URL"
    E2FS_EXTRACT="$WORK/e2fsprogs-extract"
    rm -rf "$E2FS_EXTRACT" && mkdir -p "$E2FS_EXTRACT"
    bsdtar -xf "$E2FSPROGS_APK" -C "$E2FS_EXTRACT" 2>/dev/null || true
    if [ -f "$E2FS_EXTRACT/sbin/mke2fs" ]; then
        cp "$E2FS_EXTRACT/sbin/mke2fs" "$E2FSPROGS_BIN"
        chmod 755 "$E2FSPROGS_BIN"
        echo "  Extracted mke2fs binary"
    else
        echo "ERROR: mke2fs not found in $E2FSPROGS_APK" >&2; exit 1
    fi
else
    echo "  (cached: mke2fs-bin)"
fi
if [ ! -f "$WORK/libext2fs.so.2.4" ]; then
    [ ! -f "$E2FSPROGS_LIBS_APK" ] && curl -L --progress-bar -o "$E2FSPROGS_LIBS_APK" "$E2FSPROGS_LIBS_URL"
    [ ! -f "$LIBCOM_ERR_APK" ] && curl -L --progress-bar -o "$LIBCOM_ERR_APK" "$LIBCOM_ERR_URL"
    E2FSLIBS_EXTRACT="$WORK/e2fsprogs-libs-extract"
    rm -rf "$E2FSLIBS_EXTRACT" && mkdir -p "$E2FSLIBS_EXTRACT"
    bsdtar -xf "$E2FSPROGS_LIBS_APK" -C "$E2FSLIBS_EXTRACT" 2>/dev/null || true
    bsdtar -xf "$LIBCOM_ERR_APK"    -C "$E2FSLIBS_EXTRACT" 2>/dev/null || true
    for lib in $(find "$E2FSLIBS_EXTRACT" -name "*.so.*" -not -name ".SIGN*" 2>/dev/null); do
        cp "$lib" "$WORK/$(basename "$lib")"
        echo "  Extracted $(basename "$lib")"
    done
else
    echo "  (cached: e2fsprogs libs)"
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

    # tun.ko: required by pasta (passt) to create TAP devices for pasta-mode
    # networking in pelagos build RUN steps and pasta-mode containers.
    TUN_KO="$NETMOD_BASE/drivers/net/tun.ko"
    if [ -f "$TUN_KO" ]; then
        mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/net"
        cp "$TUN_KO" "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/net/tun.ko"
        echo "  staged tun.ko"
    else
        echo "  WARNING: tun.ko not found in modloop" >&2
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

    # virtio_blk.ko: provides /dev/vda (the persistent OCI image cache disk).
    # Without this, /dev/vda does not exist and the ext4 mount fails — all OCI
    # layer data goes to the root tmpfs and fills it up during large builds.
    VBK_KO="$NETMOD_BASE/drivers/block/virtio_blk.ko"
    if [ -f "$VBK_KO" ]; then
        mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/block"
        cp "$VBK_KO" "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/block/virtio_blk.ko"
        echo "  staged virtio_blk.ko"
    else
        echo "  WARNING: virtio_blk.ko not found in modloop — /dev/vda will be unavailable" >&2
    fi

    # ext4 filesystem module (+ jbd2 journal + mbcache deps): needed to mount /dev/vda.
    # ext4 is a module in linux-lts (not built-in).  ext4 replaces ext2 because its
    # journal makes the filesystem self-healing after unclean VM shutdowns — no more
    # "deleted inode referenced" corruption when the daemon is killed mid-write.
    for ko_rel in fs/mbcache.ko fs/jbd2/jbd2.ko fs/ext4/ext4.ko; do
        src="$MODLOOP_DIR/modules/$KVER/kernel/$ko_rel"
        dst="$INITRD_TMP/lib/modules/$KVER/kernel/$ko_rel"
        if [ -f "$src" ]; then
            mkdir -p "$(dirname "$dst")"
            cp "$src" "$dst"
        else
            echo "  WARNING: $ko_rel not found in modloop" >&2
        fi
    done
    echo "  staged ext4 + jbd2 + mbcache modules"

    # Replace the Alpine initramfs's modules.dep with the one from the modloop.
    # The Alpine initramfs modules.dep only covers its bundled subset; ours
    # covers all linux-lts modules so modprobe can resolve full dependency chains.
    for meta in modules.dep modules.dep.bin modules.alias modules.alias.bin \
                modules.builtin modules.builtin.bin modules.builtin.modinfo \
                modules.builtin.alias.bin modules.symbols.bin modules.devname; do
        src="$MODLOOP_DIR/modules/$KVER/$meta"
        [ -f "$src" ] && cp "$src" "$INITRD_TMP/lib/modules/$KVER/$meta"
    done
    echo "  updated modules.dep from modloop"

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

    # Add mke2fs + libs for formatting /dev/vda (persistent OCI image cache) on first boot.
    # busybox in Alpine's initramfs-lts does not include the mke2fs applet.
    if [ -f "$E2FSPROGS_BIN" ]; then
        mkdir -p "$INITRD_TMP/sbin" "$INITRD_TMP/usr/lib"
        cp "$E2FSPROGS_BIN" "$INITRD_TMP/sbin/mke2fs"
        chmod 755 "$INITRD_TMP/sbin/mke2fs"
        # Stage versioned .so files into /usr/lib and create soname symlinks.
        for sofile in "$WORK"/lib*.so.*; do
            [ -f "$sofile" ] || continue
            fname="$(basename "$sofile")"
            cp "$sofile" "$INITRD_TMP/usr/lib/$fname"
            # Create soname symlink (strip minor version): e.g. libfoo.so.2.3 → libfoo.so.2
            soname="$(echo "$fname" | sed 's/\(\.so\.[0-9]*\)\..*/\1/')"
            [ "$soname" != "$fname" ] && ln -sf "$fname" "$INITRD_TMP/usr/lib/$soname"
        done
        echo "  staged mke2fs + e2fsprogs libs"
    fi


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

# Alpine linux-lts has CONFIG_DEVTMPFS_MOUNT=y: the kernel auto-mounts
# devtmpfs at /dev before executing init, so /dev/null etc. always exist.
# The explicit mount below is a no-op on a running kernel but keeps the
# script self-contained if that config were ever absent.
busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null || true

# Mount /proc — needed for the rootfs detection check below.
busybox mount -t proc proc /proc 2>/dev/null || true

# Pass 1: if we are still on the initramfs rootfs, load kernel modules and
# switch_root to a tmpfs so that pivot_root(2) works for container spawns.
if busybox grep -q '^rootfs / rootfs' /proc/mounts 2>/dev/null; then
    echo "[pelagos-init] pass 1: loading modules"

    # modprobe reads modules.dep and resolves the full dependency chain
    # automatically — no manual ordering needed.  virtio_pci is listed
    # first because AVF presents virtio devices over PCIe; it must be
    # probed before any device driver (console, net, vsock) can attach.
    modprobe virtio_pci          2>/dev/null || true
    modprobe virtio_console      2>/dev/null || true
    modprobe virtio-rng          2>/dev/null || true
    modprobe vmw_vsock_virtio_transport 2>/dev/null || true
    modprobe overlay             2>/dev/null || true
    modprobe virtio_net          2>/dev/null || true
    modprobe virtio_blk          2>/dev/null || true
    modprobe tun                 2>/dev/null || true
    modprobe jbd2                2>/dev/null || true
    modprobe ext4                2>/dev/null || true
    # Create /dev/net/tun device node.  The tun kernel module registers
    # the device (char major 10, minor 200) but does not create the node
    # automatically without udevd/mdev.  pasta requires /dev/net/tun to
    # create TAP interfaces for pasta-mode container networking.
    busybox mkdir -p /dev/net
    busybox mknod /dev/net/tun c 10 200 2>/dev/null || true
    busybox chmod 0666 /dev/net/tun 2>/dev/null || true

    echo "[pelagos-init] pass 1: modules loaded"

    busybox mkdir -p /newroot
    busybox mount -t tmpfs -o size=2048m tmpfs /newroot
    for d in bin sbin usr lib etc root mnt var; do
        [ -d "/\$d" ] && busybox cp -a "/\$d" /newroot/ 2>/dev/null || true
    done
    busybox cp /init /newroot/init
    busybox mkdir -p /newroot/proc /newroot/sys /newroot/dev /newroot/dev/pts \
                     /newroot/dev/net \
                     /newroot/tmp /newroot/run /newroot/run/pelagos \
                     /newroot/sys/fs/cgroup /newroot/newroot
    busybox mknod /newroot/dev/net/tun c 10 200 2>/dev/null || true
    busybox chmod 0666 /newroot/dev/net/tun 2>/dev/null || true

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
# Ensure /dev/net/tun exists.  devtmpfs may or may not auto-create it from the
# already-loaded tun module; create it explicitly as a safe fallback.
# pasta (passt) requires /dev/net/tun to create TAP interfaces.
busybox mkdir -p /dev/net
busybox mknod /dev/net/tun c 10 200 2>/dev/null || true
busybox chmod 0666 /dev/net/tun 2>/dev/null || true
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

# Sync clock from the host UTC time embedded in the kernel cmdline by pelagos-mac.
# Format: clock.utc=YYYY-MM-DDTHH:MM:SS — busybox date -s accepts "YYYY-MM-DD HH:MM:SS".
_utc=\$(busybox cat /proc/cmdline | busybox tr ' ' '\n' | busybox grep '^clock\.utc=' | busybox head -1 | busybox cut -d= -f2)
if [ -n "\$_utc" ]; then
    _utc_space=\$(echo "\$_utc" | busybox tr 'T' ' ')
    busybox date -s "\$_utc_space" >/dev/null 2>&1 && \
        echo "[pelagos-init] clock set from host: \$(busybox date -u)" || \
        echo "[pelagos-init] WARNING: date -s failed (utc=\$_utc)" >&2
else
    echo "[pelagos-init] WARNING: clock.utc not in cmdline, clock may be wrong" >&2
fi

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
if busybox test -b /dev/vda; then
    if busybox blkid /dev/vda 2>/dev/null | busybox grep -q 'TYPE="ext4"'; then
        busybox mount -t ext4 /dev/vda /var/lib/pelagos 2>/dev/null && \
            echo "[pelagos-init] mounted /dev/vda (ext4) at /var/lib/pelagos" || \
            echo "[pelagos-init] WARNING: ext4 mount of /dev/vda failed" >&2
    elif busybox test -x /sbin/mke2fs; then
        if busybox blkid /dev/vda 2>/dev/null | busybox grep -q 'TYPE="ext2"'; then
            echo "[pelagos-init] ext2 detected — reformatting as ext4 (OCI cache will be repopulated)..."
        else
            echo "[pelagos-init] formatting /dev/vda as ext4 for OCI image cache..."
        fi
        /sbin/mke2fs -F -t ext4 /dev/vda 2>/dev/null && \
            busybox mount -t ext4 /dev/vda /var/lib/pelagos 2>/dev/null && \
            echo "[pelagos-init] formatted and mounted /dev/vda (ext4) at /var/lib/pelagos" || \
            echo "[pelagos-init] WARNING: ext4 format/mount failed — OCI cache in RAM" >&2
    else
        echo "[pelagos-init] WARNING: mke2fs missing — OCI cache will be in RAM" >&2
    fi
else
    echo "[pelagos-init] WARNING: /dev/vda not found — OCI cache will be in RAM (tmpfs)" >&2
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

export RUST_LOG=warn
exec /usr/local/bin/pelagos-guest </dev/null >/tmp/guest.log 2>&1
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
    dd if=/dev/zero of="$DISK_IMG" bs=1m count=0 seek=8192 2>/dev/null
    echo "  disk: $DISK_IMG (8192 MiB sparse, formatted on first boot via VM init)"
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
