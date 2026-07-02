# Release signing and verification

This page separates what users can verify from the maintainer-only signing
setup. Checksums are always present. Platform code signing depends on the
corresponding repository secrets being configured for the tagged release run.

## Current release posture

- Release assets are built by GitHub Actions from the exact Git tag and are
  uploaded with matching `.sha256` files.
- Windows packages are unsigned unless the Azure Trusted Signing secrets are
  configured. Unsigned builds can trigger SmartScreen or antivirus
  machine-learning warnings on first download.
- macOS packages are signed, notarized, and stapled when the Apple Developer
  ID and App Store Connect secrets are configured. If those secrets are absent
  or a user downloads manually, macOS may still show a first-run warning.
- Linux archives are not code-signed; use the `.sha256` file.

The source of truth for a release is the GitHub Actions run attached to that
tag. It shows whether the signing steps ran, skipped, or failed.

## User verification

Compare the downloaded asset with the matching `.sha256` file on the release
page.

Windows PowerShell:

```powershell
Get-FileHash .\bowecho-windows-x64.exe -Algorithm SHA256
Get-Content .\bowecho-windows-x64.exe.sha256
```

macOS or Linux:

```sh
shasum -a 256 bowecho-macos-apple-silicon.zip
sha256sum bowecho-linux-x64
```

If a Windows executable is signed, Authenticode should report `Valid`:

```powershell
Get-AuthenticodeSignature .\bowecho.exe | Format-List Status, SignerCertificate
```

If the build is unsigned, that command will report `NotSigned`; that is a
signing status, not proof of malware.

For macOS app bundles:

```sh
spctl --assess --type execute --verbose BowEcho.app
codesign --verify --deep --strict --verbose=2 BowEcho.app
```

## In-app updater verification chain (Windows)

Since v0.28.2 the Windows builds can install updates from Settings >
security & updates ("Install update"). The updater never runs without an
explicit click, and it installs nothing that fails any link of this chain:

1. **Variant self-identification (build time).** The release workflow bakes
   the exact asset name each Windows binary ships as into that binary
   (`BOWECHO_UPDATE_ASSET`, e.g. `bowecho-windows-x64-v3.exe`). The updater
   can therefore only download the variant it is already running. Dev/local
   builds and macOS/Linux builds have no baked name and never offer in-app
   install — they keep the "Open releases" browser button.
2. **Download over HTTPS (rustls).** The asset and its `.sha256` are fetched
   from the GitHub release for the new tag, streamed to a temp file in the
   same directory as the executable (same volume, so the final rename never
   crosses filesystems).
3. **Checksum gate.** The SHA-256 of the streamed bytes must match the
   `.sha256` file CI published beside the asset. Mismatch or a malformed
   checksum file deletes the download and reports the reason.
4. **Authenticode gate.** `WinVerifyTrust` with the
   `WINTRUST_ACTION_GENERIC_VERIFY_V2` policy must report a valid embedded
   signature chaining to a trusted root — the same decision
   `Get-AuthenticodeSignature` makes. Revocation is not fetched over the
   network during this check (`WTD_REVOKE_NONE` plus cache-only URL
   retrieval): the checksum pin against the release page is the primary
   integrity gate, and a revocation-endpoint hiccup must not strand an
   update midway. Any rejection deletes the download.
5. **Atomic-ish swap and restart.** The running `bowecho.exe` is renamed to
   `bowecho.exe.old` (renaming a running executable is legal on Windows),
   the verified file is renamed into its place, and the app restarts itself
   with the same arguments after a clean shutdown. If the second rename
   fails, the original binary is renamed back. Leftover `.old` files are
   cleaned up best-effort on later launches.

Known limitation: the Authenticode gate verifies *a* trusted signature, not
a pinned publisher identity. Pinning the expected signer is a candidate
hardening if the signing identity ever stabilizes long-term.

## Windows signing setup

Azure Trusted Signing is the preferred CI-friendly path.

1. Create an Azure Trusted Signing account with a Public Trust certificate
   profile. Identity validation can take several days.
2. Create a Microsoft Entra app registration with a client secret.
3. Grant that app the `Trusted Signing Certificate Profile Signer` role on the
   Trusted Signing account/profile.
4. Add the GitHub Actions secrets consumed by the release workflow for the
   Azure tenant, client identity, client credential, Trusted Signing endpoint,
   account, and certificate profile. The workflow file is the source of truth
   for exact secret names.

The release workflow is already scaffolded to sign Windows binaries when those
secrets exist. Without them, the signing step is skipped.

Classic OV/EV code-signing certificates can also work, but physical USB token
certificates are not practical for GitHub Actions. Use a cloud-signing vendor
if this path is chosen.

## macOS signing setup

1. Join the Apple Developer Program.
2. Create a Developer ID Application certificate and export it from Keychain
   Access as a password-protected `.p12`.
3. Create an App Store Connect API key for notarization.
4. Add the GitHub Actions secrets consumed by the release workflow for the
   Developer ID certificate, certificate password, App Store Connect API key,
   key id, and issuer id. The workflow file is the source of truth for exact
   secret names.

The release workflow imports the certificate, signs the `.app`, submits it to
Apple notarization, staples the ticket, and then zips the app when those
secrets are present.
