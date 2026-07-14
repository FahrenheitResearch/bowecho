# Release signing and verification

This page separates what users can verify from the maintainer-only signing
setup. Checksums are always present. Tagged macOS releases additionally require
the Apple signing and notarization secrets; the release build fails without
them.

## Current release posture

- Release assets are built by GitHub Actions from the exact Git tag and are
  uploaded with matching `.sha256` files.
- Windows packages are unsigned unless the Azure Trusted Signing secrets are
  configured. Unsigned builds can trigger SmartScreen or antivirus
  machine-learning warnings on first download.
- Tagged macOS packages are signed with the BowEcho Developer ID, notarized,
  stapled, and validated by `codesign`, `stapler`, and Gatekeeper before they
  are zipped. Missing credentials or a failed validation fails the release
  build rather than publishing an unsigned Mac asset.
- Linux archives are not code-signed; use the `.sha256` file.

The source of truth for a release is the GitHub Actions run attached to that
tag. It shows whether each signing and validation step passed or failed.

## Release executable size guard

Every release-matrix job checks the **raw shipping executable**, after code
signing where applicable and before any ZIP, artifact, or release upload. The
limit is exactly **134,217,728 bytes (128 MiB)** for every Windows, Linux, and
macOS architecture. The guard intentionally does not inspect compressed
archive sizes, which vary by packager and can hide a large embedded payload.

The cap is generous relative to the approximately 68-71 MB v0.33 executables
and exists to catch accidental embedding of large lookup-table or scattering
bundles. Optional scientific resources must instead ship as versioned external
data packs that BowEcho downloads, validates, and caches at runtime. Do not
raise the cap to accommodate an embedded data bundle; changing it requires an
explicit release-policy decision and an updated documented baseline.

Maintainers can run the same check locally:

```sh
bash tools/check_release_binary_size.sh path/to/bowecho
bash tools/check_release_binary_size_test.sh
```

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

## In-app updater verification chains

The updater never runs without an explicit click. It only offers installation
for builds carrying an exact release-asset name baked in by the release
workflow. Local, development, and Linux builds without that name keep the
"Open releases" browser flow.

### Windows

Since v0.28.2 the Windows builds can install updates from Settings >
security & updates ("Install update"). They install nothing that fails any
link of this chain:

1. **Variant self-identification (build time).** The release workflow bakes
   the exact asset name each Windows binary ships as into that binary
   (`BOWECHO_UPDATE_ASSET`, e.g. `bowecho-windows-x64-v3.exe`). The updater
   can therefore only download the variant it is already running.
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

### macOS

The Intel and Apple Silicon release jobs bake their exact whole-app archive
names (`bowecho-macos-intel.zip` and
`bowecho-macos-apple-silicon.zip`) into BowEcho. Installation follows this
chain:

1. **Canonical repository and variant pin.** BowEcho accepts updater assets
   only from `https://github.com/FahrenheitResearch/bowecho` and downloads only
   the asset name baked into the running architecture's release build.
2. **HTTPS and SHA-256 gate.** The archive and its matching `.sha256` file are
   fetched from the selected GitHub release. The downloaded bytes must match
   the published SHA-256 before extraction; malformed or mismatched downloads
   are rejected.
3. **Private whole-bundle staging.** The archive must contain the expected
   complete `BowEcho.app` bundle and is extracted as that exact whole bundle,
   not as a replacement for only the executable. It is staged in a private
   directory beside the installed app and outside the currently running
   bundle, keeping the swap on one filesystem.
4. **Apple trust and publisher continuity.** Before installation,
   `codesign --verify --deep --strict` and Gatekeeper's `spctl` must accept the
   staged app. Its bundle Identifier and Developer ID `TeamIdentifier` must
   also match the currently running signed BowEcho bundle. A valid signature
   from another Apple developer is not accepted.
5. **Exit, swap, rollback, and relaunch.** BowEcho defers installation until
   the eframe window has exited. Before exit it starts the old, signed BowEcho
   binary in a private helper mode; the helper waits for parent-stdin EOF, then
   moves the installed bundle into the uniquely named, sentinel-owned private
   stage and moves the staged bundle to the original path. If the second rename
   fails, it restores the original bundle. On success it relaunches the app
   through `/usr/bin/open`. The private stage retains that last working app
   through the first launch; only a later explicit update attempt prunes it.

The updater cannot replace an App-Translocated or otherwise unwritable app.
Those cases, any trust/checksum failure, and a failed swap leave or restore the
installed app and fall back to a manual download from the releases page.

The first updater-capable macOS version is a one-time manual bootstrap:
v0.31.1 Mac builds do not contain a baked macOS asset name, so they cannot
discover an installable variant. Download the signed/notarized archive for the
Mac's architecture, quit BowEcho, replace `BowEcho.app` manually (normally in
`/Applications`), and launch it once. Later compatible releases can then use
"Install update."

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
Apple notarization, staples the ticket, and hard-gates the archive on
`codesign`, `stapler validate`, and `spctl`. All five secrets are mandatory for
a tagged release; if any is absent, both Mac matrix jobs fail before an asset
can be published.
