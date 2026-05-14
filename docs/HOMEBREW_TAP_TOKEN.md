# Homebrew Tap Token Setup

The release CI workflows for pelagos, pelagos-mac, and pelagos-ui all
auto-update the [homebrew-tap](https://github.com/pelagos-containers/homebrew-tap)
repo after a successful release. Each repo updates a different file:

| Repo | Updates | File in homebrew-tap |
|------|---------|---------------------|
| pelagos | Linux formula | `Formula/pelagos.rb` |
| pelagos-mac | macOS formula | `Formula/pelagos-mac.rb` |
| pelagos-ui | macOS cask | `Casks/pelagos-ui.rb` |

All three use a GitHub Actions secret named `TAP_GITHUB_TOKEN` -- a
fine-grained personal access token (PAT) with write access to the
homebrew-tap repo.

---

## Prerequisites: org token policy

Fine-grained tokens scoped to org repos require the org to allow them.
This only needs to be done once.

1. Go to https://github.com/organizations/pelagos-containers/settings/personal-access-tokens
2. Ensure **Allow access via fine-grained personal access tokens** is enabled
3. **Require administrator approval** can be on or off (if on, you must
   approve your own token request after creating it)

---

## Step 1: Create the token

Go to: https://github.com/settings/personal-access-tokens/new

Fill in EXACTLY these values:

| Field | Value |
|-------|-------|
| **Token name** | `homebrew-tap-updater` |
| **Expiration** | Custom -> 1 year from today (set a calendar reminder) |
| **Resource owner** | **`pelagos-containers`** (NOT your personal account -- use the dropdown) |

If `pelagos-containers` does not appear in the Resource owner dropdown,
the org token policy is not enabled (see Prerequisites above).

Under **Repository access**, select:

- **Only select repositories**
- Search for and select: **`pelagos-containers/homebrew-tap`**

Under **Permissions** -> **Repository permissions**, set:

| Permission | Access level |
|-----------|-------------|
| **Contents** | **Read and write** |

Everything else stays at "No access". Do NOT set any other permissions.

Click **Generate token**. Copy the token value immediately -- you cannot
see it again.

### If admin approval is required

If the org has "Require administrator approval" enabled, the token is
created in a pending state. Go to:

https://github.com/organizations/pelagos-containers/settings/personal-access-token-requests

Find your pending request and approve it.

---

## Step 2: Add the secret to each repo

The same token value must be added to all three repos. Open each URL
and add the secret:

1. https://github.com/pelagos-containers/pelagos/settings/secrets/actions
2. https://github.com/pelagos-containers/pelagos-mac/settings/secrets/actions
3. https://github.com/pelagos-containers/pelagos-ui/settings/secrets/actions

For each repo:
- Click **New repository secret**
- **Name**: `TAP_GITHUB_TOKEN`
- **Value**: paste the token from Step 1
- Click **Add secret**

If updating an existing secret, click the pencil icon next to
`TAP_GITHUB_TOKEN` and paste the new value.

---

## Verification

After adding the secrets, verify by re-running a failed tap update job:

```bash
# Find the failed run ID
cd ~/Projects/pelagos-ui
gh run list --limit 5

# Re-run only the failed jobs
gh run rerun <RUN_ID> --failed

# Watch for success
gh run watch <RUN_ID>
```

Or just push a new tag and watch the full release pipeline.

---

## Rotation

Fine-grained tokens expire (1 year max). When expired, the tap update
CI job fails but the build and release still succeed.

Symptoms of an expired or misconfigured token:

| Error | Cause |
|-------|-------|
| `HTTP 401 Bad credentials` | Token expired or deleted |
| `HTTP 403 Resource not accessible by personal access token` | Token exists but wrong permissions or wrong repo selected |
| `pelagos-containers` not in Resource owner dropdown | Org token policy not enabled |

To rotate:
1. Create a new token (repeat Step 1)
2. Update the secret in all three repos (repeat Step 2)
3. Re-run the failed tap update job or wait for the next release

---

## Manual tap update (when CI is broken)

If the token is broken and you need the tap updated now:

```bash
# pelagos-ui cask example:
VERSION=0.1.6
DMG_URL="https://github.com/pelagos-containers/pelagos-ui/releases/download/v${VERSION}/Pelagos_${VERSION}_aarch64.dmg"
SHA256=$(curl -sL "$DMG_URL" | shasum -a 256 | awk '{print $1}')

# Clone the tap, edit the cask, push
git clone git@github.com:pelagos-containers/homebrew-tap.git
cd homebrew-tap
# Edit Casks/pelagos-ui.rb with new version + sha256
git add -A && git commit -m "chore: update pelagos-ui cask to v${VERSION}" && git push
```
