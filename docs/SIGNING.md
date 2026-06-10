# Code signing — removing the SmartScreen prompt

BowEcho's Windows builds are unsigned today, so first-time downloaders see
SmartScreen's "Windows protected your PC" prompt (bypass: More info → Run
anyway). Signing removes that. The release workflow is already scaffolded to
sign automatically the moment credentials exist as repo secrets — no YAML
editing needed later.

## Option A (recommended): Azure Trusted Signing
Cheapest (~$9.99/month) and modern; reputation builds quickly because
Microsoft vouches for the timestamped signature.

1. Create an Azure account, then a **Trusted Signing** resource
   (portal.azure.com → "Trusted Signing accounts"). Pick the *Public Trust*
   profile; identity validation (individual or org) takes a few days.
2. Create an App Registration (Entra ID) with a client secret; grant it the
   *Trusted Signing Certificate Profile Signer* role on the account.
3. Add these GitHub repo secrets (Settings → Secrets and variables →
   Actions) on `FahrenheitResearch/bowecho`:
   - `AZURE_TENANT_ID`
   - `AZURE_CLIENT_ID`
   - `AZURE_CLIENT_SECRET`
   - `AZURE_TS_ENDPOINT` (e.g. `https://eus.codesigning.azure.net`)
   - `AZURE_TS_ACCOUNT` (account name)
   - `AZURE_TS_PROFILE` (certificate profile name)
4. Done — the next tag push signs `bowecho.exe` on both Windows targets.
   The workflow step is a no-op while the secrets are absent.

## Option B: classic OV certificate (Sectigo/SSL.com, ~$70–200/yr)
Buy an OV code-signing cert (ships on a USB HSM token per 2023 CA rules,
which does NOT work in CI), or choose a vendor offering **cloud signing**
(SSL.com eSigner). For eSigner, set secrets `ESIGNER_USERNAME`,
`ESIGNER_PASSWORD`, `ESIGNER_TOTP_SECRET` and ask to extend the workflow.
Reputation with OV builds slowly (downloads accumulate); EV grants instant
reputation but costs more.

## macOS — Apple Developer ID ($99/yr)
The workflow step is already scaffolded (sign + notarize + staple), gated on
secrets — add them and the next tag ships Gatekeeper-clean .apps:

1. Join the Apple Developer Program (developer.apple.com, $99/yr).
2. In Xcode or developer.apple.com → Certificates, create a
   **Developer ID Application** certificate; export it (with its private
   key) from Keychain Access as a `.p12` with a password.
3. Create an **app-specific password** for your Apple ID
   (account.apple.com → Sign-In and Security → App-Specific Passwords).
4. Repo secrets on `FahrenheitResearch/bowecho` (CONFIGURED 2026-06-09):
   - `MACOS_CERTIFICATE_BASE64` — Developer ID Application .p12, base64
   - `MACOS_CERTIFICATE_PWD` — the .p12 export password
   - `ASC_API_KEY_BASE64` — App Store Connect API key (.p8), base64
   - `ASC_KEY_ID` / `ASC_ISSUER_ID` — the key's ID and issuer
   The signing identity is discovered from the certificate automatically.
5. Active since v0.6.0: both macOS matrix builds sign, notarize
   (notarytool --wait via the ASC API key), and staple.

## Verifying a signed release
```powershell
Get-AuthenticodeSignature .\bowecho.exe | Format-List Status, SignerCertificate
```
Status should read `Valid` with the Fahrenheit Research subject.
