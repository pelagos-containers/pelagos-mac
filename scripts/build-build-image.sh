#!/usr/bin/env bash
# build-build-image.sh — Provision a 20 GB Ubuntu 22.04 build VM image.
#
# Creates out/build.img: an ext4 filesystem labeled "ubuntu-build" containing
# a minimal Ubuntu 22.04 arm64 rootfs with Rust toolchain and pelagos build
# dependencies.  The image boots via the same kernel/initramfs as the Alpine
# pelagos VM; the init script pivots to Ubuntu systemd when it detects the
# "ubuntu-build" disk label instead of "pelagos-root".
#
# After provisioning, writes vm.conf for the named profile so
#   pelagos --profile <name> ping
# boots the Ubuntu VM without extra flags.
#
# Requirements:
#   - Alpine VM already running: bash scripts/vm-ping.sh
#   - out/vmlinuz, out/initramfs-custom.gz must exist
#   - scripts/build-vm-image.sh must have been run (stages loop.ko)
#
# Usage:
#   bash scripts/build-build-image.sh [--profile <name>] [--memory <mib>] [--cpus <n>]
#
# Options:
#   --profile <name>   Profile name for the build VM (default: "build")
#   --memory  <mib>    Memory for the build VM in MiB (default: 4096)
#   --cpus    <n>      vCPU count for the build VM (default: 4)
#   --disk-size <gb>   Disk size in GB (default: 20)
#
# The build VM is accessible after provisioning via:
#   bash scripts/vm-restart.sh --profile <name>
#   pelagos --profile <name> vm ssh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

PROFILE="build"
MEMORY_MIB=4096
CPUS=4
DISK_SIZE_GB=20

while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile)   PROFILE="$2";     shift 2 ;;
        --memory)    MEMORY_MIB="$2";  shift 2 ;;
        --cpus)      CPUS="$2";        shift 2 ;;
        --disk-size) DISK_SIZE_GB="$2"; shift 2 ;;
        *)           echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
ALPINE_DISK="$REPO_ROOT/out/root.img"
BUILD_IMG="$REPO_ROOT/out/build.img"

PELAGOS_BASE="${XDG_DATA_HOME:-$HOME/.local/share}/pelagos"
if [[ "$PROFILE" == "default" ]]; then
    PROFILE_STATE_DIR="$PELAGOS_BASE"
else
    PROFILE_STATE_DIR="$PELAGOS_BASE/profiles/$PROFILE"
fi

ALPINE_VOLUMES_DIR="$PELAGOS_BASE/volumes"
SSH_KEY_FILE="$PELAGOS_BASE/vm_key"
UBUNTU_BASE_URL="http://cdimage.ubuntu.com/ubuntu-base/releases/22.04/release/ubuntu-base-22.04-base-arm64.tar.gz"
UBUNTU_TARBALL_NAME="ubuntu-base-22.04-base-arm64.tar.gz"

# ---------------------------------------------------------------------------
# Pre-flight checks
# ---------------------------------------------------------------------------

echo ""
echo "=== build-build-image.sh ==="
echo "  profile:   $PROFILE"
echo "  output:    $BUILD_IMG"
echo "  memory:    ${MEMORY_MIB} MiB"
echo "  cpus:      $CPUS"
echo "  disk size: ${DISK_SIZE_GB} GB"
echo ""

for f in "$KERNEL" "$INITRD" "$ALPINE_DISK" "$BINARY"; do
    if [[ ! -f "$f" ]]; then
        echo "ABORT: missing $f" >&2
        echo "       Run 'bash scripts/build-vm-image.sh' and 'bash scripts/sign.sh' first." >&2
        exit 1
    fi
done

if [[ ! -f "$SSH_KEY_FILE" ]]; then
    echo "ABORT: SSH key $SSH_KEY_FILE not found" >&2
    echo "       Run 'bash scripts/build-vm-image.sh' first to generate it." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Ensure Alpine VM is running
# ---------------------------------------------------------------------------

pelagos_alpine() {
    "$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$ALPINE_DISK" "$@"
}

echo "--- pre-flight: Alpine VM ---"
printf "  pinging Alpine VM... "
if ! pelagos_alpine ping 2>&1 | grep -q pong; then
    echo ""
    echo "ABORT: Alpine VM not running." >&2
    echo "       Run 'bash scripts/vm-ping.sh' first." >&2
    exit 1
fi
echo "ok"
echo ""

# ---------------------------------------------------------------------------
# Create the sparse build image on macOS
# ---------------------------------------------------------------------------

DISK_SIZE_MB=$((DISK_SIZE_GB * 1024))

if [[ -f "$BUILD_IMG" ]]; then
    echo "--- reusing existing $BUILD_IMG ---"
    echo "  (delete it to reprovision from scratch)"
    echo ""
else
    echo "--- creating sparse ${DISK_SIZE_GB} GB image ---"
    dd if=/dev/zero of="$BUILD_IMG" bs=1m count=0 seek="$DISK_SIZE_MB" 2>/dev/null
    echo "  created: $BUILD_IMG (sparse ${DISK_SIZE_GB} GB)"
    echo ""
fi

# ---------------------------------------------------------------------------
# Copy build.img to the Alpine volumes dir so the VM can see it via virtiofs.
# ---------------------------------------------------------------------------

mkdir -p "$ALPINE_VOLUMES_DIR"
VOLUMES_IMG="$ALPINE_VOLUMES_DIR/build.img"

if [[ "$BUILD_IMG" -ef "$VOLUMES_IMG" ]]; then
    : # same file (shouldn't happen but be safe)
elif [[ -f "$VOLUMES_IMG" ]]; then
    echo "--- $VOLUMES_IMG already present (skipping copy) ---"
    echo ""
else
    echo "--- copying build.img to volumes dir for VM access ---"
    cp "$BUILD_IMG" "$VOLUMES_IMG"
    echo "  copied to $VOLUMES_IMG"
    echo ""
fi

# ---------------------------------------------------------------------------
# Write the provisioning script to the volumes dir (visible in VM as
# /var/lib/pelagos/volumes/provision-build.sh).
# ---------------------------------------------------------------------------

PUB_KEY_CONTENT="$(cat "${SSH_KEY_FILE}.pub")"

cat > "$ALPINE_VOLUMES_DIR/provision-build.sh" << OUTER_EOF
#!/bin/sh
# Provisioning script — runs inside the Alpine VM as root.
# Executed by build-build-image.sh via pelagos vm ssh.
# Uses only busybox-compatible commands (no util-linux losetup extensions).
set -eux

VOLUMES=/var/lib/pelagos/volumes
IMG="\$VOLUMES/build.img"
MNT=/mnt/build-provision
LODEV=/dev/loop0

echo "[provision] loading loop module"
modprobe loop
# Loop module doesn't auto-create device nodes without udev; create manually.
mknod "\$LODEV" b 7 0 2>/dev/null || true

# Format the image if it doesn't already have the ubuntu-build label.
# mke2fs -F operates directly on a regular file — no loop device needed.
if blkid "\$IMG" 2>/dev/null | grep -q 'LABEL="ubuntu-build"'; then
    echo "[provision] image already formatted as ubuntu-build — skipping format"
else
    echo "[provision] formatting as ext4 label=ubuntu-build"
    /sbin/mke2fs -F -t ext4 -L ubuntu-build "\$IMG"
fi

# Attach loop device for mounting.
losetup "\$LODEV" "\$IMG"
echo "[provision] attached \$LODEV"

mkdir -p "\$MNT"
mount "\$LODEV" "\$MNT"

# ---- Ubuntu base extraction ----

if [ -f "\$MNT/etc/os-release" ]; then
    echo "[provision] Ubuntu base already extracted — skipping download"
else
    echo "[provision] downloading Ubuntu 22.04 arm64 base tarball"
    TARBALL="\$VOLUMES/${UBUNTU_TARBALL_NAME}"
    if [ ! -f "\$TARBALL" ]; then
        wget -q -O "\$TARBALL" "${UBUNTU_BASE_URL}" || \
            curl -fsSL -o "\$TARBALL" "${UBUNTU_BASE_URL}"
    fi
    echo "[provision] extracting Ubuntu base"
    tar -xzf "\$TARBALL" -C "\$MNT"
    echo "[provision] extraction complete"
fi

# ---- chroot provisioning ----

# Bind-mount kernel filesystems.  Create target dirs first — ubuntu-base
# includes /proc and /sys but /dev/pts may be absent.
mkdir -p "\$MNT/proc" "\$MNT/sys" "\$MNT/dev" "\$MNT/dev/pts"
mount -t proc  proc   "\$MNT/proc"
mount -t sysfs sysfs  "\$MNT/sys"
mount --bind /dev     "\$MNT/dev"
mount --bind /dev/pts "\$MNT/dev/pts"

# DNS for apt inside chroot.
echo "nameserver 8.8.8.8"  > "\$MNT/etc/resolv.conf"
echo "nameserver 1.1.1.1" >> "\$MNT/etc/resolv.conf"

# apt sources.
cat > "\$MNT/etc/apt/sources.list" << 'SOURCES'
deb http://ports.ubuntu.com/ubuntu-ports jammy main restricted universe multiverse
deb http://ports.ubuntu.com/ubuntu-ports jammy-updates main restricted universe multiverse
deb http://ports.ubuntu.com/ubuntu-ports jammy-security main restricted universe multiverse
SOURCES

echo "[provision] apt-get update + install"
chroot "\$MNT" apt-get update -qq
chroot "\$MNT" env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential git curl wget ca-certificates \
    iproute2 nftables openssh-server \
    systemd systemd-sysv \
    pkg-config libssl-dev

# ---- networking: systemd-networkd with static IP ----

echo "[provision] setting up systemd-networkd for static IP"
mkdir -p "\$MNT/etc/systemd/network"
cat > "\$MNT/etc/systemd/network/10-eth.network" << 'NETCFG'
[Match]
Name=en* eth*

[Network]
Address=192.168.105.2/24
Gateway=192.168.105.1
DNS=8.8.8.8
DNS=1.1.1.1
NETCFG

# Enable services via symlinks — systemctl enable doesn't work without running
# systemd, but symlink creation is equivalent and always works in a chroot.
mkdir -p "\$MNT/etc/systemd/system/multi-user.target.wants"
ln -sf /lib/systemd/system/systemd-networkd.service \
    "\$MNT/etc/systemd/system/multi-user.target.wants/systemd-networkd.service" 2>/dev/null || true
ln -sf /lib/systemd/system/ssh.service \
    "\$MNT/etc/systemd/system/multi-user.target.wants/ssh.service" 2>/dev/null || true

# ---- SSH ----

mkdir -p "\$MNT/root/.ssh"
chmod 700 "\$MNT/root/.ssh"
printf '%s\n' "${PUB_KEY_CONTENT}" > "\$MNT/root/.ssh/authorized_keys"
chmod 600 "\$MNT/root/.ssh/authorized_keys"

# Ubuntu default PermitRootLogin is "prohibit-password" (key auth only) — correct.
# No sshd_config edit needed; just confirm it's not set to "no".
sed -i 's/^PermitRootLogin no/PermitRootLogin prohibit-password/' "\$MNT/etc/ssh/sshd_config" 2>/dev/null || true

# ---- Rust toolchain ----

echo "[provision] installing Rust stable toolchain"
# HOME must be set explicitly; chroot doesn't inherit it reliably.
chroot "\$MNT" env HOME=/root \
    bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
             | sh -s -- -y --default-toolchain stable --no-modify-path'

# ---- cleanup ----

echo "[provision] cleaning up"
chroot "\$MNT" apt-get clean
umount "\$MNT/dev/pts" 2>/dev/null || true
umount "\$MNT/dev"     2>/dev/null || true
umount "\$MNT/sys"     2>/dev/null || true
umount "\$MNT/proc"    2>/dev/null || true
umount "\$MNT"
losetup -d "\$LODEV"
rmdir  "\$MNT" 2>/dev/null || true

echo "[provision] done"
OUTER_EOF

chmod +x "$ALPINE_VOLUMES_DIR/provision-build.sh"
echo "--- wrote provisioning script ---"
echo ""

# ---------------------------------------------------------------------------
# Execute the provisioning script inside the Alpine VM.
# ---------------------------------------------------------------------------

echo "--- running provisioning script in Alpine VM ---"
echo "    (this will download Ubuntu and install packages; takes several minutes)"
echo ""

pelagos_alpine vm ssh -- sh /var/lib/pelagos/volumes/provision-build.sh

echo ""
echo "--- provisioning complete ---"

# Clean up the provisioning script and tarball from the volumes dir.
rm -f "$ALPINE_VOLUMES_DIR/provision-build.sh"
rm -f "$ALPINE_VOLUMES_DIR/$UBUNTU_TARBALL_NAME"

# ---------------------------------------------------------------------------
# Move build.img from volumes dir to out/ (canonical location).
# ---------------------------------------------------------------------------

echo ""
echo "--- moving build.img to out/ ---"
mv "$VOLUMES_IMG" "$BUILD_IMG"
echo "  $BUILD_IMG"

# ---------------------------------------------------------------------------
# Write vm.conf for the build profile.
# ---------------------------------------------------------------------------

echo ""
echo "--- writing vm.conf for profile '$PROFILE' ---"
mkdir -p "$PROFILE_STATE_DIR"
cat > "$PROFILE_STATE_DIR/vm.conf" << VMCONF_EOF
# vm.conf — auto-written by build-build-image.sh
# Profile: $PROFILE
disk   = $BUILD_IMG
kernel = $KERNEL
initrd = $INITRD
memory = $MEMORY_MIB
cpus   = $CPUS
VMCONF_EOF

echo "  $PROFILE_STATE_DIR/vm.conf"
echo ""
echo "=== build VM image ready ==="
echo ""
echo "Boot the build VM:"
echo "  bash scripts/vm-restart.sh --profile $PROFILE"
echo "SSH into it:"
echo "  pelagos --profile $PROFILE vm ssh"
echo ""
