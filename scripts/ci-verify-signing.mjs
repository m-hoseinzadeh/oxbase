#!/usr/bin/env node
// Checks the code signatures of the Tauri bundles built by the Bundle workflow.
//
// A green `tauri build` does not prove anything is signed: without secrets, ci-tauri.mjs
// ad-hoc signs on macOS and leaves Windows unsigned, and the build still succeeds. This
// script inspects what was actually produced.
//
//   node scripts/ci-verify-signing.mjs --preflight   before the build: fail fast when signing
//                                                    is required but its secrets are missing
//   node scripts/ci-verify-signing.mjs               after the build: verify every bundle
//
// Environment (no secrets; only whether they are set):
//   OXBASE_REQUIRE_SIGNING     "true": unsigned / ad-hoc / unnotarized bundles fail the job.
//                           Otherwise they are reported as warnings.
//   HAS_APPLE_CERTIFICATE   "true" when APPLE_CERTIFICATE (+ password) is configured
//   HAS_APPLE_NOTARIZATION  "true" when APPLE_ID, APPLE_PASSWORD and APPLE_TEAM_ID are set
//   HAS_WINDOWS_CERTIFICATE "true" when WINDOWS_CERTIFICATE is configured
//   OXBASE_TARGET_DIR          cargo target dir to search (default: <repo>/target)
//
// A signature that is present but broken always fails, whatever OXBASE_REQUIRE_SIGNING says.
// macOS:   codesign --verify --deep --strict on every .app (always); with a certificate, the
//          signature must not be ad-hoc; with notarization, `spctl -a -t exec` must accept the
//          app and `xcrun stapler validate` must find the stapled ticket.
// Windows: Get-AuthenticodeSignature on every .msi, NSIS setup .exe and the app .exe must
//          report Valid (and a timestamp) when a certificate is configured or required.
// Linux:   there is no OS code-signing to check; bundles and their SHA-256 are listed.
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { appendFileSync, existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const env = process.env;
const flag = (name) => env[name] === 'true';

const required = flag('OXBASE_REQUIRE_SIGNING');
const appleCert = flag('HAS_APPLE_CERTIFICATE');
const appleNotarize = flag('HAS_APPLE_NOTARIZATION');
const windowsCert = flag('HAS_WINDOWS_CERTIFICATE');
const platform = process.platform;

const rows = []; // [artifact, check, result]
let failures = 0;
let warnings = 0;

function fail(artifact, check, detail) {
  failures += 1;
  rows.push([artifact, check, `FAIL: ${detail}`]);
  console.log(`::error title=signing::${artifact}: ${check}: ${detail}`);
}
function warn(artifact, check, detail) {
  warnings += 1;
  rows.push([artifact, check, `warning: ${detail}`]);
  console.log(`::warning title=signing::${artifact}: ${check}: ${detail}`);
}
function ok(artifact, check, detail = 'ok') {
  rows.push([artifact, check, detail]);
  console.log(`ok   ${artifact}: ${check}: ${detail}`);
}
/** Required-or-warn: a missing signature fails only when signing is required. */
function missing(artifact, check, detail) {
  (required ? fail : warn)(artifact, check, detail);
}

function run(cmd, args, extraEnv = {}) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', env: { ...env, ...extraEnv } });
  const out = `${r.stdout ?? ''}${r.stderr ?? ''}`.trim();
  if (r.error) return { ok: false, out: String(r.error.message) };
  return { ok: r.status === 0, out };
}

function writeSummary(title) {
  const lines = [`### ${title}`, '', '| Artifact | Check | Result |', '|---|---|---|'];
  for (const [a, c, r] of rows) lines.push(`| ${a} | ${c} | ${r.replaceAll('|', '\\|').replaceAll('\n', ' ')} |`);
  lines.push('', `Signing required: ${required}. Failures: ${failures}. Warnings: ${warnings}.`, '');
  if (env.GITHUB_STEP_SUMMARY) appendFileSync(env.GITHUB_STEP_SUMMARY, `${lines.join('\n')}\n`);
}

// ------------------------------------------------------------------ preflight

if (process.argv.includes('--preflight')) {
  console.log(
    `signing preflight: platform=${platform} required=${required} apple-cert=${appleCert}` +
      ` apple-notarization=${appleNotarize} windows-cert=${windowsCert}`,
  );
  if (platform === 'darwin') {
    if (!appleCert) missing('macOS', 'secrets', 'APPLE_CERTIFICATE / APPLE_CERTIFICATE_PASSWORD not set (bundle will be ad-hoc signed)');
    if (!appleNotarize) missing('macOS', 'secrets', 'APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID not set (bundle will not be notarized)');
  } else if (platform === 'win32') {
    if (!windowsCert) missing('Windows', 'secrets', 'WINDOWS_CERTIFICATE / WINDOWS_CERTIFICATE_PASSWORD not set (bundle will be unsigned)');
  }
  if (failures === 0) ok(platform, 'secrets', 'preflight passed');
  writeSummary('Signing preflight');
  process.exit(failures > 0 ? 1 : 0);
}

// ------------------------------------------------------------------ find bundles

const targetDir = resolve(env.OXBASE_TARGET_DIR ?? join(repoRoot, 'target'));

/** `<target>/{,<triple>/}{release,debug}/bundle` directories. */
function bundleDirs() {
  const out = [];
  const consider = (base) => {
    for (const profile of ['release', 'debug']) {
      const dir = join(base, profile, 'bundle');
      if (existsSync(dir) && statSync(dir).isDirectory()) out.push(dir);
    }
  };
  if (!existsSync(targetDir)) return out;
  consider(targetDir);
  for (const entry of readdirSync(targetDir, { withFileTypes: true })) {
    if (entry.isDirectory() && entry.name.includes('-')) consider(join(targetDir, entry.name));
  }
  return out;
}

/** Entries of `dir/sub` whose names end with one of `exts`. */
function entries(dir, sub, exts) {
  const d = join(dir, sub);
  if (!existsSync(d)) return [];
  return readdirSync(d)
    .filter((n) => exts.some((e) => n.toLowerCase().endsWith(e)))
    .map((n) => join(d, n));
}

const rel = (p) => (p.startsWith(repoRoot) ? p.slice(repoRoot.length + 1) : p).replaceAll('\\', '/');

const dirs = bundleDirs();
if (dirs.length === 0) {
  fail(rel(targetDir) || targetDir, 'bundles', 'no <target>/[<triple>/]{release,debug}/bundle directory found');
  writeSummary('Bundle signatures');
  process.exit(1);
}

// ------------------------------------------------------------------ macOS

function verifyMac(dir) {
  const apps = entries(dir, 'macos', ['.app']);
  if (apps.length === 0) fail(rel(dir), 'bundles', 'no .app under bundle/macos');
  for (const app of apps) {
    const name = rel(app);
    const verify = run('codesign', ['--verify', '--deep', '--strict', '--verbose=2', app]);
    if (!verify.ok) {
      fail(name, 'codesign --verify --deep --strict', verify.out);
      continue;
    }
    const info = run('codesign', ['-dv', '--verbose=4', app]).out;
    const adhoc = /Signature=adhoc/.test(info);
    const authority = (info.match(/^Authority=(.*)$/m) ?? [])[1] ?? 'none';
    const team = (info.match(/^TeamIdentifier=(.*)$/m) ?? [])[1] ?? 'none';
    if (adhoc) {
      (appleCert ? fail : missing)(name, 'signing identity', 'ad-hoc signature (no Developer ID certificate)');
    } else {
      ok(name, 'signing identity', `${authority} (team ${team})`);
    }

    if (appleNotarize && !adhoc) {
      const spctl = run('spctl', ['-a', '-t', 'exec', '-vv', app]);
      if (spctl.ok) ok(name, 'spctl -a -t exec', spctl.out.split('\n').slice(1).join('; ') || 'accepted');
      else fail(name, 'spctl -a -t exec', spctl.out);
      const staple = run('xcrun', ['stapler', 'validate', app]);
      if (staple.ok) ok(name, 'stapler validate', 'ticket stapled');
      else fail(name, 'stapler validate', staple.out);
    } else {
      missing(name, 'notarization', 'not notarized (Gatekeeper will block it on other Macs)');
    }
  }
  for (const dmg of entries(dir, 'dmg', ['.dmg'])) {
    const r = run('codesign', ['-dv', dmg]);
    ok(rel(dmg), 'dmg', r.ok ? 'signed' : 'no signature on the disk image (the .app inside is what Gatekeeper checks)');
  }
}

// ------------------------------------------------------------------ Windows

const PS_CHECK = [
  "$ErrorActionPreference = 'Stop'",
  '$s = Get-AuthenticodeSignature -LiteralPath $env:OXBASE_SIG_FILE',
  '[pscustomobject]@{',
  '  Status = [string]$s.Status',
  '  Message = [string]$s.StatusMessage',
  "  Signer = if ($s.SignerCertificate) { $s.SignerCertificate.Subject } else { '' }",
  "  Timestamp = if ($s.TimeStamperCertificate) { $s.TimeStamperCertificate.Subject } else { '' }",
  '} | ConvertTo-Json -Compress',
].join('\n');

function verifyWindowsFile(file) {
  const name = rel(file);
  const r = run('pwsh', ['-NoProfile', '-NonInteractive', '-Command', PS_CHECK], { OXBASE_SIG_FILE: file });
  if (!r.ok) {
    fail(name, 'Get-AuthenticodeSignature', r.out);
    return;
  }
  let sig;
  try {
    sig = JSON.parse(r.out.split('\n').pop());
  } catch {
    fail(name, 'Get-AuthenticodeSignature', `unparsable output: ${r.out}`);
    return;
  }
  if (sig.Status === 'Valid') {
    ok(name, 'Authenticode', `Valid, signer ${sig.Signer}`);
    if (sig.Timestamp) ok(name, 'timestamp', sig.Timestamp);
    else (required ? fail : warn)(name, 'timestamp', 'not timestamped (signature expires with the certificate)');
  } else if (sig.Status === 'NotSigned') {
    (windowsCert ? fail : missing)(name, 'Authenticode', 'not signed');
  } else {
    fail(name, 'Authenticode', `${sig.Status}: ${sig.Message}`);
  }
}

function verifyWindows(dir) {
  const files = [...entries(dir, 'msi', ['.msi']), ...entries(dir, 'nsis', ['.exe'])];
  if (files.length === 0) fail(rel(dir), 'bundles', 'no .msi or NSIS .exe under bundle/');
  // The app executable itself sits next to bundle/ (named after productName or the crate).
  const profileDir = dirname(dir);
  for (const exe of ['Oxbase.exe', 'oxbase.exe', 'app-tauri.exe']) {
    const p = join(profileDir, exe);
    if (existsSync(p)) files.push(p);
  }
  for (const f of files) verifyWindowsFile(f);
}

// ------------------------------------------------------------------ Linux

function listLinux(dir) {
  const files = [
    ...entries(dir, 'appimage', ['.appimage']),
    ...entries(dir, 'deb', ['.deb']),
    ...entries(dir, 'rpm', ['.rpm']),
  ];
  if (files.length === 0) fail(rel(dir), 'bundles', 'no AppImage/deb/rpm under bundle/');
  for (const f of files) {
    const sha = createHash('sha256').update(readFileSync(f)).digest('hex');
    ok(rel(f), 'sha256', `${sha} (Linux has no OS code-signing check)`);
    if (existsSync(`${f}.sig`)) ok(rel(f), 'updater signature', `${basename(f)}.sig present`);
  }
}

for (const dir of dirs) {
  console.log(`checking ${rel(dir)}`);
  if (platform === 'darwin') verifyMac(dir);
  else if (platform === 'win32') verifyWindows(dir);
  else listLinux(dir);
}

writeSummary('Bundle signatures');
console.log(`signing check: ${failures} failure(s), ${warnings} warning(s), required=${required}`);
process.exit(failures > 0 ? 1 : 0);
