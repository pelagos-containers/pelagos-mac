#!/usr/bin/env bash
# build-release.sh — Build release tarballs and write the Homebrew formula.
#
# Produces in dist/:
#
#   pelagos-mac-<version>-aarch64-apple-darwin.tar.gz  (~15 MB)
#     pelagos
#     pelagos-docker
#     pelagos-tui
#
#   pelagos-mac-vm-<version>.tar.gz  (~215 MB)
#     vmlinuz        (normalised from out/ubuntu-vmlinuz)
#     initramfs.gz   (normalised from out/initramfs-custom.gz)
#     root.img
#
#   tap/Formula/pelagos-mac.rb  — Homebrew formula with file:// URLs + sha256s
#                                  also synced to the local brew tap
#
# The production formula in pelagos-containers/homebrew-tap is identical except the
# urls are https://github.com/.../releases/download/... instead of file://.
#
# To install locally after running this script:
#
#   brew uninstall pelagos-mac 2>/dev/null || true
#   HOMEBREW_DEVELOPER=1 HOMEBREW_NO_INSTALL_FROM_API=1 brew install pelagos-containers/tap/pelagos-mac
#
# Prerequisites: out/ must exist (run scripts/build-vm-image.sh first).

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$REPO/out"
DIST="$REPO/dist"

# ---------------------------------------------------------------------------
# Version
# ---------------------------------------------------------------------------
VERSION="$(grep '^version\s*=' "$REPO/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')"
BIN_TARBALL="pelagos-mac-${VERSION}-aarch64-apple-darwin.tar.gz"
VM_TARBALL="pelagos-mac-vm-${VERSION}.tar.gz"

echo "[release] version ${VERSION}"

# ---------------------------------------------------------------------------
# Check prerequisites
# ---------------------------------------------------------------------------
for f in ubuntu-vmlinuz initramfs-custom.gz root.img; do
    if [[ ! -f "$OUT/$f" ]]; then
        echo "ERROR: $OUT/$f not found"
        echo "       Run scripts/build-vm-image.sh first."
        exit 1
    fi
done

# ---------------------------------------------------------------------------
# Build + sign
# ---------------------------------------------------------------------------
if [[ -f "$REPO/target/aarch64-apple-darwin/release/.developer-id-signed" ]]; then
    echo "[release] Developer ID binary present — skipping rebuild and ad-hoc sign"
else
    echo "[release] building pelagos..."
    cargo build -p pelagos-mac --release 2>&1 | grep -E "Compiling|Finished|^error"
    echo "[release] signing pelagos..."
    bash "$REPO/scripts/sign.sh"
fi

echo "[release] building pelagos-docker..."
cargo build -p pelagos-docker --release 2>&1 | grep -E "Compiling|Finished|^error"

echo "[release] building pelagos-tui..."
cargo build -p pelagos-tui --release 2>&1 | grep -E "Compiling|Finished|^error"

echo "[release] building pelagos-pfctl..."
cargo build -p pelagos-pfctl --release 2>&1 | grep -E "Compiling|Finished|^error"

# ---------------------------------------------------------------------------
# Pack
# ---------------------------------------------------------------------------
mkdir -p "$DIST"
BIN_STAGING="$(mktemp -d)"
VM_STAGING="$(mktemp -d)"
trap 'rm -rf "$BIN_STAGING" "$VM_STAGING"' EXIT

cp "$REPO/target/aarch64-apple-darwin/release/pelagos"        "$BIN_STAGING/pelagos"
cp "$REPO/target/aarch64-apple-darwin/release/pelagos-docker" "$BIN_STAGING/pelagos-docker"
cp "$REPO/target/aarch64-apple-darwin/release/pelagos-tui"    "$BIN_STAGING/pelagos-tui"
# The helper is named by its bundle identifier.  SMJobBless looks for it
# in the same directory as the calling binary using this exact name.
cp "$REPO/target/aarch64-apple-darwin/release/pelagos-pfctl"  "$BIN_STAGING/com.pelagos.pfctl"
# Entitlements, LaunchDaemon plist, and fallback install script shipped in
# share/pelagos-mac.  The install script is a last-resort fallback only.
mkdir -p "$BIN_STAGING/share"
cp "$REPO/pelagos-mac/entitlements.plist"                      "$BIN_STAGING/share/entitlements.plist"
cp "$REPO/scripts/com.pelagos.pfctl.plist"                     "$BIN_STAGING/share/com.pelagos.pfctl.plist"
cp "$REPO/scripts/install-pfctl-daemon.sh"                     "$BIN_STAGING/share/install-pfctl-daemon.sh"

echo "[release] packing ${BIN_TARBALL}..."
COPYFILE_DISABLE=1 tar -czf "$DIST/$BIN_TARBALL" -C "$BIN_STAGING" .
BIN_SHA256="$(shasum -a 256 "$DIST/$BIN_TARBALL" | awk '{print $1}')"
echo "[release]   $(du -sh "$DIST/$BIN_TARBALL" | awk '{print $1}')  sha256: ${BIN_SHA256}"

# Normalise names: drop ubuntu- prefix and -custom suffix.
cp "$OUT/ubuntu-vmlinuz"      "$VM_STAGING/vmlinuz"
cp "$OUT/initramfs-custom.gz" "$VM_STAGING/initramfs.gz"
# Always create a fresh sparse placeholder — never ship the local disk which
# contains per-machine container image cache and state.  On first boot the VM
# formats this as ext4 and populates it from the initramfs.
truncate -s 8192m "$VM_STAGING/root.img"

echo "[release] packing ${VM_TARBALL}..."
# root.img is a fresh sparse 8192 MiB placeholder; zeros compress to ~1 MiB.
COPYFILE_DISABLE=1 tar -czf "$DIST/$VM_TARBALL" -C "$VM_STAGING" .
VM_SHA256="$(shasum -a 256 "$DIST/$VM_TARBALL" | awk '{print $1}')"
echo "[release]   $(du -sh "$DIST/$VM_TARBALL" | awk '{print $1}')  sha256: ${VM_SHA256}"

# ---------------------------------------------------------------------------
# Write formula into dist/tap (canonical source) and sync to brew tap
# ---------------------------------------------------------------------------
TAP_FORMULA="$DIST/tap/Formula/pelagos-mac.rb"
BREW_TAP_FORMULA="/opt/homebrew/Library/Taps/pelagos-containers/homebrew-tap/Formula/pelagos-mac.rb"

mkdir -p "$DIST/tap/Formula"

cat > "$TAP_FORMULA" <<FORMULA
# pelagos-mac Homebrew formula — LOCAL TEST BUILD
#
# Generated by scripts/build-release.sh.  Do not commit this file to the
# main repo; the canonical formula lives in pelagos-containers/homebrew-tap.
# The production formula is identical except urls are https://github.com/…
#
# Install:   brew install pelagos-containers/tap/pelagos-mac
# Uninstall: brew uninstall pelagos-mac

class PelagosMac < Formula
  desc "Linux container runtime for Apple Silicon via Virtualization.framework"
  homepage "https://github.com/pelagos-containers/pelagos-mac"
  version "${VERSION}"

  url "file://${DIST}/${BIN_TARBALL}"
  sha256 "${BIN_SHA256}"

  resource "vm" do
    url "file://${DIST}/${VM_TARBALL}"
    sha256 "${VM_SHA256}"
  end

  def install
    bin.install "pelagos"
    bin.install "pelagos-docker"
    bin.install "pelagos-tui"
    # SMJobBless convention for non-bundle CLIs: the helper binary must be in
    # the same directory as the calling binary, named by its bundle identifier.
    # Install as bin/com.pelagos.pfctl (symlink to pkgshare copy).
    pkgshare.install "com.pelagos.pfctl"
    bin.install_symlink pkgshare/"com.pelagos.pfctl"
    pkgshare.install "share/entitlements.plist"
    pkgshare.install "share/com.pelagos.pfctl.plist"
    pkgshare.install "share/install-pfctl-daemon.sh"
    resource("vm").stage { pkgshare.install Dir["*"] }
  end

  def post_install
    # macOS 26 taskgated validates the AVF entitlement signature at the binary's
    # actual run path.  Re-sign both binaries after install.
    entitlements = pkgshare/"entitlements.plist"
    system "codesign", "--sign", "-", "--entitlements", entitlements.to_s,
           "--force", (bin/"pelagos").to_s
    # Re-sign the helper at its pkgshare path and the bin symlink target.
    system "codesign", "--sign", "-", "--force", (pkgshare/"com.pelagos.pfctl").to_s
  end

  def caveats
    <<~EOS
      On first 'pelagos vm start', pelagos will automatically install the
      privileged helper daemon (pelagos-pfctl) via SMJobBless.  macOS will
      prompt for admin credentials once.

      If SMJobBless fails (e.g. signing requirements don't match), run manually:
        sudo bash #{pkgshare}/install-pfctl-daemon.sh
    EOS
  end

  test do
    assert_match "pelagos", shell_output("#{bin}/pelagos --help 2>&1")
  end
end
FORMULA

# Sync to the brew tap directory so no manual copy is needed at install time.
if [[ -d "$(dirname "$BREW_TAP_FORMULA")" ]]; then
    cp "$TAP_FORMULA" "$BREW_TAP_FORMULA"
    echo "[release] synced formula to brew tap"
else
    echo "[release] warning: brew tap not found at $(dirname "$BREW_TAP_FORMULA") — run 'brew tap skeptomai/tap dist/tap' first"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "[release] done."
echo ""
echo "  $DIST/$BIN_TARBALL"
echo "  $DIST/$VM_TARBALL"
echo "  $TAP_FORMULA"
echo ""
echo "  To install:"
echo "    brew uninstall pelagos-mac 2>/dev/null || true"
echo "    HOMEBREW_DEVELOPER=1 HOMEBREW_NO_INSTALL_FROM_API=1 brew install pelagos-containers/tap/pelagos-mac"
