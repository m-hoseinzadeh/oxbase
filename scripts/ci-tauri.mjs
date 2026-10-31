#!/usr/bin/env node
// Runs the Tauri CLI for the Bundle workflow (used as tauri-action's `tauriScript`).
//
// The bundle job passes every signing secret to the build step through `env:` only
// (never $GITHUB_ENV). A secret that is not configured arrives as an empty string, and
// the Tauri bundler treats an empty APPLE_CERTIFICATE / TAURI_SIGNING_PRIVATE_KEY as
// "set". This wrapper therefore:
//   1. unsets every signing variable that is empty;
//   2. drops partial credential sets (certificate without password, incomplete
//      notarization credentials, notarization without a certificate);
//   3. on macOS without a certificate, ad-hoc signs (APPLE_SIGNING_IDENTITY="-");
//   4. with OXBASE_UI_PREBUILT=1, clears build.beforeBuildCommand so the frontend build
//      (vite and its npm plugin tree) does not run in the process that holds secrets.
//      The workflow builds ui/dist in an earlier step without secrets.
// It then runs the CLI in-process, pointed at crates/app-tauri and ui/.
//
// Usage: node scripts/ci-tauri.mjs <tauri args...>   e.g. `build --target x86_64-apple-darwin`
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const uiDir = join(repoRoot, 'ui');
const env = process.env;

const SIGNING_VARS = [
  'APPLE_CERTIFICATE',
  'APPLE_CERTIFICATE_PASSWORD',
  'APPLE_SIGNING_IDENTITY',
  'APPLE_ID',
  'APPLE_PASSWORD',
  'APPLE_TEAM_ID',
  'TAURI_SIGNING_PRIVATE_KEY',
  'TAURI_SIGNING_PRIVATE_KEY_PASSWORD',
];

const unset = (...names) => {
  for (const name of names) delete env[name];
};
const has = (name) => typeof env[name] === 'string' && env[name].length > 0;

for (const name of SIGNING_VARS) {
  if (!has(name)) unset(name);
}

const notes = [];

// macOS code signing: certificate and its password come as a pair.
if (has('APPLE_CERTIFICATE') !== has('APPLE_CERTIFICATE_PASSWORD')) {
  notes.push('APPLE_CERTIFICATE and APPLE_CERTIFICATE_PASSWORD must both be set; ignoring both');
  unset('APPLE_CERTIFICATE', 'APPLE_CERTIFICATE_PASSWORD');
}
const appleCert = has('APPLE_CERTIFICATE');

// macOS notarization: needs all three credentials and a real (non ad-hoc) certificate.
const notarizeVars = ['APPLE_ID', 'APPLE_PASSWORD', 'APPLE_TEAM_ID'];
const notarizeCount = notarizeVars.filter(has).length;
if (notarizeCount > 0 && (notarizeCount < notarizeVars.length || !appleCert)) {
  notes.push(
    appleCert
      ? 'notarization needs APPLE_ID, APPLE_PASSWORD and APPLE_TEAM_ID; skipping notarization'
      : 'notarization needs a signing certificate; skipping notarization',
  );
  unset(...notarizeVars);
}

if (process.platform === 'darwin' && !appleCert) {
  // Without a certificate, an explicit identity cannot be used: ad-hoc sign instead.
  env.APPLE_SIGNING_IDENTITY = '-';
}

// Updater signatures: a password without a key is meaningless.
if (!has('TAURI_SIGNING_PRIVATE_KEY')) unset('TAURI_SIGNING_PRIVATE_KEY_PASSWORD');

for (const note of notes) console.log(`ci-tauri: ${note}`);
console.log(
  `ci-tauri: macOS signing=${process.platform === 'darwin' ? (appleCert ? 'certificate' : 'ad-hoc') : 'n/a'}` +
    ` notarization=${has('APPLE_ID') ? 'on' : 'off'}` +
    ` updater-signing=${has('TAURI_SIGNING_PRIVATE_KEY') ? 'on' : 'off'}`,
);

env.TAURI_APP_PATH ??= join(repoRoot, 'crates', 'app-tauri');
env.TAURI_FRONTEND_PATH ??= uiDir;

const args = process.argv.slice(2);
if (env.OXBASE_UI_PREBUILT === '1' && args[0] === 'build') {
  // Merged into tauri.conf.json after any other --config (later patches win); an empty
  // hook command is skipped by the CLI. Inserted before `--`, which starts runner args.
  const end = args.indexOf('--');
  args.splice(end === -1 ? args.length : end, 0, '--config', JSON.stringify({ build: { beforeBuildCommand: '' } }));
  console.log('ci-tauri: OXBASE_UI_PREBUILT=1, skipping build.beforeBuildCommand');
}

// Resolve the CLI that ui/ installed (pinned by ui/pnpm-lock.yaml).
const requireFromUi = createRequire(join(uiDir, 'package.json'));
const { run, logError } = await import(pathToFileURL(requireFromUi.resolve('@tauri-apps/cli')).href);

try {
  await run(args, 'node scripts/ci-tauri.mjs');
} catch (err) {
  logError(err?.message ?? String(err));
  process.exit(1);
}
