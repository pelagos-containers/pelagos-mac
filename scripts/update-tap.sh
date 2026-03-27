#!/usr/bin/env bash
# update-tap.sh <tag>
#
# After pushing a release tag, run this script to:
#   1. Wait for the GitHub Actions release workflow to attach artifacts
#   2. Download both release tarballs and compute their SHA256s
#   3. Clone pelagos-containers/homebrew-tap, update Formula/pelagos-mac.rb
#   4. Commit and push the tap update
#
# Usage:
#   bash scripts/update-tap.sh v0.5.1

set -euo pipefail

TAG="${1:-}"
if [[ -z "$TAG" ]]; then
    echo "usage: $0 <tag>   e.g. $0 v0.5.1"
    exit 1
fi

VERSION="${TAG#v}"   # strip leading 'v'
REPO="pelagos-containers/pelagos-mac"
TAP_REPO="pelagos-containers/homebrew-tap"
BIN_ASSET="pelagos-mac-${VERSION}-aarch64-apple-darwin.tar.gz"
VM_ASSET="pelagos-mac-vm-${VERSION}.tar.gz"

echo "[tap] updating pelagos-mac formula to ${VERSION}"

# ---------------------------------------------------------------------------
# Wait for both release assets to be attached
# ---------------------------------------------------------------------------
echo "[tap] waiting for release artifacts on ${REPO} ${TAG}..."
TIMEOUT=1800   # 30 minutes
INTERVAL=30
ELAPSED=0

while true; do
    ASSETS="$(gh release view "${TAG}" --repo "${REPO}" --json assets \
        --jq '[.assets[].name] | join(" ")' 2>/dev/null || echo "")"

    BIN_READY=false
    VM_READY=false
    [[ "$ASSETS" == *"$BIN_ASSET"* ]] && BIN_READY=true
    [[ "$ASSETS" == *"$VM_ASSET"* ]]  && VM_READY=true

    if $BIN_READY && $VM_READY; then
        echo "[tap]   both artifacts present"
        break
    fi

    if (( ELAPSED >= TIMEOUT )); then
        echo "ERROR: timed out waiting for release artifacts after ${TIMEOUT}s"
        echo "  bin ready: $BIN_READY"
        echo "  vm  ready: $VM_READY"
        exit 1
    fi

    echo "[tap]   not ready yet (${ELAPSED}s elapsed) — retrying in ${INTERVAL}s..."
    sleep "$INTERVAL"
    (( ELAPSED += INTERVAL ))
done

# ---------------------------------------------------------------------------
# Download artifacts and compute SHA256s
# ---------------------------------------------------------------------------
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "[tap] downloading artifacts..."
gh release download "${TAG}" \
    --repo "${REPO}" \
    --pattern "${BIN_ASSET}" \
    --pattern "${VM_ASSET}" \
    --dir "${TMPDIR}"

BIN_SHA="$(shasum -a 256 "${TMPDIR}/${BIN_ASSET}" | awk '{print $1}')"
VM_SHA="$(shasum -a 256 "${TMPDIR}/${VM_ASSET}"  | awk '{print $1}')"
echo "[tap]   bin sha256: ${BIN_SHA}"
echo "[tap]   vm  sha256: ${VM_SHA}"

# ---------------------------------------------------------------------------
# Clone tap, update formula, commit, push
# ---------------------------------------------------------------------------
TAP_DIR="${TMPDIR}/homebrew-tap"
git clone "git@github.com:${TAP_REPO}.git" "${TAP_DIR}" --depth=1

FORMULA="${TAP_DIR}/Formula/pelagos-mac.rb"

# Update version
sed -i '' "s/^  version \".*\"/  version \"${VERSION}\"/" "${FORMULA}"

# Update the two sha256 lines.
# The formula has exactly two sha256 lines: the top-level one (bin tarball)
# and the one inside resource "vm" (VM tarball).  Use awk to replace them
# in order without touching the URL lines.
awk -v bin_sha="${BIN_SHA}" -v vm_sha="${VM_SHA}" '
    /^  sha256 "/ && !in_vm  { print "  sha256 \"" bin_sha "\""; in_vm=1; next }
    /resource "vm"/           { in_vm=2 }
    /^    sha256 "/ && in_vm==2 { print "    sha256 \"" vm_sha "\""; in_vm=3; next }
    { print }
' "${FORMULA}" > "${FORMULA}.tmp" && mv "${FORMULA}.tmp" "${FORMULA}"

echo "[tap] updated Formula/pelagos-mac.rb:"
grep -E 'version|sha256' "${FORMULA}"

cd "${TAP_DIR}"
git add Formula/pelagos-mac.rb
git commit -m "pelagos-mac ${VERSION}

Update formula to ${VERSION} with release artifact SHA256s.

Bin:  ${BIN_SHA}
VM:   ${VM_SHA}"
git push

echo ""
echo "[tap] done — homebrew-tap Formula/pelagos-mac.rb updated to ${VERSION}"
echo "      Users can now: brew upgrade pelagos-mac"
