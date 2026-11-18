# Releases and Updates

Releases are published from `vX.Y.Z`, `vX.Y.Z-preview.N` or `vX.Y.Z-rc.N` tags.
The tag must match `workspace.package.version` in `Cargo.toml`. Preview and RC
releases belong to the Preview update channel and are marked as GitHub prereleases.
Stable accepts only stable versions; Preview accepts both, ordered by SemVer.
Switching channels never downgrades the application.

## Signing Configuration

Generate an Ed25519 key using OpenSSL 3 on a secure machine:

```sh
openssl genpkey -algorithm Ed25519 -out zp-update-key.pem
openssl pkey -in zp-update-key.pem -pubout -outform DER | tail -c 32 | openssl base64 -A
```

Store the PEM private key as the GitHub Actions secret `ZP_UPDATE_PRIVATE_KEY`.
Store the base64 raw public key as the Actions variable `ZP_UPDATE_PUBLIC_KEY`.
Only the public key is embedded in builds. Keep the private key backed up outside
the repository. Existing clients require this same key to verify later updates;
key rotation needs a separately designed migration.

The existing macOS secrets are also required: `MACOS_CERTIFICATE`,
`MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGN_IDENTITY`, `APPLE_ID`, `APPLE_TEAM_ID`,
and `APPLE_APP_PASSWORD`. The application ZIP and DMG are signed and notarized.
Keep the macOS bundle ID and Developer ID team consistent between releases.

## Publication

The workflow validates the tag, builds all platforms, and prepares a draft release
with generated notes. It then
generates `release.json`, signs its exact bytes as `release.json.sig`, uploads all
packages, and publishes the draft. The manifest contains the version, channel,
notes, platform and package type, filenames, sizes, URLs and SHA-256 hashes.

An incomplete draft stays hidden from update discovery. Before retrying a failed
publish job, inspect and remove its unpublished draft; published releases must
not be overwritten. Unpublished drafts never trigger client downloads.

## Client Behavior

The updater checks GitHub after startup and every six hours when enabled. Update
checks download metadata only. The title bar Update button and About settings
open a confirmation dialog before any package download. Downloads can be
cancelled, and packages and signed manifests are verified before installation.

macOS, portable Windows, AppImage and Linux archive updates use a helper after
the old process exits. The helper keeps the old application until the new one
acknowledges successful startup; replacement or startup failure restores it.
Windows installed applications use Inno Setup. Debian packages use the system
package installer rather than overwriting files owned by the package manager.
The Linux Debian installation is recognized at `/usr/bin/zp`. Its package version
uses Debian's `~` for previews (`0.3.0~rc.1`), but the asset keeps the release
version (`zp_0.3.0-rc.1_amd64.deb`) because GitHub renames `~` in asset names.

Development builds without `ZP_UPDATE_PUBLIC_KEY` can check for releases but
cannot install an unverified update. `ZP_UPDATE_FEED` or `updateFeed` can override
the Releases API endpoint for development; the signed manifest protocol is the
same. Use `updateChannel` (`stable` or `preview`) to persist the selected channel.
