#!/usr/bin/env bash
# test-tap-token.sh -- Test that a TAP_GITHUB_TOKEN can read+write to homebrew-tap.
#
# Usage:
#   bash scripts/test-tap-token.sh <TOKEN>
set -euo pipefail

TOKEN="${1:-}"
if [[ -z "$TOKEN" ]]; then
    echo "Usage: bash scripts/test-tap-token.sh <TOKEN>" >&2
    exit 1
fi

REPO="pelagos-containers/homebrew-tap"
FILE="Casks/pelagos-ui.rb"
export GH_TOKEN="$TOKEN"

echo "=== testing TAP_GITHUB_TOKEN ==="

printf "  read $FILE... "
SHA=$(gh api "repos/$REPO/contents/$FILE" --jq '.sha' 2>&1) || true
if [[ "$SHA" =~ ^[0-9a-f]{40}$ ]]; then
    echo "ok (sha: ${SHA:0:12}...)"
else
    echo "FAILED"
    echo "  $SHA"
    exit 1
fi

printf "  write $FILE (no-op update)... "
CONTENT=$(gh api "repos/$REPO/contents/$FILE" --jq '.content' | tr -d '\n')
RESULT=$(gh api --method PUT "repos/$REPO/contents/$FILE" \
    --field message="chore: token write test (no-op)" \
    --field content="$CONTENT" \
    --field sha="$SHA" \
    --jq '.commit.sha' 2>&1) || true
if [[ "$RESULT" =~ ^[0-9a-f]{40}$ ]]; then
    echo "ok (commit: ${RESULT:0:12}...)"
else
    echo "FAILED"
    echo "  $RESULT"
    exit 1
fi

echo ""
echo "Token works. Read and write access confirmed."
