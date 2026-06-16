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
Get-FileHash .\bowecho-windows-x64.zip -Algorithm SHA256
Get-Content .\bowecho-windows-x64.zip.sha256
```

macOS or Linux:

```sh
shasum -a 256 bowecho-macos-apple-silicon.zip
sha256sum bowecho-linux-x64.tar.gz
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

## Windows signing setup

Azure Trusted Signing is the preferred CI-friendly path.

1. Create an Azure Trusted Signing account with a Public Trust certificate
   profile. Identity validation can take several days.
2. Create a Microsoft Entra app registration with a client secret.
3. Grant that app the `Trusted Signing Certificate Profile Signer` role on the
   Trusted Signing account/profile.
4. Add these GitHub Actions secrets to `FahrenheitResearch/bowecho`:
   - `AZURE_TENANT_ID`
   - `AZURE_CLIENT_ID`
   - `AZURE_CLIENT_SECRET`
   - `AZURE_TS_ENDPOINT`, for example `https://eus.codesigning.azure.net`
   - `AZURE_TS_ACCOUNT`
   - `AZURE_TS_PROFILE`

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
4. Add these GitHub Actions secrets:
   - `MACOS_CERTIFICATE_BASE64`
   - `MACOS_CERTIFICATE_PWD`
   - `ASC_API_KEY_BASE64`
   - `ASC_KEY_ID`
   - `ASC_ISSUER_ID`

The release workflow imports the certificate, signs the `.app`, submits it to
Apple notarization, staples the ticket, and then zips the app when those
secrets are present.
