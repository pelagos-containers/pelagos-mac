#!/usr/bin/env bash
# export-to-laptop.sh — copy all pelagos-mac context to a destination directory
# for transfer to a new machine.
#
# Usage:
#   bash scripts/export-to-laptop.sh /path/to/export-dir
#
# Then copy the export directory to the new laptop, e.g.:
#   rsync -av /path/to/export-dir/ newlaptop:~/pelagos-export/
# And on the new laptop, run the restore script it contains:
#   bash ~/pelagos-export/restore.sh

set -euo pipefail

DEST="${1:-}"
if [[ -z "$DEST" ]]; then
    echo "Usage: $0 <destination-directory>" >&2
    exit 1
fi

mkdir -p "$DEST"

# ---------------------------------------------------------------------------
# 1. Claude memory — persistent context for Claude Code sessions.
#    Stores architectural facts, feedback, project state, and build quirks.
#    Without this, Claude starts with zero context on the next machine.
#    Path must match exactly: Claude keys memory to the project path.
# ---------------------------------------------------------------------------
echo "Copying Claude memory..."
mkdir -p "$DEST/claude-memory"
rsync -a \
    "$HOME/.claude/projects/-Users-cb-Projects-pelagos-mac/memory/" \
    "$DEST/claude-memory/"

# ---------------------------------------------------------------------------
# 2. Claude global instructions — ~/.claude/CLAUDE.md
#    User-level settings that apply to all projects (e.g. persona preferences).
# ---------------------------------------------------------------------------
echo "Copying Claude global CLAUDE.md..."
mkdir -p "$DEST/claude-global"
cp "$HOME/.claude/CLAUDE.md" "$DEST/claude-global/CLAUDE.md"

# ---------------------------------------------------------------------------
# 3. Build VM profile config — vm.conf for the --profile build Ubuntu VM.
#    Contains: disk path, kernel, initrd, memory (4096), cpus (4),
#    ping_mode=ssh, cmdline (net.ifnames=0 nohz=off cpuidle.off=1).
#    Without this file, `pelagos --profile build ping` won't find the VM.
# ---------------------------------------------------------------------------
echo "Copying build VM profile config..."
mkdir -p "$DEST/pelagos-profiles/build"
cp "$HOME/.local/share/pelagos/profiles/build/vm.conf" \
   "$DEST/pelagos-profiles/build/vm.conf"

# ---------------------------------------------------------------------------
# 4. VM SSH key pair — ed25519 key used to SSH into the Alpine and Ubuntu VMs.
#    The public key is baked into the initramfs at VM image build time.
#    If the key is lost, SSH access requires rebuilding the VM image.
#    vm_key is mode 600 (private key); restore that on the other side.
# ---------------------------------------------------------------------------
echo "Copying VM SSH key pair..."
mkdir -p "$DEST/pelagos-ssh"
cp "$HOME/.local/share/pelagos/vm_key"     "$DEST/pelagos-ssh/vm_key"
cp "$HOME/.local/share/pelagos/vm_key.pub" "$DEST/pelagos-ssh/vm_key.pub"

# ---------------------------------------------------------------------------
# 5. Ubuntu build VM disk image — out/build.img
#    20 GB sparse ext4, Ubuntu 22.04 base + build-essential + Rust stable.
#    Already has systemd-networkd/resolved masked, plain resolv.conf, udev
#    masking — all required fixes for reliable cold-boot SSH in AVF.
#    Cold boot: ~61s. Without this, re-provisioning takes several minutes
#    (requires Alpine VM running + bash scripts/build-build-image.sh).
#
#    Compressed with gzip: zeros compress extremely well, so the .gz will be
#    close to the ~2.5 GB of actual data regardless of destination filesystem.
#    restore.sh decompresses back to a sparse file on the new machine.
# ---------------------------------------------------------------------------
echo "Compressing and copying build.img (~2.5 GB after compression)..."
mkdir -p "$DEST/out"
if command -v pv &>/dev/null; then
    pv "$HOME/Projects/pelagos-mac/out/build.img" | gzip -1 > "$DEST/out/build.img.gz"
else
    echo "(install pv for progress display; running without it)"
    gzip -1 -c "$HOME/Projects/pelagos-mac/out/build.img" > "$DEST/out/build.img.gz"
fi
echo "Compressed size: $(du -sh "$DEST/out/build.img.gz" | cut -f1)"

# ---------------------------------------------------------------------------
# Generate a restore.sh script inside the export directory.
# Run this on the new laptop after copying the export directory there.
# Assumes the project will be cloned to ~/Projects/pelagos-mac.
# ---------------------------------------------------------------------------
cat > "$DEST/restore.sh" <<'RESTORE'
#!/usr/bin/env bash
# restore.sh — restore pelagos-mac context on a new laptop.
# Run from inside the export directory after cloning the repo to ~/Projects/pelagos-mac.
set -euo pipefail

EXPORT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Restoring Claude memory..."
mkdir -p "$HOME/.claude/projects/-Users-cb-Projects-pelagos-mac/memory"
rsync -a "$EXPORT_DIR/claude-memory/" \
    "$HOME/.claude/projects/-Users-cb-Projects-pelagos-mac/memory/"

echo "Restoring Claude global CLAUDE.md..."
mkdir -p "$HOME/.claude"
cp "$EXPORT_DIR/claude-global/CLAUDE.md" "$HOME/.claude/CLAUDE.md"

echo "Restoring build VM profile config..."
mkdir -p "$HOME/.local/share/pelagos/profiles/build"
cp "$EXPORT_DIR/pelagos-profiles/build/vm.conf" \
   "$HOME/.local/share/pelagos/profiles/build/vm.conf"

echo "Restoring VM SSH key pair..."
mkdir -p "$HOME/.local/share/pelagos"
cp "$EXPORT_DIR/pelagos-ssh/vm_key"     "$HOME/.local/share/pelagos/vm_key"
cp "$EXPORT_DIR/pelagos-ssh/vm_key.pub" "$HOME/.local/share/pelagos/vm_key.pub"
chmod 600 "$HOME/.local/share/pelagos/vm_key"

echo "Decompressing build.img (this may take a minute)..."
mkdir -p "$HOME/Projects/pelagos-mac/out"
gunzip -c "$EXPORT_DIR/out/build.img.gz" > "$HOME/Projects/pelagos-mac/out/build.img"

echo ""
echo "Done. Next steps:"
echo "  1. cd ~/Projects/pelagos-mac"
echo "  2. cargo build -p pelagos-mac --release && bash scripts/sign.sh"
echo "  3. pelagos --profile build ping"
RESTORE

chmod +x "$DEST/restore.sh"

echo ""
echo "Export complete: $DEST"
echo ""
echo "Transfer to new laptop:"
echo "  rsync -av \"$DEST/\" newlaptop:~/pelagos-export/"
echo "Then on the new laptop:"
echo "  bash ~/pelagos-export/restore.sh"
