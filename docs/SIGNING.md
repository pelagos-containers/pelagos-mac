# Apple Developer ID Signing & Notarization

Checklist for signing `pelagos-mac` and `pelagos-ui` for public distribution.

---

## Credentials reference

| Item | Value |
|---|---|
| Apple ID | `skeptomai@mac.com` |
| Team ID | `8HHC296Z8Q` |
| Signing identity | `Developer ID Application: Christopher Brown (8HHC296Z8Q)` |
| App Store Connect Key ID | `U9KZ8M7HL9` |
| App Store Connect Issuer ID | 69a6de6e-96f9-47e3-e053-5b8c7c11a4d1 |
| .p8 key file | `~/.private_keys/AuthKey_U9KZ8M7HL9.p8` |

Move the .p8 to its canonical location (notarytool looks there by default):

```bash
mkdir -p ~/.private_keys
mv ~/Downloads/AuthKey_U9KZ8M7HL9.p8 ~/.private_keys/
```

---

## Step 1: Install the Developer ID certificate

If not already in Keychain:

```bash
# Double-click the downloaded .cer, or:
security import ~/Downloads/developerID_application.cer -k ~/Library/Keychains/login.keychain-db
```

Verify:

```bash
security find-identity -v -p codesigning | grep "Developer ID Application"
# Should print: Developer ID Application: Christopher Brown (8HHC296Z8Q)
```

---

## Step 2: Sign and notarize pelagos-mac (CLI binary)

```bash
cd /Users/christopherbrown/Projects/pelagos-mac
cargo build --release -p pelagos-mac
bash scripts/notarize.sh
bash scripts/build-release.sh
```

`notarize.sh` handles signing, submission to Apple, and `spctl` verification.
Standalone binaries in tarballs cannot be stapled — the notarization ticket lives
on Apple's servers and macOS checks it online on first run. This is fine for
Homebrew formula distribution.

---

## Step 3: Sign and notarize pelagos-ui (Tauri app)

```bash
cd /Users/christopherbrown/Projects/pelagos-ui

# Tauri auto-signs when this env var is set
APPLE_SIGNING_IDENTITY="Developer ID Application: Christopher Brown (8HHC296Z8Q)" \
  npm run tauri build

# Notarize the DMG
xcrun notarytool submit target/release/bundle/dmg/Pelagos_*.dmg \
  --key ~/.private_keys/AuthKey_U9KZ8M7HL9.p8 \
  --key-id U9KZ8M7HL9 \
  --issuer 69a6de6e-96f9-47e3-e053-5b8c7c11a4d1 \
  --wait

xcrun stapler staple target/release/bundle/dmg/Pelagos_*.dmg

# Verify Gatekeeper accepts it
spctl --assess --type open --context context:primary-signature \
  target/release/bundle/dmg/Pelagos_*.dmg
# Expected: accepted
```

---

## Step 4: Export certificate as .p12 for GitHub CI

1. Open Keychain Access → My Certificates
2. Right-click **Developer ID Application: Christopher Brown (8HHC296Z8Q)** → Export
3. Save as `developer-id.p12`, set a strong password
4. Base64-encode it:

```bash
base64 -i developer-id.p12 | pbcopy   # copies to clipboard
```

---

## Step 5: Add GitHub secrets

Add to both `pelagos-containers/pelagos-mac` and `pelagos-containers/pelagos-ui`:

| Secret name | Value |
|---|---|
| `APPLE_CERTIFICATE` | base64-encoded .p12 (from step 4) |
| `APPLE_CERTIFICATE_PASSWORD` | .p12 export password |
| `APPLE_TEAM_ID` | `8HHC296Z8Q` |
| `APPLE_API_KEY` | contents of `AuthKey_U9KZ8M7HL9.p8` |
| `APPLE_API_KEY_ID` | `U9KZ8M7HL9` |
| `APPLE_API_ISSUER_ID` | `69a6de6e-96f9-47e3-e053-5b8c7c11a4d1` |

```bash
for REPO in pelagos-containers/pelagos-mac pelagos-containers/pelagos-ui; do
    gh secret set APPLE_TEAM_ID        --body "8HHC296Z8Q"                           -R "$REPO"
    gh secret set APPLE_API_KEY_ID     --body "U9KZ8M7HL9"                           -R "$REPO"
    gh secret set APPLE_API_ISSUER_ID  --body "69a6de6e-96f9-47e3-e053-5b8c7c11a4d1" -R "$REPO"
    gh secret set APPLE_CERTIFICATE    < <(base64 -i developer-id.p12)               -R "$REPO"
    gh secret set APPLE_API_KEY        < ~/.private_keys/AuthKey_U9KZ8M7HL9.p8       -R "$REPO"
    # APPLE_CERTIFICATE_PASSWORD — enter interactively when prompted:
    gh secret set APPLE_CERTIFICATE_PASSWORD -R "$REPO"
done
```

`APPLE_API_KEY` is the raw .p8 file contents (multiline); pipe it directly rather than
copying to clipboard. `APPLE_CERTIFICATE_PASSWORD` is entered interactively so it never
touches shell history.

---

## Step 6: Wire CI (after local tests pass)

Both `release.yml` files have TODO stubs for signing (epic pelagos-containers/pelagos-mac#225).
Replace the TODO comment block with these steps:

```yaml
      - name: Import Developer ID certificate
        env:
          APPLE_CERTIFICATE: ${{ secrets.APPLE_CERTIFICATE }}
          APPLE_CERTIFICATE_PASSWORD: ${{ secrets.APPLE_CERTIFICATE_PASSWORD }}
        run: |
          KEYCHAIN=build.keychain
          security create-keychain -p "" "$KEYCHAIN"
          security default-keychain -s "$KEYCHAIN"
          security unlock-keychain -p "" "$KEYCHAIN"
          echo "$APPLE_CERTIFICATE" | base64 --decode > certificate.p12
          security import certificate.p12 -k "$KEYCHAIN" -P "$APPLE_CERTIFICATE_PASSWORD" \
            -T /usr/bin/codesign
          security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "" "$KEYCHAIN"
          rm certificate.p12

      - name: Sign and notarize
        env:
          APPLE_TEAM_ID: ${{ secrets.APPLE_TEAM_ID }}
          APPLE_API_KEY_ID: ${{ secrets.APPLE_API_KEY_ID }}
          APPLE_API_ISSUER_ID: ${{ secrets.APPLE_API_ISSUER_ID }}
          APPLE_API_KEY: ${{ secrets.APPLE_API_KEY }}
        run: |
          mkdir -p ~/.private_keys
          echo "$APPLE_API_KEY" > ~/.private_keys/AuthKey_${APPLE_API_KEY_ID}.p8
          bash scripts/notarize.sh
```

`build-release.sh` (called in the next existing step) detects the Developer ID
signature and skips the ad-hoc re-sign automatically.

---

## Troubleshooting

**`cannot read entitlement data`** — entitlements file missing. It lives at
`scripts/pelagos-mac.entitlements` in this repo.

**`spctl: rejected` with "does not seem to be an app"** — `spctl --assess --type exec`
is designed for `.app` bundles and is not a meaningful check for CLI binaries distributed
via Homebrew formula. Homebrew strips the quarantine xattr on install, so Gatekeeper
never gates the binary. `notarytool status: Accepted` is the authoritative success signal.

**notarytool `Invalid`** — binary not hardened runtime signed before submission.
Always sign before notarizing.

**Gatekeeper still blocks after staple** — quarantine xattr set on the file.
Remove it: `xattr -dr com.apple.quarantine <file>`
