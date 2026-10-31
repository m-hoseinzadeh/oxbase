#!/usr/bin/env node
// Makes sure the Tauri `build.frontendDist` directory exists so that
// `tauri::generate_context!()` compiles in jobs that do not build the UI
// (clippy, cargo test). Bundle jobs build the real UI and never need this.
//
// Usage: node scripts/ci-ensure-frontend-dist.mjs [path/to/tauri.conf.json]
// Default config: crates/app-tauri/tauri.conf.json. Exits 0 (with a notice) when the
// config does not exist yet or `frontendDist` is a URL.
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, isAbsolute, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const confPath = resolve(repoRoot, process.argv[2] ?? 'crates/app-tauri/tauri.conf.json');

if (!existsSync(confPath)) {
  console.log(`ci-ensure-frontend-dist: ${confPath} not found, nothing to do`);
  process.exit(0);
}

let conf;
try {
  conf = JSON.parse(readFileSync(confPath, 'utf8'));
} catch (err) {
  console.error(`ci-ensure-frontend-dist: cannot parse ${confPath}: ${err.message}`);
  process.exit(1);
}

const dist = conf?.build?.frontendDist;
if (typeof dist !== 'string' || dist.length === 0) {
  console.log('ci-ensure-frontend-dist: build.frontendDist is not a path string, nothing to do');
  process.exit(0);
}
if (/^[a-z][a-z0-9+.-]*:\/\//i.test(dist)) {
  console.log(`ci-ensure-frontend-dist: frontendDist is a URL (${dist}), nothing to do`);
  process.exit(0);
}

const distDir = isAbsolute(dist) ? dist : resolve(dirname(confPath), dist);
mkdirSync(distDir, { recursive: true });
if (readdirSync(distDir).length === 0) {
  writeFileSync(
    resolve(distDir, 'index.html'),
    '<!doctype html><meta charset="utf-8"><title>oxbase</title><p>CI placeholder frontend.</p>\n',
  );
  console.log(`ci-ensure-frontend-dist: created placeholder ${distDir}/index.html`);
} else {
  console.log(`ci-ensure-frontend-dist: ${distDir} already populated`);
}
