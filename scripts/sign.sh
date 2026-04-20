#!/usr/bin/env bash
# sign.sh — Ad-hoc sign the pelagos binary with the AVF virtualization entitlement.
#
# Signs target/aarch64-apple-darwin/release/pelagos in-place.
# NOTE: sign AFTER installing, not before — macOS 26 taskgated validates the
# signature at the binary's actual run path. install-dev.sh handles this correctly.
#
# For development an ad-hoc signature (-) is sufficient.
# For distribution, replace "-" with your Developer ID Application identity.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
ENTITLEMENTS="$REPO_ROOT/pelagos-mac/entitlements.plist"

if [ ! -f "$BINARY" ]; then
    echo "ERROR: binary not found: $BINARY"
    echo "Build it first:  cargo build --release -p pelagos-mac"
    exit 1
fi

codesign --sign - --entitlements "$ENTITLEMENTS" --force "$BINARY"
echo "Signed: $BINARY"
