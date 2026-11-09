# Code signing

How Oxbase releases are signed, and how to turn OS code signing on once there are certificates.
The public status is in the README ([Code signing](../README.md#code-signing)).

| Platform | Today | With a certificate |
|---|---|---|
| Windows | Unsigned: browser download warning and SmartScreen | `.msi`, NSIS `-setup.exe` and the app signed with the certificate (Authenticode, SHA-256, timestamped) |
| macOS | Ad-hoc signed: Gatekeeper blocks the first launch until **Open Anyway** in System Settings → Privacy & Security | Developer ID signed and notarized |
| Linux | No OS signing exists | Unchanged |
| All | Build provenance attestations, Sigstore-signed `SHA256SUMS.txt` | Unchanged |

`bundle.yml` signs when the secrets below exist and skips signing otherwise.
`scripts/ci-verify-signing.mjs` checks what was produced: a signature that is present but broken
always fails the job; a missing one is a warning until signing is required.

## Windows: a code signing certificate

Secrets (Settings → Secrets and variables → Actions):

| Secret | Value |
|---|---|
| `WINDOWS_CERTIFICATE` | the certificate with its private key, as a base64 `.pfx` |
| `WINDOWS_CERTIFICATE_PASSWORD` | the `.pfx` password |

The Windows job imports the `.pfx`, picks the certificate with a private key (preferring one with
the Code Signing usage) and passes its thumbprint to the Tauri bundler, which signs with SHA-256
and a DigiCert timestamp. Only the thumbprint leaves that step.

New code signing certificates must keep their key on a hardware token or in a cloud key
service, which a `.pfx` secret cannot hold. Such a service (Azure Trusted Signing, for example)
needs a change to `bundle.yml`: Tauri's `bundle.windows.signCommand` calling the service's
signing tool instead of the `.pfx` import.

## macOS: an Apple Developer ID

Needs the Apple Developer Program (paid, yearly).

| Secret | Value |
|---|---|
| `APPLE_CERTIFICATE` | the "Developer ID Application" certificate with its key, as a base64 `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | the `.p12` password |
| `APPLE_SIGNING_IDENTITY` | optional; Tauri derives it from the certificate |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | notarization: the Apple ID, an app-specific password and the team id |

With them, the macOS bundles are Developer ID signed and notarized, and the verify step checks
`codesign`, `spctl` and the stapled ticket.

## Requiring signing

Once the secrets exist, set the repository variable `OXBASE_REQUIRE_SIGNING=true` (or tick
`require_signing` on a manual Bundle or Release run). A release then fails before anything is
published when a bundle is unsigned, ad-hoc signed or not notarized, and its notes stop saying
the installers are unsigned. Update the README's [Code signing](../README.md#code-signing) status
at the same time.
