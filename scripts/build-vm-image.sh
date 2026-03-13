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
# Kernel cmdline to use:  console=hvc0
# (the kernel's default rdinit=/init picks up our /init from the initramfs)

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

PELAGOS_VERSION="0.27.1"
PELAGOS_BIN="$WORK/pelagos-aarch64-linux"
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
# Helper to extract a single .so from an APK (gzip'd tar).
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
    # APK is a gzip'd tar; extract usr/sbin/dropbear from it.
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

# dropbear runtime deps: libutmps (from utmps-libs), libskarnet (from skalibs-libs), libz (from zlib).
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
# pasta is the userspace-networking binary from the passt package (Alpine community).
# It enables network access in `pelagos build` RUN steps without bridge/veth modules.
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
if [ ! -f "$INITRAMFS_OUT" ] \
    || [ "$GUEST_BIN"   -nt "$INITRAMFS_OUT" ] \
    || [ "$PELAGOS_BIN" -nt "$INITRAMFS_OUT" ] \
    || [ "$0"           -nt "$INITRAMFS_OUT" ]; then
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

    # Create busybox applet symlinks in /bin.
    # Alpine's virt initramfs only ships /bin/sh → busybox; all other applets
    # must be symlinked explicitly.  busybox --install is not compiled in.
    # Only create a symlink if one does not already exist (preserves real binaries).
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
        yes zcat free blkid
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

    # Add dropbear SSH server and its runtime library dependencies.
    mkdir -p "$INITRD_TMP/usr/sbin"
    cp "$DROPBEAR_BIN" "$INITRD_TMP/usr/sbin/dropbear"
    chmod 755 "$INITRD_TMP/usr/sbin/dropbear"
    cp "$LIBUTMPS"   "$INITRD_TMP/lib/libutmps.so.0.1"
    cp "$LIBSKARNET" "$INITRD_TMP/lib/libskarnet.so.2.14"
    cp "$LIBZ"       "$INITRD_TMP/lib/libz.so.1"

    # Add pasta — userspace networking for `pelagos build` RUN steps.
    # pasta proxies TCP/UDP through the host (VM) network without needing
    # bridge/veth kernel modules.
    mkdir -p "$INITRD_TMP/usr/bin"
    cp "$PASTA_BIN" "$INITRD_TMP/usr/bin/pasta"
    chmod 755 "$INITRD_TMP/usr/bin/pasta"

    # Stage the host's public key as the VM's authorized_keys so 'pelagos vm ssh'
    # can log in without a password.  The corresponding private key is at
    # ~/.local/share/pelagos/vm_key on the host.
    mkdir -p "$INITRD_TMP/root/.ssh"
    cp "${SSH_KEY_FILE}.pub" "$INITRD_TMP/root/.ssh/authorized_keys"
    chmod 700 "$INITRD_TMP/root/.ssh"
    chmod 600 "$INITRD_TMP/root/.ssh/authorized_keys"

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
busybox mkdir -p /dev/pts
busybox mount -t devpts   devpts   /dev/pts 2>/dev/null || true
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

# Configure networking via DHCP (socket_vmnet provides a DHCP server through
# vmnet.framework shared mode).
#
# NOTE: udhcpc requires AF_PACKET (CONFIG_PACKET) for initial DHCP discovery.
# If this kernel lacks CONFIG_PACKET, udhcpc fails and we fall back to a static
# IP on vmnet's default shared subnet (192.168.64.0/24, gateway 192.168.64.1).
busybox ip link set lo up
busybox ip link set eth0 up
if busybox udhcpc -i eth0 -s /usr/share/udhcpc/default.script -q -t 5 -T 3 >/dev/null 2>&1; then
    echo "[pelagos-init] network: DHCP OK"
else
    echo "[pelagos-init] network: DHCP failed (CONFIG_PACKET=n?), using static 192.168.105.2/24"
    busybox ip addr add 192.168.105.2/24 dev eth0
    busybox ip route add default via 192.168.105.1
fi
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

# Mount virtiofs shares requested by the host.
# The host appends "virtiofs.tags=tag0,tag1,..." to the kernel cmdline.
# User shares (share0, share1, ...) are mounted at /mnt/<tag>.
# The built-in "pelagos-volumes" share is mounted at /var/lib/pelagos/volumes
# AFTER the ext2 disk is mounted below — track it with a flag.
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

# Mount the virtio block device (/dev/vda) as the persistent OCI image cache.
# On first boot the disk is blank; format it as ext2 then mount.
# On subsequent boots mount directly (format check via blkid).
busybox mkdir -p /var/lib/pelagos
if busybox blkid /dev/vda 2>/dev/null | busybox grep -q ext2; then
    busybox mount -t ext2 /dev/vda /var/lib/pelagos 2>/dev/null || true
else
    echo "[pelagos-init] formatting /dev/vda as ext2 for image cache..."
    mke2fs -F /dev/vda 2>/dev/null && \
        busybox mount -t ext2 /dev/vda /var/lib/pelagos 2>/dev/null || true
fi

# Mount the always-on pelagos-volumes virtiofs share at /var/lib/pelagos/volumes.
# This makes named volumes (e.g. for VS Code server, persistent data) survive VM restarts
# by backing them on the macOS host at ~/.local/share/pelagos/volumes/.
if [ "\$PELAGOS_VOLUMES_PRESENT" = "1" ]; then
    busybox mkdir -p /var/lib/pelagos/volumes
    busybox mount -t virtiofs pelagos-volumes /var/lib/pelagos/volumes && \
        echo "[pelagos-init] mounted pelagos-volumes virtiofs at /var/lib/pelagos/volumes" || \
        echo "[pelagos-init] WARNING: failed to mount pelagos-volumes virtiofs" >&2
fi

export PELAGOS_IMAGE_STORE=/var/lib/pelagos

# Start dropbear SSH server for 'pelagos vm ssh'.
# Fix ownership: files were staged by the macOS build user (UID != 0).
# Dropbear enforces that authorized_keys is owned by the connecting user.
busybox chown -R 0:0 /root 2>/dev/null || true
# -s: disable password auth (key-only)
# -R: generate host keys on demand (ephemeral; new keys each boot — acceptable
#     because the client uses StrictHostKeyChecking=no)
# -p 22: listen on port 22
mkdir -p /etc/dropbear
dropbear -s -R -p 22 2>/dev/null || true

# Start a root shell on hvc0 for 'pelagos vm console' access.
# Opens /dev/hvc0 as a bidirectional fd and execs /bin/sh with all I/O wired
# to it.  Loops so that reconnecting after 'Ctrl-]' detach spawns a fresh shell.
(while true; do /bin/sh </dev/hvc0 >/dev/hvc0 2>/dev/hvc0; sleep 1; done) &

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
    # 512 MiB sparse file — formatted as ext2 by the VM on first boot and
    # mounted at /var/lib/pelagos for the persistent OCI image cache.
    # Using a sparse file keeps the on-disk footprint near zero until data is written.
    dd if=/dev/zero of="$DISK_IMG" bs=1m count=0 seek=512 2>/dev/null
    echo "  disk: $DISK_IMG (512 MiB sparse, formatted on first boot)"
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
