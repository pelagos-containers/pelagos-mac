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

## Creating the token

1. Go to https://github.com/settings/tokens?type=beta (Fine-grained tokens)

2. Click **Generate new token**

3. Configure:
   - **Name**: `homebrew-tap-updater`
   - **Expiration**: 1 year (set a calendar reminder to rotate)
   - **Resource owner**: `pelagos-containers`
   - **Repository access**: Only select repositories -> `pelagos-containers/homebrew-tap`
   - **Permissions**: Repository permissions -> **Contents: Read and write**
   - No other permissions needed

4. Click **Generate token** and copy the value

## Adding the secret to each repo

The same token value is added to three repos. Go to each repo's
Settings -> Secrets and variables -> Actions -> New repository secret:

- https://github.com/pelagos-containers/pelagos/settings/secrets/actions
- https://github.com/pelagos-containers/pelagos-mac/settings/secrets/actions
- https://github.com/pelagos-containers/pelagos-ui/settings/secrets/actions

For each:
- **Name**: `TAP_GITHUB_TOKEN`
- **Value**: paste the token from step 4

## Rotation

Fine-grained tokens expire (max 1 year). When the token expires, the
tap update step in release CI will fail with `HTTP 401 Bad credentials`.
The build and release itself still succeeds -- only the Homebrew tap
update is affected.

To rotate:
1. Create a new token (same steps as above)
2. Update the secret in all three repos
3. Optionally re-run the failed tap update job, or just let the next
   release pick it up

## Diagnosing failures

If a release CI run shows the tap update job failed:

```
gh: Bad credentials (HTTP 401)
```

The token is expired or missing. Follow the rotation steps above.

To manually update the tap in the meantime:

```bash
# Example for pelagos-ui cask:
VERSION=0.1.6
DMG_URL="https://github.com/pelagos-containers/pelagos-ui/releases/download/v${VERSION}/Pelagos_${VERSION}_aarch64.dmg"
SHA256=$(curl -sL "$DMG_URL" | shasum -a 256 | awk '{print $1}')
# Then edit Casks/pelagos-ui.rb in the homebrew-tap repo with the new version + sha256
```
