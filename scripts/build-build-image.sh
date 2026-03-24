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
# I/O design (Alternative A):
#   build.img is passed as a second virtio-blk device (--extra-disk) to the
#   Alpine provisioning VM, appearing as /dev/vdb.  All provisioning I/O goes
#   directly block → ext4 with no FUSE/virtiofs in the path.  The virtiofs
#   volumes share is used only for the small provisioning script.
#
# Requirements:
#   - Alpine VM NOT running (this script stops and restarts it temporarily)
#   - out/vmlinuz, out/initramfs-custom.gz must exist
#   - scripts/build-vm-image.sh must have been run (stages loop.ko)
#   - pelagos release binary built and signed
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
echo "=== build-build-image.sh (Alternative A — virtio-blk /dev/vdb) ==="
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
# Helper: invoke the Alpine VM with the default config.
# Used for pre-flight ping and for the final normal restart.
# ---------------------------------------------------------------------------

pelagos_alpine() {
    "$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$ALPINE_DISK" "$@"
}

# Helper: invoke the Alpine VM with build.img attached as /dev/vdb.
# Used only during the provisioning session.

pelagos_provision() {
    "$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$ALPINE_DISK" \
        --extra-disk "$BUILD_IMG" "$@"
}

# ---------------------------------------------------------------------------
# Stop any running Alpine VM daemon so we can restart it with --extra-disk.
# ---------------------------------------------------------------------------

stop_alpine_vm() {
    # Terminate the daemon process and remove stale state files.
    pkill -TERM -f "pelagos.*vm-daemon-internal" 2>/dev/null || true
    # Give the daemon time to shut down cleanly.
    sleep 2
    # Force-kill if still present.
    pkill -KILL -f "pelagos.*vm-daemon-internal" 2>/dev/null || true
    rm -f "$PELAGOS_BASE/vm.pid" "$PELAGOS_BASE/vm.sock"
}

# ---------------------------------------------------------------------------
# Check whether the Alpine VM is already running, and stop it.
# We need an exclusive boot to attach --extra-disk.
# ---------------------------------------------------------------------------

echo "--- stopping Alpine VM (if running) ---"
if pelagos_alpine ping 2>/dev/null | grep -q pong; then
    echo "  Alpine VM is running — stopping it for provisioning boot"
    stop_alpine_vm
    echo "  stopped"
else
    # Ensure any stale pid/sock files are removed.
    stop_alpine_vm
    echo "  not running (ok)"
fi
echo ""

# ---------------------------------------------------------------------------
# Create the sparse build image on macOS.
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
# Start the Alpine VM with build.img as /dev/vdb.
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
# Write the provisioning script to the volumes dir (virtiofs, small file only).
# The script itself is tiny; only the build.img I/O avoids virtiofs.
# ---------------------------------------------------------------------------

PUB_KEY_CONTENT="$(cat "${SSH_KEY_FILE}.pub")"

mkdir -p "$ALPINE_VOLUMES_DIR"

cat > "$ALPINE_VOLUMES_DIR/provision-build.sh" << OUTER_EOF
#!/bin/sh
# Provisioning script — runs inside the Alpine VM as root.
# build.img is presented as /dev/vdb (virtio-blk); no loop device required.
set -eux

BLK=/dev/vdb
MNT=/mnt/build-provision

# Format /dev/vdb if it doesn't already have the ubuntu-build label.
if blkid "\$BLK" 2>/dev/null | grep -q 'LABEL="ubuntu-build"'; then
    echo "[provision] /dev/vdb already formatted as ubuntu-build — skipping format"
else
    echo "[provision] formatting /dev/vdb as ext4 label=ubuntu-build"
    /sbin/mke2fs -t ext4 -L ubuntu-build "\$BLK"
fi

mkdir -p "\$MNT"
mount "\$BLK" "\$MNT"
echo "[provision] mounted /dev/vdb at \$MNT"

# ---- Ubuntu base extraction ----

if [ -f "\$MNT/etc/os-release" ]; then
    echo "[provision] Ubuntu base already extracted — skipping download"
else
    echo "[provision] downloading Ubuntu 22.04 arm64 base tarball"
    # Download to /tmp (tmpfs) — tarball is ~30 MB, fits easily in RAM.
    TARBALL="/tmp/${UBUNTU_TARBALL_NAME}"
    if [ ! -f "\$TARBALL" ]; then
        wget -q -O "\$TARBALL" "${UBUNTU_BASE_URL}" || \
            curl -fsSL -o "\$TARBALL" "${UBUNTU_BASE_URL}"
    fi
    echo "[provision] extracting Ubuntu base"
    tar -xzf "\$TARBALL" -C "\$MNT"
    rm -f "\$TARBALL"
    echo "[provision] extraction complete"
fi

# ---- chroot provisioning ----

# Bind-mount kernel filesystems.
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
# flash-kernel tries to flash the kernel to embedded ARM hardware; it fails
# in a VM with "Unsupported platform" and blocks post-install hooks for any
# package that pulls in initramfs-tools.  Remove it before installing anything.
chroot "\$MNT" env DEBIAN_FRONTEND=noninteractive apt-get remove -y flash-kernel 2>/dev/null || true
chroot "\$MNT" env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential git curl wget ca-certificates \
    iproute2 nftables openssh-server sudo \
    systemd systemd-sysv systemd-timesyncd \
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
# Keep any IP pre-configured by the initramfs so the relay can reach the
# VM before networkd re-applies config.
KeepConfiguration=static
NETCFG

# Enable services via symlinks — systemctl enable doesn't work without running
# systemd, but symlink creation is equivalent and always works in a chroot.
mkdir -p "\$MNT/etc/systemd/system/multi-user.target.wants"
ln -sf /lib/systemd/system/ssh.service \
    "\$MNT/etc/systemd/system/multi-user.target.wants/ssh.service" 2>/dev/null || true

# Enable systemd-networkd — configures eth0 with the static IP at boot.
# With the Ubuntu kernel, no initramfs pre-configures eth0; networkd is
# responsible for bringing the interface up.
ln -sf /lib/systemd/system/systemd-networkd.service \
    "\$MNT/etc/systemd/system/multi-user.target.wants/systemd-networkd.service" 2>/dev/null || true

# Enable serial-getty on hvc0 with root auto-login.
# With the Ubuntu kernel, /dev/hvc0 is available at boot (virtio_console
# is built in), so the getty starts cleanly.  Auto-login gives interactive
# emergency access without a password — this is a single-user build VM.
mkdir -p "\$MNT/etc/systemd/system/serial-getty@hvc0.service.d"
cat > "\$MNT/etc/systemd/system/serial-getty@hvc0.service.d/autologin.conf" << 'AUTOLOGIN_CONF'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root --noclear %I \$TERM
AUTOLOGIN_CONF

# Mask systemd-resolved — without it, /etc/resolv.conf would be a dead
# symlink pointing to resolved's stub socket, breaking all DNS lookups.
ln -sf /dev/null "\$MNT/etc/systemd/system/systemd-resolved.service"

# Static resolv.conf — plain file, not a symlink to the resolved stub.
rm -f "\$MNT/etc/resolv.conf"
printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > "\$MNT/etc/resolv.conf"

# Load overlay at boot — required for pelagos container workloads.
# The Ubuntu 6.8 HWE kernel ships overlay as a module (=m), not built-in,
# so it must be explicitly loaded.  /etc/modules is read by systemd-modules-load.
printf 'overlay\n' >> "\$MNT/etc/modules"

# Disable predictable interface renaming (belt-and-suspenders alongside
# net.ifnames=0 in the kernel cmdline).  Without this, udev renames eth0
# to enp0sN, bringing it down while networkd is trying to configure it.
mkdir -p "\$MNT/etc/udev/rules.d"
ln -sf /dev/null "\$MNT/etc/udev/rules.d/80-net-setup-link.rules"

# ---- SSH ----

mkdir -p "\$MNT/root/.ssh"
chmod 700 "\$MNT/root/.ssh"
printf '%s\n' "${PUB_KEY_CONTENT}" > "\$MNT/root/.ssh/authorized_keys"
chmod 600 "\$MNT/root/.ssh/authorized_keys"

# Ubuntu default PermitRootLogin is "prohibit-password" (key auth only) — correct.
sed -i 's/^PermitRootLogin no/PermitRootLogin prohibit-password/' "\$MNT/etc/ssh/sshd_config" 2>/dev/null || true

# ---- Rust toolchain ----

echo "[provision] installing Rust stable toolchain"
# HOME must be set explicitly; chroot doesn't inherit it reliably.
chroot "\$MNT" env HOME=/root \
    bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
             | sh -s -- -y --default-toolchain stable --no-modify-path'

# Make rustc/cargo available system-wide for login and non-login shells.
# Use '. /root/.cargo/env' rather than baking in $PATH — the OUTER_EOF heredoc
# is unquoted so $PATH would expand to the macOS host PATH at provisioning time.
printf '%s\n' '. /root/.cargo/env' > "\$MNT/etc/profile.d/rust.sh"
chmod +x "\$MNT/etc/profile.d/rust.sh"

# Append to root's .bashrc so non-login interactive shells also get cargo.
printf '\n# Rust toolchain\nsource /root/.cargo/env\n' >> "\$MNT/root/.bashrc"

# git needs an explicit CA bundle path on Ubuntu 22.04 — without this, git
# reports "CAfile: none" even though ca-certificates is installed.
chroot "\$MNT" git config --global http.sslCAInfo /etc/ssl/certs/ca-certificates.crt

# Sync the system clock on boot via systemd-timesyncd (NTP).
# Without this the VM clock is frozen at image-build time, causing TLS
# certificate verification failures for git and cargo.
ln -sf /lib/systemd/system/systemd-timesyncd.service \
    "\$MNT/etc/systemd/system/multi-user.target.wants/systemd-timesyncd.service" 2>/dev/null || true

# ---- Extract Ubuntu kernel and initrd for AVF boot ----
#
# AVF VZLinuxBootLoader requires a raw arm64 EFI-stub Image (MZ + ARMd at
# offset 0x38), not the gzip-wrapped vmlinuz Ubuntu ships.  Decompress with
# zcat here inside the Alpine VM, then copy initrd as-is (Ubuntu's 6.8 kernel
# handles zstd initrds natively).  Both files land on the virtiofs share so
# the outer script can move them to out/ on the macOS host.

echo "[provision] extracting Ubuntu kernel, initrd, and modules for host AVF boot"
KVER=\$(ls "\$MNT/boot/vmlinuz-"* 2>/dev/null | sort -V | tail -1 | sed 's|.*/vmlinuz-||')
if [ -n "\$KVER" ]; then
    zcat "\$MNT/boot/vmlinuz-\$KVER" > /var/lib/pelagos/volumes/ubuntu-vmlinuz
    cp "\$MNT/boot/initrd.img-\$KVER" /var/lib/pelagos/volumes/ubuntu-initrd.img
    echo "  kernel: vmlinuz-\$KVER (\$(du -sh /var/lib/pelagos/volumes/ubuntu-vmlinuz | cut -f1) decompressed)"
    echo "  initrd: initrd.img-\$KVER (\$(du -sh /var/lib/pelagos/volumes/ubuntu-initrd.img | cut -f1))"

    # Extract kernel modules that are =m in Ubuntu 6.8 HWE and are required
    # by the container VM initramfs.  All virtio drivers are =y (built-in).
    #
    # Modules extracted:
    #   vsock          — pelagos-guest ↔ host comms
    #   overlayfs      — container layer stacking
    #   bridge + deps  — pelagos bridge networking (NetworkMode::Bridge, -p flag)
    #   nftables       — port-forward DNAT rules (pelagos network.rs)
    MODDIR="\$MNT/lib/modules/\$KVER/kernel"
    VOLS=/var/lib/pelagos/volumes
    mkdir -p \
        "\$VOLS/ubuntu-modules/net/vmw_vsock" \
        "\$VOLS/ubuntu-modules/fs/overlayfs" \
        "\$VOLS/ubuntu-modules/net/bridge" \
        "\$VOLS/ubuntu-modules/net/802" \
        "\$VOLS/ubuntu-modules/net/llc" \
        "\$VOLS/ubuntu-modules/net/netfilter"
    # vsock
    for ko in \
        "\$MODDIR/net/vmw_vsock/vsock.ko" \
        "\$MODDIR/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko" \
        "\$MODDIR/net/vmw_vsock/vmw_vsock_virtio_transport.ko"
    do
        [ -f "\$ko" ] && cp "\$ko" "\$VOLS/ubuntu-modules/net/vmw_vsock/" \
            && echo "  module: \$(basename \$ko)"
    done
    # overlayfs
    [ -f "\$MODDIR/fs/overlayfs/overlay.ko" ] \
        && cp "\$MODDIR/fs/overlayfs/overlay.ko" "\$VOLS/ubuntu-modules/fs/overlayfs/" \
        && echo "  module: overlay.ko"
    # bridge + dependency chain (llc → stp → bridge)
    for ko in \
        "\$MODDIR/net/llc/llc.ko" \
        "\$MODDIR/net/802/stp.ko" \
        "\$MODDIR/net/bridge/bridge.ko"
    do
        dir="\$VOLS/ubuntu-modules/\$(dirname \${ko#\$MODDIR/})"
        mkdir -p "\$dir"
        [ -f "\$ko" ] && cp "\$ko" "\$dir/" && echo "  module: \$(basename \$ko)"
    done
    # nftables / netfilter (required for port-forward DNAT rules)
    for ko in \
        "\$MODDIR/net/netfilter/nfnetlink.ko" \
        "\$MODDIR/net/netfilter/nf_tables.ko" \
        "\$MODDIR/net/netfilter/nf_conntrack.ko" \
        "\$MODDIR/net/netfilter/nf_nat.ko"
    do
        dir="\$VOLS/ubuntu-modules/\$(dirname \${ko#\$MODDIR/})"
        mkdir -p "\$dir"
        [ -f "\$ko" ] && cp "\$ko" "\$dir/" && echo "  module: \$(basename \$ko)"
    done
    # modules.dep so modprobe can resolve the full dependency chain.
    cp "\$MNT/lib/modules/\$KVER/modules.dep" "\$VOLS/ubuntu-modules/" 2>/dev/null || true
    cp "\$MNT/lib/modules/\$KVER/modules.dep.bin" "\$VOLS/ubuntu-modules/" 2>/dev/null || true
    # Write kver.txt so build-vm-image.sh can detect the kernel version without
    # parsing modules.dep (which uses relative paths, not absolute paths).
    echo "\$KVER" > "\$VOLS/ubuntu-modules/kver.txt"
    echo "  stored Ubuntu modules in volumes/ubuntu-modules/ (kver: \$KVER)"
else
    echo "WARNING: no vmlinuz found in \$MNT/boot — cannot extract kernel" >&2
fi

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

chmod +x "$ALPINE_VOLUMES_DIR/provision-build.sh"
echo "--- wrote provisioning script ---"
echo ""

# ---------------------------------------------------------------------------
# Execute the provisioning script inside the Alpine VM.
# ---------------------------------------------------------------------------

echo "--- running provisioning script in Alpine VM ---"
echo "    waiting for VM SSH to become available (~30s cold start)..."
echo "    once connected, [provision] log lines will stream in real-time"
echo ""

pelagos_provision vm ssh -- sh /var/lib/pelagos/volumes/provision-build.sh

echo ""
echo "--- provisioning complete ---"

# Clean up the provisioning script from the volumes dir.
rm -f "$ALPINE_VOLUMES_DIR/provision-build.sh"

# ---------------------------------------------------------------------------
# Stop the provisioning VM and restart the normal Alpine VM.
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
# Write vm.conf for the build profile.
# ---------------------------------------------------------------------------

# Move the Ubuntu kernel and initrd from the Alpine volumes dir to out/.
# These were extracted from the provisioned build.img by the provisioning
# script and staged on the virtiofs share.
UBUNTU_VMLINUZ="$REPO_ROOT/out/ubuntu-vmlinuz"
UBUNTU_INITRD="$REPO_ROOT/out/ubuntu-initrd.img"
if [[ -f "$ALPINE_VOLUMES_DIR/ubuntu-vmlinuz" ]]; then
    mv "$ALPINE_VOLUMES_DIR/ubuntu-vmlinuz"   "$UBUNTU_VMLINUZ"
    mv "$ALPINE_VOLUMES_DIR/ubuntu-initrd.img" "$UBUNTU_INITRD"
    echo "--- extracted Ubuntu kernel to out/ubuntu-vmlinuz ($(du -sh "$UBUNTU_VMLINUZ" | cut -f1)) ---"
    echo "--- extracted Ubuntu initrd  to out/ubuntu-initrd.img ($(du -sh "$UBUNTU_INITRD" | cut -f1)) ---"
    # Move kernel modules needed by the container VM initramfs.
    UBUNTU_MODULES_SRC="$ALPINE_VOLUMES_DIR/ubuntu-modules"
    UBUNTU_MODULES_DST="$REPO_ROOT/out/ubuntu-modules"
    if [[ -d "$UBUNTU_MODULES_SRC" ]]; then
        rm -rf "$UBUNTU_MODULES_DST"
        mv "$UBUNTU_MODULES_SRC" "$UBUNTU_MODULES_DST"
        echo "--- extracted Ubuntu kernel modules to out/ubuntu-modules/ ---"
    fi
else
    echo "ERROR: ubuntu-vmlinuz not found in Alpine volumes dir — kernel extraction failed" >&2
    exit 1
fi
echo ""

echo "--- writing vm.conf for profile '$PROFILE' ---"
mkdir -p "$PROFILE_STATE_DIR"
cat > "$PROFILE_STATE_DIR/vm.conf" << VMCONF_EOF
# vm.conf — auto-written by build-build-image.sh
# Profile: $PROFILE
disk      = $BUILD_IMG
kernel    = $UBUNTU_VMLINUZ
initrd    = $UBUNTU_INITRD
memory    = $MEMORY_MIB
cpus      = $CPUS
ping_mode = ssh
# net.ifnames=0: prevent udev from renaming eth0 → enp0sN.
# Without this, udev brings eth0 down to rename it, dropping the IP configured
# by the initramfs before switch_root, leaving smoltcp unable to ARP the VM.
# cpuidle.off=1: disable cpuidle-psci deep idle states. Ubuntu 6.8 HWE uses
# PSCI CPU_SUSPEND for deep idle; AVF does not reliably deliver hrtimers to
# vCPUs parked in PSCI idle, causing rcu_preempt kthread timer stalls.
# nohz=off alone did not help — tick delivery is fine, hrtimer delivery is not.
cmdline   = console=hvc0 net.ifnames=0 cpuidle.off=1 root=LABEL=ubuntu-build rw
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
