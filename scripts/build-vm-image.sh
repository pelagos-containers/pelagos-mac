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

# Ubuntu 6.8 HWE kernel artifacts produced by build-build-image.sh.
# When present, these replace the Alpine lts kernel and its modules.
# CONFIG_KVM_GUEST=y in the Ubuntu kernel eliminates RCU stalls under AVF.
# On first-time setup, run build-build-image.sh after this script to produce them.
UBUNTU_VMLINUZ="$OUT/ubuntu-vmlinuz"
UBUNTU_MODULES="$OUT/ubuntu-modules"

PELAGOS_VERSION="0.59.4"
PELAGOS_BIN="$WORK/pelagos-${PELAGOS_VERSION}-aarch64-linux"
PELAGOS_URL="https://github.com/pelagos-containers/pelagos/releases/download/v${PELAGOS_VERSION}/pelagos-aarch64-linux"
# If a local build exists, use it instead of downloading.
PELAGOS_LOCAL_BUILD="$HOME/Projects/pelagos/target/aarch64-unknown-linux-musl/release/pelagos"

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

# iproute2-minimal: provides /sbin/ip — required by pelagos bridge networking (-p flag).
IPROUTE2_MIN_PKG="iproute2-minimal-6.11.0-r0"
IPROUTE2_MIN_APK="$WORK/${IPROUTE2_MIN_PKG}.apk"
IPROUTE2_MIN_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${IPROUTE2_MIN_PKG}.apk"
IP_BIN="$WORK/ip-bin"

# nftables: provides /usr/sbin/nft — required for DNAT port-forward rules.
NFTABLES_PKG="nftables-1.1.1-r0"
NFTABLES_APK="$WORK/${NFTABLES_PKG}.apk"
NFTABLES_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${NFTABLES_PKG}.apk"
NFT_BIN="$WORK/nft-bin"

# Shared library dependencies for ip and nft.
# All paths are usr/lib/ inside the APK (Alpine 3.21+ moved libs there).
LIBCAP2_PKG="libcap2-2.71-r0"
LIBCAP2_APK="$WORK/${LIBCAP2_PKG}.apk"
LIBCAP2_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${LIBCAP2_PKG}.apk"

LIBELF_PKG="libelf-0.191-r0"
LIBELF_APK="$WORK/${LIBELF_PKG}.apk"
LIBELF_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${LIBELF_PKG}.apk"

LIBMNL_PKG="libmnl-1.0.5-r2"
LIBMNL_APK="$WORK/${LIBMNL_PKG}.apk"
LIBMNL_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${LIBMNL_PKG}.apk"

LIBNFTNL_PKG="libnftnl-1.2.8-r0"
LIBNFTNL_APK="$WORK/${LIBNFTNL_PKG}.apk"
LIBNFTNL_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${LIBNFTNL_PKG}.apk"

LIBGMP_PKG="gmp-6.3.0-r2"
LIBGMP_APK="$WORK/${LIBGMP_PKG}.apk"
LIBGMP_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${LIBGMP_PKG}.apk"

LIBJANSSON_PKG="jansson-2.14-r4"
LIBJANSSON_APK="$WORK/${LIBJANSSON_PKG}.apk"
LIBJANSSON_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${LIBJANSSON_PKG}.apk"

LIBREADLINE_PKG="readline-8.2.13-r0"
LIBREADLINE_APK="$WORK/${LIBREADLINE_PKG}.apk"
LIBREADLINE_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${LIBREADLINE_PKG}.apk"

LIBNCURSESW_PKG="libncursesw-6.5_p20241006-r3"
LIBNCURSESW_APK="$WORK/${LIBNCURSESW_PKG}.apk"
LIBNCURSESW_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}/${LIBNCURSESW_PKG}.apk"

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
    # Prefer the Ubuntu 6.8 HWE kernel (CONFIG_KVM_GUEST=y, no RCU stalls under AVF)
    # when build-build-image.sh has already produced it.  Fall back to the Alpine lts
    # kernel on first-time setup (before the Ubuntu build VM exists).
    if [ -f "$UBUNTU_VMLINUZ" ]; then
        cp "$UBUNTU_VMLINUZ" "$KERNEL_OUT"
        echo "  kernel: using Ubuntu 6.8 HWE ($UBUNTU_VMLINUZ)"
        echo "  (CONFIG_KVM_GUEST=y — no RCU stalls under AVF)"
    else
        echo "  Ubuntu kernel not yet available — using Alpine lts (run build-build-image.sh to upgrade)"
    fi
fi

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
echo "[5f/8] Downloading iproute2-minimal + nftables (for pelagos bridge networking)"
# ---------------------------------------------------------------------------
extract_bin() {
    # Usage: extract_bin <apk> <src-path-in-apk> <dest>
    local apk="$1" src="$2" dest="$3"
    local tmpdir
    tmpdir=$(mktemp -d)
    bsdtar -xf "$apk" -C "$tmpdir" 2>/dev/null || true
    if [ -f "$tmpdir/$src" ]; then
        cp "$tmpdir/$src" "$dest"
        chmod 755 "$dest"
        rm -rf "$tmpdir"
        return 0
    fi
    rm -rf "$tmpdir"
    return 1
}

if [ ! -f "$IP_BIN" ]; then
    [ ! -f "$IPROUTE2_MIN_APK" ] && curl -L --progress-bar -o "$IPROUTE2_MIN_APK" "$IPROUTE2_MIN_URL"
    extract_bin "$IPROUTE2_MIN_APK" "sbin/ip" "$IP_BIN" || \
        { echo "ERROR: ip not found in $IPROUTE2_MIN_APK" >&2; exit 1; }
    echo "  Extracted ip binary"
else
    echo "  (cached: ip-bin)"
fi

if [ ! -f "$NFT_BIN" ]; then
    [ ! -f "$NFTABLES_APK" ] && curl -L --progress-bar -o "$NFTABLES_APK" "$NFTABLES_URL"
    extract_bin "$NFTABLES_APK" "usr/sbin/nft" "$NFT_BIN" || \
        { echo "ERROR: nft not found in $NFTABLES_APK" >&2; exit 1; }
    echo "  Extracted nft binary"
else
    echo "  (cached: nft-bin)"
fi

# Shared libs needed by ip and nft.
# ip  needs: libcap.so.2, libelf.so.1, libmnl.so.0
# nft needs: libmnl.so.0, libnftnl.so.11, libnftables.so.1, libgmp.so.10,
#             libjansson.so.4, libreadline.so.8, libncursesw.so.6
_fetch_so() {
    local apk_file="$1" apk_url="$2" soname="$3"
    local dest="$WORK/$soname"
    if [ ! -f "$dest" ]; then
        [ ! -f "$apk_file" ] && curl -L --progress-bar -o "$apk_file" "$apk_url"
        extract_so "$apk_file" "$soname" "$dest" || \
            { echo "ERROR: $soname not found in $apk_file" >&2; exit 1; }
        echo "  Extracted $soname"
    else
        echo "  (cached: $soname)"
    fi
}
_fetch_so "$LIBCAP2_APK"    "$LIBCAP2_URL"    "libcap.so.2"
_fetch_so "$LIBELF_APK"     "$LIBELF_URL"     "libelf.so.1"
_fetch_so "$LIBMNL_APK"     "$LIBMNL_URL"     "libmnl.so.0"
_fetch_so "$LIBNFTNL_APK"   "$LIBNFTNL_URL"   "libnftnl.so.11"
_fetch_so "$LIBGMP_APK"     "$LIBGMP_URL"     "libgmp.so.10"
_fetch_so "$LIBJANSSON_APK" "$LIBJANSSON_URL" "libjansson.so.4"
_fetch_so "$LIBREADLINE_APK" "$LIBREADLINE_URL" "libreadline.so.8"
_fetch_so "$LIBNCURSESW_APK" "$LIBNCURSESW_URL" "libncursesw.so.6"
_fetch_so "$NFTABLES_APK"   "$NFTABLES_URL"   "libnftables.so.1"


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

# Detect which kernel version string to embed in the initramfs module tree.
# If the Ubuntu 6.8 modules are available (produced by build-build-image.sh),
# use their version and source vsock/overlay/bridge/nftables from there; all
# virtio drivers are =y (built-in) in Ubuntu 6.8 HWE and need no modules.
# On first-time setup (before build-build-image.sh), fall back to Alpine lts.
USE_UBUNTU_MODULES=0
if [[ -f "$UBUNTU_VMLINUZ" && -d "$UBUNTU_MODULES" ]]; then
    # Kernel version is stored in a kver.txt written by build-build-image.sh
    # alongside the extracted modules (modules.dep uses relative paths, not
    # absolute, so we cannot parse the version from it directly).
    UBUNTU_KVER=""
    if [[ -f "$UBUNTU_MODULES/kver.txt" ]]; then
        UBUNTU_KVER=$(cat "$UBUNTU_MODULES/kver.txt" | tr -d '[:space:]')
    fi
    if [[ -n "$UBUNTU_KVER" ]]; then
        KVER="$UBUNTU_KVER"
        USE_UBUNTU_MODULES=1
        echo "  kernel version: $KVER (Ubuntu 6.8 HWE — CONFIG_KVM_GUEST=y)"
    else
        echo "  WARNING: $UBUNTU_MODULES/kver.txt missing or empty — falling back to Alpine" >&2
    fi
fi

if [[ "$USE_UBUNTU_MODULES" -eq 0 ]]; then
    KVER=$(ls "$MODLOOP_DIR/modules/" 2>/dev/null | grep -- "-${ALPINE_FLAVOR}$" | head -1)
    if [ -z "$KVER" ]; then
        echo "ERROR: could not detect kernel version from modloop (looked for *-${ALPINE_FLAVOR} in $MODLOOP_DIR/modules/)" >&2
        exit 1
    fi
    echo "  kernel version: $KVER (Alpine lts — run build-build-image.sh to upgrade to Ubuntu kernel)"
fi

PELAGOS_DNS_BIN="$HOME/Projects/pelagos/target/aarch64-unknown-linux-musl/release/pelagos-dns"

if [ ! -f "$INITRAMFS_OUT" ] \
    || [ "$GUEST_BIN"        -nt "$INITRAMFS_OUT" ] \
    || [ "$PELAGOS_BIN"      -nt "$INITRAMFS_OUT" ] \
    || { [ -f "$PELAGOS_DNS_BIN" ] && [ "$PELAGOS_DNS_BIN" -nt "$INITRAMFS_OUT" ]; } \
    || [ "$0"                -nt "$INITRAMFS_OUT" ]; then

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

    if [[ "$USE_UBUNTU_MODULES" -eq 1 ]]; then
        # Ubuntu 6.8 HWE: virtio_net, virtio_blk, virtio_pci, ext4, tun, virtio_console
        # are all CONFIG_xxx=y (built-in).  Modules that are =m and required:
        #   vsock        — pelagos-guest comms
        #   overlayfs    — container layer stacking
        #   virtiofs     — AVF host directory sharing (volumes, $HOME bind-mounts)
        #   bridge+deps  — pelagos bridge networking (NetworkMode::Bridge / -p)
        #   nftables     — port-forward DNAT rules
        # Stage all of the above from the Ubuntu module tree extracted by
        # build-build-image.sh; no Alpine modules needed.
        for rel_dir in \
            net/vmw_vsock \
            fs/overlayfs \
            fs/fuse \
            net/llc \
            net/802 \
            net/bridge \
            net/netfilter
        do
            mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/$rel_dir"
        done
        for ko in vsock.ko vmw_vsock_virtio_transport_common.ko vmw_vsock_virtio_transport.ko; do
            src="$UBUNTU_MODULES/net/vmw_vsock/$ko"
            if [ -f "$src" ]; then
                cp "$src" "$INITRD_TMP/lib/modules/$KVER/kernel/net/vmw_vsock/$ko"
                echo "  staged (Ubuntu) $ko"
            else
                echo "  WARNING: $ko not found in $UBUNTU_MODULES" >&2
            fi
        done
        if [ -f "$UBUNTU_MODULES/fs/overlayfs/overlay.ko" ]; then
            cp "$UBUNTU_MODULES/fs/overlayfs/overlay.ko" \
               "$INITRD_TMP/lib/modules/$KVER/kernel/fs/overlayfs/overlay.ko"
            echo "  staged (Ubuntu) overlay.ko"
        else
            echo "  WARNING: overlay.ko not found in $UBUNTU_MODULES" >&2
        fi
        # virtiofs, bridge, nftables modules (optional — warn but don't abort if missing,
        # since they are absent on old ubuntu-modules trees built before this fix)
        for rel_ko in \
            fs/fuse/virtiofs.ko \
            net/llc/llc.ko \
            net/802/stp.ko \
            net/bridge/bridge.ko \
            drivers/net/veth.ko \
            lib/libcrc32c.ko \
            net/ipv4/netfilter/nf_defrag_ipv4.ko \
            net/ipv6/netfilter/nf_defrag_ipv6.ko \
            net/netfilter/nfnetlink.ko \
            net/netfilter/nf_tables.ko \
            net/netfilter/nf_conntrack.ko \
            net/netfilter/nf_nat.ko \
            net/netfilter/nft_nat.ko \
            net/netfilter/nft_chain_nat.ko \
            net/netfilter/nft_masq.ko
        do
            src="$UBUNTU_MODULES/$rel_ko"
            dst="$INITRD_TMP/lib/modules/$KVER/kernel/$rel_ko"
            if [ -f "$src" ]; then
                mkdir -p "$(dirname "$dst")"
                cp "$src" "$dst"
                echo "  staged (Ubuntu) $(basename $rel_ko)"
            else
                echo "  INFO: $(basename $rel_ko) not in ubuntu-modules (rebuild with build-build-image.sh to enable virtiofs/bridge/nftables)" >&2
            fi
        done
        # Use the Ubuntu modules.dep so modprobe resolves all dependency chains.
        cp "$UBUNTU_MODULES/modules.dep"     "$INITRD_TMP/lib/modules/$KVER/modules.dep"     2>/dev/null || true
        cp "$UBUNTU_MODULES/modules.dep.bin" "$INITRD_TMP/lib/modules/$KVER/modules.dep.bin" 2>/dev/null || true
        echo "  updated modules.dep from Ubuntu modules"
    else
        # Alpine lts fallback: all virtio drivers are modules, so we must stage them.
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

        # virtio core modules.
        for ko in virtio_ring.ko virtio.ko; do
            src="$NETMOD_BASE/drivers/virtio/$ko"
            if [ -f "$src" ]; then
                mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/virtio"
                cp "$src" "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/virtio/$ko"
            fi
        done

        VC_KO="$NETMOD_BASE/drivers/char/virtio_console.ko"
        if [ -f "$VC_KO" ]; then
            mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/char"
            cp "$VC_KO" "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/char/virtio_console.ko"
            echo "  staged virtio_console.ko"
        fi

        TUN_KO="$NETMOD_BASE/drivers/net/tun.ko"
        if [ -f "$TUN_KO" ]; then
            mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/net"
            cp "$TUN_KO" "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/net/tun.ko"
            echo "  staged tun.ko"
        else
            echo "  WARNING: tun.ko not found in modloop" >&2
        fi

        OVERLAY_KO="$NETMOD_BASE/fs/overlayfs/overlay.ko"
        if [ -f "$OVERLAY_KO" ]; then
            mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/fs/overlayfs"
            cp "$OVERLAY_KO" "$INITRD_TMP/lib/modules/$KVER/kernel/fs/overlayfs/overlay.ko"
            echo "  staged overlay.ko (module)"
        else
            echo "  overlay.ko not in modloop — assuming CONFIG_OVERLAY_FS=y (built-in)"
        fi

        VBK_KO="$NETMOD_BASE/drivers/block/virtio_blk.ko"
        if [ -f "$VBK_KO" ]; then
            mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/block"
            cp "$VBK_KO" "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/block/virtio_blk.ko"
            echo "  staged virtio_blk.ko"
        else
            echo "  WARNING: virtio_blk.ko not found in modloop — /dev/vda will be unavailable" >&2
        fi

        LOOP_KO="$NETMOD_BASE/drivers/block/loop.ko"
        if [ -f "$LOOP_KO" ]; then
            mkdir -p "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/block"
            cp "$LOOP_KO" "$INITRD_TMP/lib/modules/$KVER/kernel/drivers/block/loop.ko"
            echo "  staged loop.ko"
        else
            echo "  WARNING: loop.ko not found in modloop — losetup will be unavailable" >&2
        fi

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

        for meta in modules.dep modules.dep.bin modules.alias modules.alias.bin \
                    modules.builtin modules.builtin.bin modules.builtin.modinfo \
                    modules.builtin.alias.bin modules.symbols.bin modules.devname; do
            src="$MODLOOP_DIR/modules/$KVER/$meta"
            [ -f "$src" ] && cp "$src" "$INITRD_TMP/lib/modules/$KVER/$meta"
        done
        echo "  updated modules.dep from modloop"
    fi

    mkdir -p "$INITRD_TMP/proc" "$INITRD_TMP/sys" "$INITRD_TMP/dev"

    # Add guest daemon and pelagos runtime.
    mkdir -p "$INITRD_TMP/usr/local/bin"
    cp "$GUEST_BIN" "$INITRD_TMP/usr/local/bin/pelagos-guest"
    chmod 755 "$INITRD_TMP/usr/local/bin/pelagos-guest"
    cp "$PELAGOS_BIN" "$INITRD_TMP/usr/local/bin/pelagos"
    chmod 755 "$INITRD_TMP/usr/local/bin/pelagos"
    # pelagos-dns: container name resolution daemon (co-located with pelagos binary).
    PELAGOS_DNS_BIN="$HOME/Projects/pelagos/target/aarch64-unknown-linux-musl/release/pelagos-dns"
    if [ -f "$PELAGOS_DNS_BIN" ]; then
        cp "$PELAGOS_DNS_BIN" "$INITRD_TMP/usr/local/bin/pelagos-dns"
        chmod 755 "$INITRD_TMP/usr/local/bin/pelagos-dns"
        echo "  Staged pelagos-dns"
    else
        echo "  WARNING: pelagos-dns binary not found at $PELAGOS_DNS_BIN — container hostname DNS will not work" >&2
    fi

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


    # Add ip (iproute2-minimal) + nft (nftables) + their shared libs.
    # Required for pelagos bridge networking and port-forward DNAT rules.
    mkdir -p "$INITRD_TMP/sbin" "$INITRD_TMP/usr/sbin" "$INITRD_TMP/usr/lib"
    cp "$IP_BIN"  "$INITRD_TMP/sbin/ip"
    cp "$NFT_BIN" "$INITRD_TMP/usr/sbin/nft"
    chmod 755 "$INITRD_TMP/sbin/ip" "$INITRD_TMP/usr/sbin/nft"
    for soname in \
        libcap.so.2 libelf.so.1 libmnl.so.0 libnftnl.so.11 \
        libgmp.so.10 libjansson.so.4 libreadline.so.8 libncursesw.so.6 \
        libnftables.so.1; do
        src="$WORK/$soname"
        if [ -f "$src" ]; then
            cp "$src" "$INITRD_TMP/usr/lib/$soname"
            echo "  staged $soname"
        else
            echo "  WARNING: $soname not found in $WORK — ip/nft may not work" >&2
        fi
    done

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

    # Compute a version marker from the pelagos-guest binary, the pelagos
    # runtime binary, AND this build script.  Embedded as a literal string in
    # the init script at build time; pass 1 compares it against
    # /etc/pelagos-root-version on /dev/vda to decide whether to refresh the
    # disk root.  Changes to any of these three inputs trigger an auto-refresh
    # on next boot, with the OCI cache preserved.
    #
    # NOTE: the init script is written to $INITRD_TMP/init just below, so we
    # cannot include it in this hash yet.  Instead we hash the guest binary,
    # the pelagos runtime binary, and a stable hash of this build script
    # itself as a proxy for "init script changed".  Any edit to
    # build-vm-image.sh (which owns the init script) will change the marker.
    ROOT_VERSION="$(cat "$INITRD_TMP/usr/local/bin/pelagos-guest" "$INITRD_TMP/usr/local/bin/pelagos" "$0" | shasum -a 256 | cut -c1-16)"
    echo "$ROOT_VERSION" > "$INITRD_TMP/etc/pelagos-root-version"
    echo "  root version marker: $ROOT_VERSION"

    # Replace /init.
    # $KVER and $ROOT_VERSION are build-time variables expanded by bash now.
    # \$ inside the heredoc are runtime shell variables — NOT expanded at build time.
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
# switch_root to /dev/vda (ext4) so that pivot_root(2) works for container
# spawns and the root filesystem is persistent on disk (not RAM).
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
    # virtiofs for AVF host directory sharing (pelagos volumes, $HOME bind-mounts).
    # =m in Ubuntu 6.8 HWE; absent on old images, silently skipped in that case.
    modprobe virtiofs            2>/dev/null || true
    # Bridge networking + nftables for pelagos container networking (-p / bridge mode).
    # These are =m in Ubuntu 6.8 HWE; loaded here so pelagos run works immediately
    # after boot.  Silently skipped on old images that predate #172.
    modprobe llc                 2>/dev/null || true
    modprobe stp                 2>/dev/null || true
    modprobe bridge              2>/dev/null || true
    modprobe libcrc32c           2>/dev/null || true
    modprobe nf_defrag_ipv4      2>/dev/null || true
    modprobe nf_defrag_ipv6      2>/dev/null || true
    modprobe nfnetlink           2>/dev/null || true
    modprobe nf_conntrack        2>/dev/null || true
    modprobe nf_nat              2>/dev/null || true
    modprobe nft_nat             2>/dev/null || true
    modprobe nft_chain_nat       2>/dev/null || true
    modprobe nft_masq            2>/dev/null || true
    modprobe nf_tables           2>/dev/null || true
    modprobe veth                2>/dev/null || true
    # Create /dev/net/tun device node.  The tun kernel module registers
    # the device (char major 10, minor 200) but does not create the node
    # automatically without udevd/mdev.  pasta requires /dev/net/tun to
    # create TAP interfaces for pasta-mode container networking.
    busybox mkdir -p /dev/net
    busybox mknod /dev/net/tun c 10 200 2>/dev/null || true
    busybox chmod 0666 /dev/net/tun 2>/dev/null || true

    echo "[pelagos-init] pass 1: modules loaded"

    busybox mkdir -p /newroot

    # Determine whether /dev/vda already has a current root filesystem.
    # ROOT_VERSION is the build-time hash of pelagos-guest (first 16 hex chars
    # of SHA256), embedded as a literal string here at image-build time.
    EXPECTED_VERSION="${ROOT_VERSION}"
    NEEDS_FORMAT=0
    NEEDS_COPY=0

    if ! busybox blkid /dev/vda 2>/dev/null | busybox grep -q 'TYPE="ext4"'; then
        echo "[pelagos-init] pass 1: /dev/vda has no ext4 filesystem — will format"
        NEEDS_FORMAT=1
        NEEDS_COPY=1
    else
        # Check the disk label BEFORE mounting — busybox blkid returns empty output
        # on an already-mounted device, so the label check MUST happen pre-mount.
        DISK_LABEL="\$(busybox blkid /dev/vda 2>/dev/null | busybox grep -o 'LABEL="[^"]*"' | busybox cut -d'"' -f2)"
        if [ -n "\$DISK_LABEL" ] && [ "\$DISK_LABEL" != "pelagos-root" ]; then
            # External rootfs (e.g. ubuntu-build) — mount it, then hand off to
            # the disk's own /sbin/init (systemd).  Skip all Alpine copy logic.
            echo "[pelagos-init] pass 1: external rootfs label='\$DISK_LABEL' — pivoting to disk's /sbin/init"
            busybox mount -t ext4 /dev/vda /newroot || {
                echo "[pelagos-init] FATAL: mount of external rootfs failed" >/dev/console
                exec busybox sh
            }
            # Configure the network interface with the static IP before switch_root.
            # Kernel networking persists across switch_root; this ensures the relay
            # can reach the VM immediately, even if systemd-networkd takes time.
            # Use ifconfig/route — they are in the busybox applet list; 'ip' is not.
            busybox ifconfig eth0 192.168.105.2 netmask 255.255.255.0 up 2>/dev/null || true
            busybox route add default gw 192.168.105.1 2>/dev/null || true
            echo "[pelagos-init] pass 1: configured eth0 with 192.168.105.2/24"
            exec busybox switch_root /newroot /sbin/init
            echo "[pelagos-init] FATAL: switch_root to external rootfs failed" >/dev/console 2>&1
            exec busybox sh
        elif busybox mount -t ext4 /dev/vda /newroot 2>/dev/null; then
            DISK_VERSION="\$(busybox cat /newroot/etc/pelagos-root-version 2>/dev/null || true)"
            if [ "\$DISK_VERSION" = "\$EXPECTED_VERSION" ]; then
                echo "[pelagos-init] pass 1: disk root is current (version=\$EXPECTED_VERSION)"
            else
                echo "[pelagos-init] pass 1: version mismatch (disk='\${DISK_VERSION:-none}' want='\$EXPECTED_VERSION') — refreshing"
                NEEDS_COPY=1
            fi
        else
            echo "[pelagos-init] pass 1: ext4 mount failed — will reformat"
            NEEDS_FORMAT=1
            NEEDS_COPY=1
        fi
    fi

    if [ "\$NEEDS_FORMAT" = "1" ]; then
        busybox umount /newroot 2>/dev/null || true
        echo "[pelagos-init] pass 1: formatting /dev/vda as ext4"
        /sbin/mke2fs -F -t ext4 -L pelagos-root /dev/vda 2>/dev/null || {
            echo "[pelagos-init] FATAL: mke2fs failed" >/dev/console
            exec busybox sh
        }
        busybox mount -t ext4 /dev/vda /newroot || {
            echo "[pelagos-init] FATAL: mount of fresh ext4 /dev/vda failed" >/dev/console
            exec busybox sh
        }
    fi

    if [ "\$NEEDS_COPY" = "1" ]; then
        echo "[pelagos-init] pass 1: copying root filesystem to /dev/vda"
        # Copy Alpine userland from initramfs to disk.  /var/lib/pelagos is
        # deliberately excluded so the OCI image cache survives root refreshes.
        for d in bin sbin usr lib etc root mnt; do
            [ -d "/\$d" ] && busybox cp -a "/\$d" /newroot/ 2>/dev/null || true
        done
        busybox cp /init /newroot/init
        busybox chmod 755 /newroot/init
        busybox mkdir -p /newroot/proc /newroot/sys \
                         /newroot/dev /newroot/dev/pts /newroot/dev/net \
                         /newroot/tmp /newroot/run /newroot/run/pelagos \
                         /newroot/sys/fs/cgroup /newroot/newroot \
                         /newroot/var /newroot/var/lib /newroot/var/lib/pelagos \
                         /newroot/var/run /newroot/var/log /newroot/var/tmp
        busybox mknod /newroot/dev/net/tun c 10 200 2>/dev/null || true
        busybox chmod 0666 /newroot/dev/net/tun 2>/dev/null || true
        # Write version marker LAST — an interrupted copy leaves no marker so
        # the next boot detects the incomplete state and re-copies.
        echo "\$EXPECTED_VERSION" > /newroot/etc/pelagos-root-version
        echo "[pelagos-init] pass 1: root filesystem written (version=\$EXPECTED_VERSION)"
    fi

    exec busybox switch_root /newroot /init

    echo "[pelagos-init] FATAL: switch_root failed" >/dev/console 2>&1
    exec busybox sh
fi

# Pass 2: root is /dev/vda (ext4). Kernel modules already loaded.
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
busybox ip addr add 192.168.105.2/24 dev eth0
busybox ip route add default via 192.168.105.1
echo "[pelagos-init] network: static 192.168.105.2/24"
# Enable IP forwarding unconditionally — this is a container runtime VM.
# pelagos port-forwarding uses nftables DNAT in PREROUTING to redirect
# host-port connections to the container IP, which requires ip_forward=1
# to move packets from eth0 to the bridge (pelagos0) interface.
echo 1 > /proc/sys/net/ipv4/ip_forward
# Allow all GIDs to open ICMP echo sockets (enables ping inside pasta containers).
echo '0 2147483647' > /proc/sys/net/ipv4/ping_group_range
echo "[pelagos-init] network ready"
busybox mkdir -p /etc
echo 'nameserver 8.8.8.8' > /etc/resolv.conf
echo 'nameserver 8.8.4.4' >> /etc/resolv.conf

# /tmp: bounded tmpfs — prevents VM-level OOM from unbounded temp storage.
# Container workloads write to their own overlayfs (on /dev/vda), not here.
busybox mkdir -p /tmp /run /run/pelagos
# pelagos looks for /run/netns/{name}; Alpine iproute2 creates /var/run/netns.
# Mount as tmpfs so stale netns files do not survive a VM restart (issue #161).
# Symlink /run/netns → /var/run/netns so both paths resolve to the same tmpfs.
busybox mkdir -p /var/run/netns
busybox mount -t tmpfs -o size=4m tmpfs /var/run/netns
busybox ln -sf /var/run/netns /run/netns
busybox mount -t tmpfs -o size=512m tmpfs /tmp

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

# /var/lib/pelagos is already on /dev/vda (we are /dev/vda).  No secondary
# mount needed.  Ensure the directory exists in case this is a fresh disk.
busybox mkdir -p /var/lib/pelagos

if [ "\$PELAGOS_VOLUMES_PRESENT" = "1" ]; then
    busybox mkdir -p /var/lib/pelagos/volumes
    busybox mount -t virtiofs pelagos-volumes /var/lib/pelagos/volumes && \
        echo "[pelagos-init] mounted pelagos-volumes virtiofs at /var/lib/pelagos/volumes" || \
        echo "[pelagos-init] WARNING: failed to mount pelagos-volumes virtiofs" >&2
fi

export PELAGOS_IMAGE_STORE=/var/lib/pelagos

busybox chown -R 0:0 /root 2>/dev/null || true
mkdir -p /etc/dropbear
/usr/sbin/dropbear -s -R -p 22 -F &

(while true; do /bin/sh </dev/hvc0 >/dev/hvc0 2>/dev/hvc0; sleep 1; done) &

export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export RUST_LOG=warn
# Raise the open-file limit.  The default (1024) is easily exceeded when VS
# Code opens 10+ simultaneous vsock connections each using namespace fds,
# pipe fds, and proc fds for exec-into.  64 k is the typical container default.
ulimit -n 65536
# Log to disk (/var/log is on /dev/vda ext4) so guest output does not consume
# tmpfs RAM.  /tmp is a bounded 512 MiB tmpfs reserved for transient workloads.
busybox mkdir -p /var/log
exec /usr/local/bin/pelagos-guest </dev/null >/var/log/pelagos-guest.log 2>&1
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
