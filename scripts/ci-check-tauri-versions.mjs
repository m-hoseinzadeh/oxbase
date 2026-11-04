#!/usr/bin/env node
// Fails when the Tauri Rust crates and their npm counterparts are on different
// major.minor releases. `tauri build` refuses to run in that state, but CI only runs
// `tauri build` in the Bundle workflow (after merge). This check runs on every PR so a
// one-sided dependency bump (e.g. a Dependabot cargo PR without the matching npm PR)
// is caught before it lands.
//
// Pairs checked (the same ones the Tauri CLI checks):
//   tauri                 <-> @tauri-apps/api
//   tauri-plugin-<name>   <-> @tauri-apps/plugin-<name>
// Rust versions come from Cargo.lock; npm versions from the installed packages under
// ui/node_modules, so run it after `pnpm install`.
//
// Also: the custom NSIS template must come from the installed @tauri-apps/cli release.
//
// Usage: node scripts/ci-check-tauri-versions.mjs [Cargo.lock] [ui dir]
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const lockPath = resolve(repoRoot, process.argv[2] ?? 'Cargo.lock');
const uiDir = resolve(repoRoot, process.argv[3] ?? 'ui');

if (!existsSync(lockPath)) {
  console.error(`ci-check-tauri-versions: ${lockPath} not found`);
  process.exit(1);
}
if (!existsSync(join(uiDir, 'node_modules'))) {
  console.error(`ci-check-tauri-versions: ${uiDir}/node_modules not found; run pnpm install first`);
  process.exit(1);
}

// Minimal Cargo.lock reader: every [[package]] block has `name` and `version` lines.
const crates = new Map(); // name -> Set(version)
for (const block of readFileSync(lockPath, 'utf8').split(/^\[\[package\]\]\s*$/m)) {
  const name = /^name = "([^"]+)"$/m.exec(block)?.[1];
  const version = /^version = "([^"]+)"$/m.exec(block)?.[1];
  if (!name || !version) continue;
  if (name !== 'tauri' && !name.startsWith('tauri-plugin-')) continue;
  if (!crates.has(name)) crates.set(name, new Set());
  crates.get(name).add(version);
}

const npmNameFor = (crate) =>
  crate === 'tauri' ? '@tauri-apps/api' : `@tauri-apps/plugin-${crate.slice('tauri-plugin-'.length)}`;

const majorMinor = (v) => {
  const m = /^(\d+)\.(\d+)\./.exec(v);
  return m ? `${m[1]}.${m[2]}` : null;
};

let checked = 0;
const mismatches = [];
for (const [crate, versions] of [...crates].sort(([a], [b]) => a.localeCompare(b))) {
  const npmName = npmNameFor(crate);
  const pkgJson = join(uiDir, 'node_modules', ...npmName.split('/'), 'package.json');
  if (!existsSync(pkgJson)) continue; // Rust-only plugin or no frontend binding installed.
  const npmVersion = JSON.parse(readFileSync(pkgJson, 'utf8')).version;
  for (const crateVersion of versions) {
    checked += 1;
    const ok = majorMinor(crateVersion) !== null && majorMinor(crateVersion) === majorMinor(npmVersion);
    console.log(`${ok ? 'ok      ' : 'MISMATCH'} ${crate} ${crateVersion} <-> ${npmName} ${npmVersion}`);
    if (!ok) mismatches.push(crate);
  }
}

if (crates.has('tauri') && checked === 0) {
  console.error('ci-check-tauri-versions: tauri is in Cargo.lock but @tauri-apps/api is not installed in ui/');
  process.exit(1);
}
// The custom NSIS template (crates/app-tauri/windows/installer.nsi) is a copy of the bundler's
// template of one Tauri CLI release; the bundler fills it with that release's placeholders, so a
// CLI upgrade must re-copy it (its header says how).
const templatePath = join(repoRoot, 'crates', 'app-tauri', 'windows', 'installer.nsi');
const cliPkg = join(uiDir, 'node_modules', '@tauri-apps', 'cli', 'package.json');
if (existsSync(templatePath)) {
  const from = /tauri-cli-v(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)/.exec(readFileSync(templatePath, 'utf8'))?.[1];
  const cli = existsSync(cliPkg) ? JSON.parse(readFileSync(cliPkg, 'utf8')).version : null;
  const ok = from !== undefined && from === cli;
  console.log(`${ok ? 'ok      ' : 'MISMATCH'} NSIS template from tauri-cli ${from ?? '?'} <-> @tauri-apps/cli ${cli ?? '(not installed)'}`);
  if (!ok) {
    console.error(
      'ci-check-tauri-versions: crates/app-tauri/windows/installer.nsi was copied from another ' +
        'Tauri CLI release; copy the new upstream template and re-apply the Oxbase change.',
    );
    process.exit(1);
  }
}

if (mismatches.length > 0) {
  console.error(
    'ci-check-tauri-versions: Tauri crates and npm packages must share major.minor; ' +
      'bump both sides together.',
  );
  process.exit(1);
}
console.log(`ci-check-tauri-versions: ${checked} pair(s) checked, all consistent`);
