#!/usr/bin/env bash
# build-debian-image.sh — Provision a Debian bookworm arm64 build VM image.
#
# Creates out/debian.img: an ext4 filesystem labeled "debian-build" containing
# a minimal Debian bookworm arm64 rootfs with Rust toolchain and pelagos build
# dependencies (glibc, libseccomp-dev, libcap-dev).
#
# Reuses out/ubuntu-vmlinuz and out/ubuntu-initrd.img (extracted by
# build-build-image.sh).  Kernel and rootfs do not need to match distros;
# the Ubuntu 6.8 HWE kernel is already proven to work with AVF and its
# compatibility workarounds (cpuidle.off=1, net.ifnames=0) are already known.
#
# After provisioning, writes vm.conf for the named profile so
#   pelagos --profile <name> ping
# boots the Debian VM without extra flags.
#
# I/O design: debian.img is passed as a second virtio-blk device (--extra-disk)
# to the Alpine provisioning VM, appearing as /dev/vdb.  The provisioning
# script runs inside the Alpine VM via SSH stdin.
#
# Requirements:
#   - Alpine VM NOT running (script stops and restarts it temporarily)
#   - out/vmlinuz, out/initramfs-custom.gz, out/root.img must exist
#   - out/ubuntu-vmlinuz, out/ubuntu-initrd.img must exist
#     (run scripts/build-build-image.sh first if not present)
#   - pelagos release binary built and signed
#
# Usage:
#   bash scripts/build-debian-image.sh [--profile <name>] [--memory <mib>] [--cpus <n>] [--disk-size <gb>]
#
# Options:
#   --profile   <name>  Profile name (default: "debian")
#   --memory    <mib>   Memory in MiB (default: 2048)
#   --cpus      <n>     vCPU count (default: 2)
#   --disk-size <gb>    Disk size in GB (default: 10)
#
# After provisioning:
#   bash scripts/vm-restart.sh --profile <name>
#   pelagos --profile <name> vm ssh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

PROFILE="debian"
MEMORY_MIB=2048
CPUS=2
DISK_SIZE_GB=10

while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile)   PROFILE="$2";      shift 2 ;;
        --memory)    MEMORY_MIB="$2";   shift 2 ;;
        --cpus)      CPUS="$2";         shift 2 ;;
        --disk-size) DISK_SIZE_GB="$2"; shift 2 ;;
        *)           echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
ALPINE_DISK="$REPO_ROOT/out/root.img"
DEBIAN_IMG="$REPO_ROOT/out/debian.img"
UBUNTU_VMLINUZ="$REPO_ROOT/out/ubuntu-vmlinuz"
UBUNTU_INITRD="$REPO_ROOT/out/ubuntu-initrd.img"

PELAGOS_BASE="${XDG_DATA_HOME:-$HOME/.local/share}/pelagos"
if [[ "$PROFILE" == "default" ]]; then
    PROFILE_STATE_DIR="$PELAGOS_BASE"
else
    PROFILE_STATE_DIR="$PELAGOS_BASE/profiles/$PROFILE"
fi

SSH_KEY_FILE="$PELAGOS_BASE/vm_key"

# ---------------------------------------------------------------------------
# Pre-flight checks
# ---------------------------------------------------------------------------

echo ""
echo "=== build-debian-image.sh ==="
echo "  profile:   $PROFILE"
echo "  output:    $DEBIAN_IMG"
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

for f in "$UBUNTU_VMLINUZ" "$UBUNTU_INITRD"; do
    if [[ ! -f "$f" ]]; then
        echo "ABORT: missing $f" >&2
        echo "       Run 'bash scripts/build-build-image.sh' first to extract the Ubuntu kernel." >&2
        exit 1
    fi
done

if [[ ! -f "$SSH_KEY_FILE" ]]; then
    echo "ABORT: SSH key $SSH_KEY_FILE not found" >&2
    echo "       Run 'bash scripts/build-vm-image.sh' first to generate it." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

pelagos_alpine() {
    "$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$ALPINE_DISK" "$@"
}

pelagos_provision() {
    "$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$ALPINE_DISK" \
        --extra-disk "$DEBIAN_IMG" "$@"
}

stop_alpine_vm() {
    pkill -TERM -f "pelagos.*vm-daemon-internal" 2>/dev/null || true
    sleep 2
    pkill -KILL -f "pelagos.*vm-daemon-internal" 2>/dev/null || true
    rm -f "$PELAGOS_BASE/vm.pid" "$PELAGOS_BASE/vm.sock"
}

# ---------------------------------------------------------------------------
# Stop any running Alpine VM
# ---------------------------------------------------------------------------

echo "--- stopping Alpine VM (if running) ---"
if pelagos_alpine ping 2>/dev/null | grep -q pong; then
    echo "  Alpine VM is running — stopping for provisioning boot"
    stop_alpine_vm
    echo "  stopped"
else
    stop_alpine_vm
    echo "  not running (ok)"
fi
echo ""

# ---------------------------------------------------------------------------
# Create the sparse disk image
# ---------------------------------------------------------------------------

DISK_SIZE_MB=$((DISK_SIZE_GB * 1024))

if [[ -f "$DEBIAN_IMG" ]]; then
    echo "--- reusing existing $DEBIAN_IMG ---"
    echo "  (delete it to reprovision from scratch)"
    echo ""
else
    echo "--- creating sparse ${DISK_SIZE_GB} GB image ---"
    dd if=/dev/zero of="$DEBIAN_IMG" bs=1m count=0 seek="$DISK_SIZE_MB" 2>/dev/null
    echo "  created: $DEBIAN_IMG (sparse ${DISK_SIZE_GB} GB)"
    echo ""
fi

# ---------------------------------------------------------------------------
# Boot Alpine VM with debian.img as /dev/vdb
# ---------------------------------------------------------------------------

echo "--- booting Alpine VM with --extra-disk (provisioning session) ---"
printf "  pinging... "
if ! pelagos_provision ping 2>&1 | grep -q pong; then
    echo ""
    echo "ABORT: provisioning VM did not respond to ping." >&2
    exit 1
fi
echo "ok"
echo ""

# ---------------------------------------------------------------------------
# Write the provisioning script
# ---------------------------------------------------------------------------

PUB_KEY_CONTENT="$(cat "${SSH_KEY_FILE}.pub")"

PROVISION_SCRIPT="$(mktemp /tmp/provision-debian.XXXXXX.sh)"
trap 'rm -f "$PROVISION_SCRIPT"' EXIT

cat > "$PROVISION_SCRIPT" << OUTER_EOF
#!/bin/sh
# Provisioning script — runs inside the Alpine VM as root.
# debian.img is presented as /dev/vdb (virtio-blk).
set -eux

BLK=/dev/vdb
MNT=/mnt/debian-provision

# Format /dev/vdb if not already labeled debian-build.
if blkid "\$BLK" 2>/dev/null | grep -q 'LABEL="debian-build"'; then
    echo "[provision] /dev/vdb already formatted as debian-build — skipping format"
else
    echo "[provision] formatting /dev/vdb as ext4 label=debian-build"
    /sbin/mke2fs -t ext4 -L debian-build "\$BLK"
fi

mkdir -p "\$MNT"
mount "\$BLK" "\$MNT"
echo "[provision] mounted /dev/vdb at \$MNT"

# ---- Debian base tarball download ----
# Use a pre-built Debian bookworm arm64 rootfs from the LXC image server.
# This mirrors the Ubuntu approach in build-build-image.sh: download a base
# tarball and extract it, rather than running debootstrap (which needs perl
# — not available in the Alpine provisioning initramfs).
#
# LXC image server URL:
#   https://images.linuxcontainers.org/images/debian/bookworm/arm64/default/
# Each dated directory (YYYYMMDD_HH:MM) contains rootfs.tar.xz.

LXC_BASE="https://images.linuxcontainers.org/images/debian/bookworm/arm64/default"

if [ -f "\$MNT/etc/debian_version" ]; then
    echo "[provision] Debian rootfs already present — skipping download"
else
    echo "[provision] fetching Debian bookworm arm64 rootfs from LXC image server"
    # Parse directory listing to find the most recent dated directory.
    # The colon in the directory name (e.g. 20260323_05:24) is URL-encoded
    # as %3A in the href attribute.
    LATEST_DIR=\$(wget -qO- "\${LXC_BASE}/" \
        | sed -n 's|.*href="\([0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]_[0-9][0-9]%3A[0-9][0-9]\)/.*|\1|p' \
        | sed 's/%3A/:/g' \
        | tail -1)
    if [ -z "\$LATEST_DIR" ]; then
        echo "[provision] ERROR: could not determine latest Debian image directory" >&2
        exit 1
    fi
    ROOTFS_URL="\${LXC_BASE}/\${LATEST_DIR}/rootfs.tar.xz"
    echo "[provision] downloading \${ROOTFS_URL}"
    # Download to /tmp (tmpfs) — rootfs is ~100 MB, fits in RAM.
    wget -q -O /tmp/debian-rootfs.tar.xz "\$ROOTFS_URL"
    echo "[provision] extracting Debian base rootfs"
    # unxz is compiled into busybox but may not have a symlink in the
    # Alpine provisioning initramfs — call it via busybox directly.
    busybox unxz -c /tmp/debian-rootfs.tar.xz | tar -xf - -C "\$MNT"
    rm -f /tmp/debian-rootfs.tar.xz
    # Verify extraction produced a real rootfs.
    if [ ! -d "\$MNT/etc" ]; then
        echo "[provision] ERROR: rootfs extraction failed — \$MNT/etc missing" >&2
        exit 1
    fi
    echo "[provision] Debian base extraction complete"
fi

# ---- chroot provisioning ----

mkdir -p "\$MNT/proc" "\$MNT/sys" "\$MNT/dev" "\$MNT/dev/pts"
mount -t proc  proc   "\$MNT/proc"
mount -t sysfs sysfs  "\$MNT/sys"
mount --bind /dev     "\$MNT/dev"
mount --bind /dev/pts "\$MNT/dev/pts"

# The LXC rootfs may have resolv.conf as a symlink into /run/systemd/resolve/
# which does not exist yet.  Remove it before writing a plain file.
rm -f "\$MNT/etc/resolv.conf"
printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > "\$MNT/etc/resolv.conf"

# apt sources for bookworm
cat > "\$MNT/etc/apt/sources.list" << 'SOURCES'
deb http://deb.debian.org/debian bookworm main contrib non-free-firmware
deb http://deb.debian.org/debian bookworm-updates main contrib non-free-firmware
deb http://security.debian.org/debian-security bookworm-security main contrib non-free-firmware
SOURCES

echo "[provision] apt-get update + install"
chroot "\$MNT" env DEBIAN_FRONTEND=noninteractive apt-get update -qq
chroot "\$MNT" env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential git curl wget ca-certificates \
    iproute2 openssh-server sudo \
    systemd systemd-sysv systemd-timesyncd \
    pkg-config libssl-dev libseccomp-dev libcap-dev

# ---- networking: systemd-networkd with static IP ----

echo "[provision] configuring systemd-networkd"
mkdir -p "\$MNT/etc/systemd/network"
cat > "\$MNT/etc/systemd/network/10-eth.network" << 'NETCFG'
[Match]
Name=en* eth*

[Network]
Address=192.168.105.2/24
Gateway=192.168.105.1
DNS=8.8.8.8
DNS=1.1.1.1
KeepConfiguration=static
NETCFG

mkdir -p "\$MNT/etc/systemd/system/multi-user.target.wants"

ln -sf /lib/systemd/system/ssh.service \
    "\$MNT/etc/systemd/system/multi-user.target.wants/ssh.service" 2>/dev/null || true

ln -sf /lib/systemd/system/systemd-networkd.service \
    "\$MNT/etc/systemd/system/multi-user.target.wants/systemd-networkd.service" 2>/dev/null || true

# Auto-login console on hvc0 for emergency access.
mkdir -p "\$MNT/etc/systemd/system/serial-getty@hvc0.service.d"
cat > "\$MNT/etc/systemd/system/serial-getty@hvc0.service.d/autologin.conf" << 'AUTOLOGIN_CONF'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root --noclear %I \$TERM
AUTOLOGIN_CONF

# Mask systemd-resolved to keep /etc/resolv.conf as a plain file.
ln -sf /dev/null "\$MNT/etc/systemd/system/systemd-resolved.service"
rm -f "\$MNT/etc/resolv.conf"
printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > "\$MNT/etc/resolv.conf"

# Disable predictable interface renaming (belt-and-suspenders with net.ifnames=0).
mkdir -p "\$MNT/etc/udev/rules.d"
ln -sf /dev/null "\$MNT/etc/udev/rules.d/80-net-setup-link.rules"

# Sync clock on boot.
ln -sf /lib/systemd/system/systemd-timesyncd.service \
    "\$MNT/etc/systemd/system/multi-user.target.wants/systemd-timesyncd.service" 2>/dev/null || true

# ---- SSH ----

mkdir -p "\$MNT/root/.ssh"
chmod 700 "\$MNT/root/.ssh"
printf '%s\n' "${PUB_KEY_CONTENT}" > "\$MNT/root/.ssh/authorized_keys"
chmod 600 "\$MNT/root/.ssh/authorized_keys"

# ---- Rust toolchain ----

echo "[provision] installing Rust stable toolchain"
chroot "\$MNT" env HOME=/root \
    bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
             | sh -s -- -y --default-toolchain stable --no-modify-path'

printf '%s\n' '. /root/.cargo/env' > "\$MNT/etc/profile.d/rust.sh"
chmod +x "\$MNT/etc/profile.d/rust.sh"
printf '\n# Rust toolchain\nsource /root/.cargo/env\n' >> "\$MNT/root/.bashrc"

chroot "\$MNT" git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt

# ---- cleanup ----

echo "[provision] cleaning up"
chroot "\$MNT" apt-get clean
umount "\$MNT/dev/pts" 2>/dev/null || true
umount "\$MNT/dev"     2>/dev/null || true
umount "\$MNT/sys"     2>/dev/null || true
umount "\$MNT/proc"    2>/dev/null || true
umount "\$MNT"
rmdir  "\$MNT" 2>/dev/null || true

echo "[provision] done"
OUTER_EOF

chmod +x "$PROVISION_SCRIPT"
echo "--- wrote provisioning script ---"
echo ""

# ---------------------------------------------------------------------------
# Run provisioning script inside Alpine VM via SSH stdin
# ---------------------------------------------------------------------------

echo "--- running provisioning script in Alpine VM ---"
echo "    (debootstrap downloads ~300 MB; this takes several minutes)"
echo ""

pelagos_provision vm ssh -- "sh -s" < "$PROVISION_SCRIPT"

echo ""
echo "--- provisioning complete ---"

# ---------------------------------------------------------------------------
# Stop provisioning VM, restart normal Alpine VM
# ---------------------------------------------------------------------------

echo ""
echo "--- stopping provisioning VM ---"
stop_alpine_vm
echo "  done"
echo ""

echo "--- restarting Alpine VM (normal, without extra-disk) ---"
printf "  pinging... "
if ! pelagos_alpine ping 2>&1 | grep -q pong; then
    echo ""
    echo "WARNING: Alpine VM did not respond after restart." >&2
    echo "         Run 'bash scripts/vm-ping.sh' manually to restore it." >&2
else
    echo "ok"
fi
echo ""

# ---------------------------------------------------------------------------
# Write vm.conf for the debian profile
# ---------------------------------------------------------------------------

echo "--- writing vm.conf for profile '$PROFILE' ---"
mkdir -p "$PROFILE_STATE_DIR"
cat > "$PROFILE_STATE_DIR/vm.conf" << VMCONF_EOF
# vm.conf — auto-written by build-debian-image.sh
# Profile: $PROFILE
disk      = $DEBIAN_IMG
kernel    = $UBUNTU_VMLINUZ
initrd    = $UBUNTU_INITRD
memory    = $MEMORY_MIB
cpus      = $CPUS
ping_mode = ssh
# net.ifnames=0: prevent udev from renaming eth0 → enp0sN.
# cpuidle.off=1: disable PSCI deep idle; AVF does not reliably deliver
# hrtimers to parked vCPUs, causing rcu_preempt stalls (same issue as
# Ubuntu 6.8 HWE — same kernel, same fix).
cmdline   = console=hvc0 net.ifnames=0 cpuidle.off=1 root=LABEL=debian-build rw
VMCONF_EOF

echo "  $PROFILE_STATE_DIR/vm.conf"
echo ""
echo "=== Debian VM image ready ==="
echo ""
echo "Boot the Debian VM:"
echo "  bash scripts/vm-restart.sh --profile $PROFILE"
echo "SSH into it:"
echo "  pelagos --profile $PROFILE vm ssh"
echo ""
