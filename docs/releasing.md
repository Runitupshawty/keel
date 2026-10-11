# Releasing Keel

How a release is made today, and how its builds get signed. Signing is ready in CI but off until the certificates and keys exist as repository secrets: without them every step below still runs and the release is the same as before, unsigned, with `SHA256SUMS` only.

- [The release procedure](#the-release-procedure)
- [What gets signed, and with which secrets](#what-gets-signed-and-with-which-secrets)
- [Windows: Authenticode](#windows-authenticode)
- [macOS: Developer ID and notarization](#macos-developer-id-and-notarization)
- [Checksums: GPG](#checksums-gpg)
- [Pinned downloads](#pinned-downloads)
- [Pinning the signer in the installers](#pinning-the-signer-in-the-installers)
- [Rotating a certificate or key](#rotating-a-certificate-or-key)
- [What cannot be tested without certificates](#what-cannot-be-tested-without-certificates)

## The release procedure

1. **Branch.** Each change is its own branch from `main` (one worktree per branch). Every commit passes `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and the tests of the crates it touches, and user-visible changes add a bullet under `## [Unreleased]` in [CHANGELOG.md](../CHANGELOG.md).
2. **Review.** Each branch is reviewed; findings are fixed as new commits on the branch.
3. **Fold.** The reviewed branches are merged into a release branch (`rel17` for 0.17.0). Run the full checks there (`cargo test --workspace` once, here), and the [QA walkthrough and screenshots](../CONTRIBUTING.md#qa-walkthrough) after UI changes.
4. **Release commit** (`release: X.Y.Z`) on the release branch:
   - CHANGELOG: `## [Unreleased]` becomes `## [X.Y.Z] - YYYY-MM-DD`.
   - `version` under `[workspace.package]` in `Cargo.toml`, and the `assemblyIdentity` version (`X.Y.Z.0`) in `crates/keel-app/keel.exe.manifest`.
   - The lock files: `cargo update -w` and `cargo update -w --manifest-path fuzz/Cargo.toml`.
5. **Merge** the release branch into `main` and wait for the `check` job on `main`.
6. **Tag** `main`: `git tag -a vX.Y.Z -m "Keel X.Y.Z"` and `git push origin vX.Y.Z`. A tag with a `-` (`v0.18.0-rc1`) publishes a pre-release.
7. **CI** (`.github/workflows/ci.yml`): `check` runs again, then `package` builds Windows x64, macOS arm64, macOS x64 (cross-built) and Linux x64 (plus the .deb), signing what it has secrets for, then `release` writes `SHA256SUMS`, signs it when it can and publishes the GitHub release with generated notes. The `release` job's summary has a **Signatures** list: what each package got, and whether `SHA256SUMS` is signed. Each skipped signing step also leaves a notice in the run's log.
8. **Install and smoke test** on each system, with the README's one-liners (and `sudo apt install ./keel_*.deb` on Debian or Ubuntu):
   - The installer prints `Checksum OK.`, and for a signed release `Signature OK.` (and `Code signature OK.` on macOS). An unsigned release prints a warning instead.
   - `keel --version` prints the version and a `signature:` line (`signed` once Windows and macOS builds are signed; `unsigned`, `unsigned (ad-hoc)` or `not signed (Linux)` otherwise).
   - The window opens; a folder lists; a PDF and an image preview; `keel daemon start` then `keel search <word>` answer.
   - On Windows, the update banner of the previous release installs the new one, and `install.ps1` next to `keel.exe` is then the new release's.
   - The release page lists the zip, three tarballs, the .deb and `SHA256SUMS` (with `SHA256SUMS.asc` and `SHA256SUMS.key.asc` once checksums are signed).

## What gets signed, and with which secrets

The secrets live in the repository's **Settings → Secrets and variables → Actions** (or `gh secret set NAME`). Every step checks its own secrets and is skipped, not failed, when any is missing.

| Where | What | Secrets | Without them |
| --- | --- | --- | --- |
| `package`, Windows | `keel.exe` and `keel-daemon.exe` (not the vendors' DLLs): `signtool sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com`, then `signtool verify /pa` | `WINDOWS_CERT_PFX_BASE64`, `WINDOWS_CERT_PASSWORD` | unsigned |
| `package`, macOS (both) | `keel`, `keel-daemon` and `libpdfium.dylib`: `codesign --options runtime --timestamp` with the Developer ID Application identity (the two programs with `assets/keel.entitlements`), then `notarytool submit --wait` | `APPLE_CERT_P12_BASE64`, `APPLE_CERT_PASSWORD`, `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD` | unsigned (the linker's ad-hoc signature only) |
| `release` | `SHA256SUMS`, which lists every archive and the .deb: `gpg --armor --detach-sign` gives `SHA256SUMS.asc`; the public key is published as `SHA256SUMS.key.asc` | `GPG_PRIVATE_KEY`, `GPG_PASSPHRASE` (empty for a key without one) | no `.asc` |

Linux binaries and the .deb carry no signature of their own: `SHA256SUMS.asc` covers them through their checksum lines (apt verifies signatures of repositories, not of single .deb files).

The PFX, the .p12 and the GPG key exist on the runner only while their step runs: the PFX is written to the runner's temp folder and deleted, the .p12 goes into a temporary keychain that is deleted, the GPG key into a temporary `GNUPGHOME` that is deleted.

## Windows: Authenticode

**What signing buys.** A signed `keel.exe` names its publisher, and Windows can tell when a file was changed after signing. It does not, by itself, stop SmartScreen's "Windows protected your PC": SmartScreen goes by reputation, which a publisher builds over time as many people download and run its signed files without trouble. New certificates start with none, so the first signed releases can still be warned about. Extended Validation (EV) certificates used to get reputation at once; Microsoft has since changed how EV certificates are treated, so check Microsoft's current SmartScreen documentation before paying extra for EV. Azure Trusted Signing, Microsoft's own service, is the other route Microsoft recommends for reputation.

**Getting a certificate**, two options:

- **An OV code-signing certificate from a CA** (DigiCert, Sectigo, SSL.com, GlobalSign and others), issued after the CA verifies the publisher's identity (an individual or an organization). Since June 2023 the private key of every new code-signing certificate must stay in a hardware token or a cloud HSM, so a CA no longer hands out a key you can export as a `.pfx`. The `signtool /f <pfx>` step above therefore only works with a certificate whose key is exportable (an older certificate, or a self-signed one for testing). For a CA certificate with a cloud key, replace the step with the CA's signing tool (for example SSL.com eSigner's CodeSignTool, or DigiCert KeyLocker's `smctl`), which signs with credentials kept as secrets in the same way.
- **Azure Trusted Signing** (a monthly subscription, identity validation by Microsoft, no key to handle; the certificates last only days and renew themselves). In CI the Windows step becomes `azure/trusted-signing-action` with the account's endpoint, account name and certificate profile, authenticated by `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and `AZURE_CLIENT_SECRET`, signing the same two files with the same timestamp server. Because the certificate changes every few days, pin its **Subject** in `install.ps1`, not a thumbprint.

**Setting the secrets** for a PFX (PowerShell; delete the files afterwards):

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes('keel-signing.pfx')) | Set-Content -NoNewline pfx.b64
gh secret set WINDOWS_CERT_PFX_BASE64 < pfx.b64
gh secret set WINDOWS_CERT_PASSWORD          # prompts for the PFX password
Remove-Item pfx.b64
```

**Reading the signer** for the installer's pin, from a signed build:

```powershell
(Get-AuthenticodeSignature .\keel.exe).SignerCertificate | Format-List Subject, Thumbprint
```

## macOS: Developer ID and notarization

**What signing buys.** A downloaded app that is signed with a Developer ID and notarized by Apple opens on the first try: Gatekeeper checks the signature and asks Apple whether the files were notarized. Unsigned builds are blocked until the user clears the download quarantine or chooses **Open Anyway** (see [Gatekeeper](getting-started.md#macos)).

**Getting the certificate.** Join the Apple Developer Program (yearly fee). In Xcode (Settings → Accounts → Manage Certificates) or on developer.apple.com (Certificates → +, with a signing request from Keychain Access), create a **Developer ID Application** certificate. Export it with its private key from Keychain Access as a `.p12` with a password. For notarization, create an app-specific password for the Apple Account at account.apple.com (Sign-In and Security → App-Specific Passwords); the Team ID is on the developer account's Membership page.

```sh
base64 -i keel-developer-id.p12 | gh secret set APPLE_CERT_P12_BASE64
gh secret set APPLE_CERT_PASSWORD     # the .p12 password
gh secret set APPLE_ID                # the Apple Account's e-mail address
gh secret set APPLE_TEAM_ID           # 10 characters
gh secret set APPLE_APP_PASSWORD      # the app-specific password
```

**Notes.**

- The release ships bare binaries in a tarball, and only an app bundle, a disk image or an installer package can have its notarization ticket stapled. Gatekeeper therefore looks the ticket up online the first time a quarantined copy starts; the first start of a browser download needs the network.
- The hardened runtime (`--options runtime`, which notarization requires) only loads libraries signed by the same team, which is why `libpdfium.dylib` is signed too. A future library that Keel loads at run time and CI does not sign needs the `com.apple.security.cs.disable-library-validation` entitlement.
- The hardened runtime also refuses Apple Events (error -1743) to an app without the `com.apple.security.automation.apple-events` entitlement, even when a child process such as `osascript` sends them. `keel` and `keel-daemon` are signed with `assets/keel.entitlements`, which grants it. Keel's own Move to Trash sends none: on macOS it calls NSFileManager instead of asking Finder (`DeleteMethod::NsFileManager` in `crates/keel-vfs/src/local.rs`), so it needs neither the entitlement nor the Automation permission. Try both on a pre-release tag before the first signed release: Move to Trash, and that `codesign -d --entitlements - keel` lists the entitlement.
- When notarization fails or Apple refuses the files, the step prints Apple's answer and, when there is a submission id, `notarytool log` with the reasons, then fails the job.
- `install.sh` checks the code signature of `keel`, `keel-daemon` and `libpdfium.dylib` (`codesign --verify --strict`, and the Team ID when pinned). It removes the quarantine attribute only for unsigned builds; a signed build is left as it is. Downloads made by `curl` carry no quarantine anyway.

## Checksums: GPG

One signature covers every download: `SHA256SUMS.asc` signs `SHA256SUMS`, which lists the hash of each archive and of the .deb. Create a signing-only key for releases (not a personal key), with a project address as its user id:

```sh
gpg --quick-gen-key "Keel releases <releases@example.invalid>" ed25519 sign 2y
gpg --armor --export-secret-keys <FINGERPRINT> | gh secret set GPG_PRIVATE_KEY
gh secret set GPG_PASSPHRASE          # the key's passphrase
```

Keep a backup of the secret key offline; GitHub secrets cannot be read back. Put the fingerprint in `KEEL_GPG_FINGERPRINT` in `scripts/install.sh` and here:

Keel's release key: **none yet** (the first signed release adds its fingerprint here).

**Verifying a download by hand:**

```sh
gpg --import SHA256SUMS.key.asc                # check the fingerprint it prints against the line above
gpg --verify SHA256SUMS.asc SHA256SUMS         # "Good signature"
sha256sum --check --ignore-missing SHA256SUMS  # macOS: shasum -a 256 --check --ignore-missing SHA256SUMS
```

On Windows, `install.ps1` does not check `SHA256SUMS.asc` (Windows has no gpg by default); it checks the checksum and the Authenticode signature of the programs instead.

## Pinned downloads

`scripts/fetch-deps.sh` and `scripts/fetch-deps.ps1` download the libraries that ship next to Keel and that CI signs on macOS. Each download is refused unless its SHA-256 is the one written in the script:

| Download | Pinned to | Where |
| --- | --- | --- |
| pdfium (`bblanchon/pdfium-binaries`) | the release tag `chromium/7543` (`pdfium-render` 0.8.37's target) and one SHA-256 per asset: `pdfium-mac-arm64`, `pdfium-mac-x64`, `pdfium-linux-x64` | `fetch-deps.sh` |
| pdfium for Windows | the same tag, `pdfium-win-x64` | `fetch-deps.ps1` |
| Everything SDK (voidtools) | the SHA-256 of `Everything-SDK.zip`, which voidtools serves at one unversioned address | `fetch-deps.ps1` |

The hashes are GitHub's digests of the release assets, checked against a download. Moving to another pdfium tag is a deliberate edit of both scripts in one commit: the tag and all four hashes (download each asset of the new tag and run `sha256sum`, or read the digest on the release page). When voidtools replaces its SDK zip, `fetch-deps.ps1` fails until its new hash is checked and pinned the same way.

## Pinning the signer in the installers

Each installer has an empty constant for the signer it expects. While it is empty, the installer verifies whatever signature a release has, reports it and installs; an unsigned release gets a warning (and on Windows the SmartScreen hint). Once it is set, a release signed by anyone else is refused, and so is an unsigned one (unless `-SkipVerify` / `--skip-verify`).

| Installer | Constant | Value |
| --- | --- | --- |
| `scripts/install.ps1` | `$ExpectedSigner` | the certificate's thumbprint, or its exact Subject (Trusted Signing: the Subject) |
| `scripts/install.sh` | `KEEL_GPG_FINGERPRINT` | the release key's fingerprint, 40 hex digits, upper case, no spaces |
| `scripts/install.sh` | `KEEL_APPLE_TEAM_ID` | the Developer ID's Team ID |

Set a pin only after a release signed that way is out: the one-liners fetch the installers from `main`, so a pinned installer refuses older, unsigned releases. Installs that update through Keel's banner on Windows run the copy of `install.ps1` next to `keel.exe`. The Windows zip contains `install.ps1` (so `SHA256SUMS` covers it) and the installer keeps the zip's copy, so each banner update runs the installer of the release installed before it, and leaves the new release's installer, with its checks and pins, for the next one. An install of 0.17.0, whose zip had no installer, makes its update to 0.18.0 with its 0.17.0 script (no signature check) and has 0.18.0's from then on. A pin therefore reaches banner installs one release after it is set; the one-liner applies it at once.

## Rotating a certificate or key

- **Windows.** Upload the new PFX and password over the old secrets. When `$ExpectedSigner` holds a thumbprint, change it to the new one in the same commit that precedes the next tag (a Subject pin survives a renewal if the Subject stays the same). Releases signed with the old certificate stay valid: the timestamp proves they were signed while it was.
- **macOS.** A Developer ID Application certificate lasts five years. Export the new one as a `.p12` and replace `APPLE_CERT_P12_BASE64` and `APPLE_CERT_PASSWORD`; the Team ID, and so the pin, stays the same. A new app-specific password only replaces `APPLE_APP_PASSWORD`.
- **GPG.** Extending the expiry (`gpg --quick-set-expire <FINGERPRINT> 2y`) keeps the fingerprint: export the key again into `GPG_PRIVATE_KEY`. A new key needs `GPG_PRIVATE_KEY`, `GPG_PASSPHRASE`, `KEEL_GPG_FINGERPRINT` and the fingerprint on this page changed together, before the next tag. Older releases keep their old signatures.
- A leaked key or certificate: delete the secret first, so no release is signed with it, then revoke it (at the CA, on developer.apple.com, or with a GPG revocation certificate), and rotate as above.

## What cannot be tested without certificates

These run only when the secrets exist, so the first tagged release after adding them is their first real run; a pre-release tag (`vX.Y.Z-rc1`) is a cheap way to try them:

- `signtool` signing and verification, the PFX decoding and the timestamp server.
- The temporary keychain, `codesign` with the Developer ID identity and the entitlements, and notarization (Apple's answer, and its log when it refuses).
- GPG signing in the `release` job (the installer's side is tested with a throwaway key; see below).
- The installers against a real signed release: `Signature OK.` from `install.ps1`, `Code signature OK.` from `install.sh` on macOS, and `keel --version` printing `signature: signed`.

What is tested without them: `scripts/test-install.ps1` and `scripts/test-install.sh` (both run in CI's `build` job) check the checksum code, which installer copy `install.ps1` keeps next to the app, the Authenticode checks against Windows' own signed files (valid, pinned by thumbprint and by Subject, another signer, partly signed, tampered), the GPG check with a fake `gpg` and with a real one on a throwaway key, and the macOS code-signature check with a fake `codesign`. The Rust tests check the Mach-O signature reader on synthetic files, WinVerifyTrust on a signed Windows driver, and that `keel --version` and crash reports report a test build as unsigned.
