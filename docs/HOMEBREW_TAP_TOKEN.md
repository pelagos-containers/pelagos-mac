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
classic personal access token (PAT) with `public_repo` scope.

---

## Why classic tokens, not fine-grained

As of May 2026, fine-grained personal access tokens **cannot write to
organization repositories** via the GitHub Contents API, even when:

- The token has Contents: Read and Write permission
- The token is scoped to the correct repository
- The token owner is an org admin with full push access
- The repo reports `push: true` in the permissions response
- The org has fine-grained tokens enabled with no approval required

The API returns `HTTP 403 Resource not accessible by personal access
token` on any write operation (PUT via Contents API, POST via Git Data
API, and git push over HTTPS).

This is a known, widespread, unresolved issue in the GitHub community:

- [Discussion #106661](https://github.com/orgs/community/discussions/106661) -- fine-grained PAT cannot create pull requests (unresolved)
- [Discussion #89800](https://github.com/orgs/community/discussions/89800) -- resource not accessible for fine-grained token (unresolved)
- [Discussion #40910](https://github.com/orgs/community/discussions/40910) -- unable to access organization repo with fine-grained token
- [Discussion #171513](https://github.com/orgs/community/discussions/171513) -- fine-grained tokens not working consistently (unresolved)

Classic tokens with `public_repo` scope work correctly for this use
case. They are coarser-grained (access to all public repos, not just
homebrew-tap) but they are the only option that works.

If GitHub fixes fine-grained tokens for org repos in the future, this
doc should be updated to use them instead.

---

## Step 1: Create the token

Go to: https://github.com/settings/tokens/new

This is the **classic** token page (not fine-grained).

Fill in EXACTLY these values:

| Field | Value |
|-------|-------|
| **Note** | `homebrew-tap-updater` |
| **Expiration** | Custom -> 1 year from today (set a calendar reminder) |

Under **Select scopes**, check ONLY:

- `public_repo` (under the `repo` section)

Do NOT check the full `repo` scope -- only `public_repo`. This grants
read/write access to public repositories, which is all that's needed
since homebrew-tap is public.

Click **Generate token**. Copy the token value immediately -- you cannot
see it again.

---

## Step 2: Distribute the token to all repos

Run the setup script:

```bash
bash scripts/setup-tap-token.sh
```

Paste the token when prompted. The script sets `TAP_GITHUB_TOKEN` in:
- pelagos-containers/pelagos
- pelagos-containers/pelagos-mac
- pelagos-containers/pelagos-ui

---

## Step 3: Verify

Test the token before relying on CI:

```bash
bash scripts/test-tap-token.sh <TOKEN>
```

This tests both read and write access to the homebrew-tap repo. If both
pass, the token is correctly configured.

---

## Rotation

Classic tokens expire (max 1 year with expiration, or no expiration).
When expired, the tap update CI job fails but the build and release
itself still succeeds.

Symptoms:

| Error | Cause |
|-------|-------|
| `HTTP 401 Bad credentials` | Token expired or deleted |
| `HTTP 403 Resource not accessible` | Wrong token type (see "Why classic tokens" above) |

To rotate:
1. Create a new classic token (repeat Step 1)
2. Run `bash scripts/setup-tap-token.sh` (repeat Step 2)
3. Run `bash scripts/test-tap-token.sh <TOKEN>` to verify
4. Re-run failed tap update jobs or wait for next release

---

## Manual tap update (when CI is broken)

If the token is broken and you need the tap updated immediately:

```bash
VERSION=0.1.6
# Clone the tap repo (uses your SSH key, not the PAT)
git clone git@github.com:pelagos-containers/homebrew-tap.git
cd homebrew-tap
# Edit the relevant file (Formula/pelagos.rb, Casks/pelagos-ui.rb, etc.)
# Update version and sha256
git add -A && git commit -m "chore: update to v${VERSION}" && git push
```

---

## Cleanup: delete old fine-grained tokens

If you previously created fine-grained tokens for this purpose, delete
them at https://github.com/settings/personal-access-tokens to avoid
confusion. They do not work for org repo writes.
