#!/usr/bin/env bash
# notarize.sh — Sign and notarize the pelagos-mac binary.
#
# Run after `cargo build --release -p pelagos-mac`.
# Signs with Developer ID + virtualization entitlement (hardened runtime),
# submits to Apple notarization, and verifies with spctl.
#
# Usage:
#   bash scripts/notarize.sh
#
# Prerequisites:
#   - Developer ID Application cert installed in Keychain (see docs/SIGNING.md)
#   - ~/.private_keys/AuthKey_U9KZ8M7HL9.p8 present

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$REPO/target/aarch64-apple-darwin/release/pelagos"
ENTITLEMENTS="$REPO/scripts/pelagos-mac.entitlements"

SIGN_IDENTITY="Developer ID Application: Christopher Brown (8HHC296Z8Q)"
KEY_FILE="${HOME}/.private_keys/AuthKey_U9KZ8M7HL9.p8"
KEY_ID="U9KZ8M7HL9"
ISSUER="69a6de6e-96f9-47e3-e053-5b8c7c11a4d1"

# ---------------------------------------------------------------------------
# Preflight checks
# ---------------------------------------------------------------------------
if [[ ! -f "$BINARY" ]]; then
    echo "ERROR: $BINARY not found — run 'cargo build --release -p pelagos-mac' first"
    exit 1
fi

if [[ ! -f "$KEY_FILE" ]]; then
    echo "ERROR: $KEY_FILE not found — see docs/SIGNING.md Step 1"
    exit 1
fi

if ! security find-identity -v -p codesigning | grep -q "Developer ID Application"; then
    echo "ERROR: Developer ID Application cert not found in Keychain — see docs/SIGNING.md Step 1"
    exit 1
fi

# ---------------------------------------------------------------------------
# Sign
# ---------------------------------------------------------------------------
echo "[notarize] signing pelagos..."
codesign --sign "$SIGN_IDENTITY" \
    --entitlements "$ENTITLEMENTS" \
    --options runtime --force \
    "$BINARY"

codesign --verify --verbose "$BINARY"
echo "[notarize] signature valid"

# ---------------------------------------------------------------------------
# Notarize
# ---------------------------------------------------------------------------
TMPZIP="$(mktemp /tmp/pelagos-notarize-XXXXXX.zip)"
trap 'rm -f "$TMPZIP"' EXIT

echo "[notarize] packing for submission..."
ditto -c -k --keepParent "$BINARY" "$TMPZIP"

echo "[notarize] submitting to Apple (this takes a few minutes)..."
xcrun notarytool submit "$TMPZIP" \
    --key "$KEY_FILE" \
    --key-id "$KEY_ID" \
    --issuer "$ISSUER" \
    --wait

echo ""
echo "[notarize] done. Binary is signed and notarized (status: Accepted)."
echo "  Run 'bash scripts/build-release.sh' to pack the release tarballs."
