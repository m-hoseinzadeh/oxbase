# Code signing

How Oxbase releases are signed, and how to set up SignPath Foundation signing for Windows.
The public policy is in the README ([Code signing policy](../README.md#code-signing-policy)).

| Platform | Today | With this setup |
|---|---|---|
| Windows | Unsigned: browser download warning and SmartScreen | `.msi` (and the app inside it) and NSIS `-setup.exe` signed by SignPath Foundation |
| macOS | Ad-hoc signed: Gatekeeper blocks until right-click → Open | Unchanged (needs an Apple Developer ID; bundle.yml already supports `APPLE_*` secrets) |
| Linux | No OS signing exists | Unchanged |
| All | Build provenance attestations, Sigstore-signed `SHA256SUMS.txt` | Unchanged |

## How Windows signing works

1. `bundle.yml` builds the bundles as before (`tauri-action`).
2. With the SignPath secret and variables set, the Windows job uploads the `.msi` and the
   `-setup.exe` as one artifact and submits a signing request with
   `signpath/github-action-submit-signing-request` (artifact configuration
   [`installers`](../.signpath/artifact-configurations/installers.xml)).
3. SignPath checks that GitHub Actions built the artifact from this repository (origin
   verification), waits for your **manual approval**, signs, and returns the files: the `.msi` is
   deep-signed (the app `.exe` inside it, then the `.msi`), the NSIS installer is signed as a whole.
4. The job puts the signed files back in place and over the workflow artifacts `release.yml`
   publishes; `scripts/ci-verify-signing.mjs` then requires a valid Authenticode signature on
   both installers. The release notes say the Windows installers are signed.

Why not the app `.exe` before bundling: the Tauri bundler stamps the bundle type into the `.exe`
while it builds each installer, which would break an earlier signature. Inside the NSIS installer
the app stays unsigned (SignPath cannot open NSIS archives); Windows does not warn about an
installed program, only about the downloaded installer, which is signed.

The publisher shown by Windows is **SignPath Foundation**, not Oxbase. SmartScreen can still warn
for the first releases until the certificate has reputation for this download.

## Setup (maintainer)

### 1. Before applying

- Turn on two-factor authentication for your GitHub account (SignPath requires MFA for every
  team member, on GitHub and on SignPath).
- The README must have the **Code signing policy** section (it does) and the release pages link
  to it (release.yml does).
- Read the conditions: <https://signpath.org/terms>. One to raise in the application: Oxbase is
  developed as open core (a separate private repository may add paid features). This repository
  and everything SignPath would sign is Apache-2.0 only, but the conditions ask for an "OSI-approved
  Open Source license without commercial dual-licensing for all components", so say how the
  paid part is separated and let them decide.

### 2. Apply

At <https://signpath.org/apply>: project name Oxbase, repository
`https://github.com/m-hoseinzadeh/oxbase`, license Apache-2.0, build system GitHub Actions,
artifacts: Windows `.msi` and NSIS `.exe` installers, releases at
`https://github.com/m-hoseinzadeh/oxbase/releases`.

### 3. In SignPath, once accepted

1. Add the predefined trusted build system **GitHub.com** to the organization and link it to the
   project.
2. Create the artifact configuration with slug `installers` from
   [`.signpath/artifact-configurations/installers.xml`](../.signpath/artifact-configurations/installers.xml).
3. Signing policies: `release-signing` (the Foundation certificate, **Use approval process**,
   yourself as approver) and, if offered, `test-signing` (a test certificate, no approval).
4. Create an API token for a user with submitter rights on both policies.

### 4. In GitHub (Settings → Secrets and variables → Actions)

| Kind | Name | Value |
|---|---|---|
| Secret | `SIGNPATH_API_TOKEN` | the API token |
| Variable | `SIGNPATH_ORGANIZATION_ID` | the organization ID |
| Variable | `SIGNPATH_PROJECT_SLUG` | the project slug |
| Variable (optional) | `SIGNPATH_SIGNING_POLICY_SLUG` | default `release-signing` |
| Variable (optional) | `SIGNPATH_ARTIFACT_CONFIGURATION_SLUG` | default `installers` |

Without the secret and the two required variables, nothing changes: the SignPath steps are
skipped and the installers stay unsigned.

### 5. Test run

Set `SIGNPATH_SIGNING_POLICY_SLUG` to `test-signing` and run **Bundle** manually (Actions →
Bundle → Run workflow). The run confirms the artifact configuration (the path of `app-tauri.exe`
inside the `.msi`, the version format Tauri writes); fix `installers.xml` in SignPath if a
restriction does not match, and run again. Then set the variable back to `release-signing` (or
delete it).

### 6. Every release

Push the tag as usual. The Windows bundle job waits up to an hour for the signing request: approve
it in SignPath's web UI (the log of the step "SignPath - sign the installers" links to it; the job
summary keeps the link afterwards). A request that is not approved
in time fails the release before anything is published; approve it and re-run the failed job.
