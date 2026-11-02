#!/usr/bin/env node
// Checks a release tag against the versions in the repository and prints the values the Release
// workflow needs. Run from the repository root.
//
//   node scripts/release-check.mjs <tag>        e.g. v0.6.0 or v0.7.0-beta.2
//   node scripts/release-check.mjs --dry-run    use the version in Cargo.toml; no tag needed
//
// Rules:
// - The tag is `v` + a SemVer version: MAJOR.MINOR.PATCH with an optional pre-release
//   (`-beta.2`, `-rc.1`, `-dev.3`). Build metadata (`+...`) is not allowed.
// - The version must equal `[workspace.package] version` in Cargo.toml (which the app and its
//   bundles use) and `version` in ui/package.json.
// - Windows MSI versions are numeric (major.minor.patch.build, major and minor <= 255, patch and
//   build <= 65535). A pre-release maps to build = its last numeric identifier (0 if none), so
//   v0.7.0-beta.2 builds an MSI with version 0.7.0.2.
//
// When GITHUB_OUTPUT is set, the results are appended to it as step outputs:
//   tag, version, prerelease (true/false), msi_version, artifact_prefix.

import { appendFileSync, readFileSync } from "node:fs";

function fail(msg) {
  console.error(`release-check: ${msg}`);
  process.exit(1);
}

const args = process.argv.slice(2);
const dryRun = args.includes("--dry-run");
const tagArg = args.find((a) => !a.startsWith("--"));
if (!dryRun && !tagArg) fail("usage: release-check.mjs <tag> | --dry-run");

const cargo = readFileSync("Cargo.toml", "utf8");
const section = cargo.match(/^\[workspace\.package\]\s*$([\s\S]*?)(?=^\[)/m);
const cargoVersion = section?.[1].match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (!cargoVersion) fail("no version in [workspace.package] of Cargo.toml");
const uiVersion = JSON.parse(readFileSync("ui/package.json", "utf8")).version;

const tag = tagArg ?? `v${cargoVersion}`;
const m = tag.match(/^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?$/);
if (!m) fail(`tag '${tag}' is not v<semver> (e.g. v0.6.0 or v0.7.0-beta.2; no build metadata)`);
const [, major, minor, patch, pre] = m;
const version = tag.slice(1);

if (cargoVersion !== version) {
  fail(`tag ${tag} does not match Cargo.toml [workspace.package] version ${cargoVersion}; bump it first`);
}
if (uiVersion !== version) {
  fail(`tag ${tag} does not match ui/package.json version ${uiVersion}; bump it first`);
}

const build = pre ? Number([...pre.split(".")].reverse().find((p) => /^\d+$/.test(p)) ?? 0) : 0;
if (Number(major) > 255 || Number(minor) > 255) fail("MSI needs major and minor <= 255");
if (Number(patch) > 65535 || build > 65535) fail("MSI needs patch and build <= 65535");
const msiVersion = pre ? `${major}.${minor}.${patch}.${build}` : `${major}.${minor}.${patch}`;

const out = {
  tag,
  version,
  prerelease: pre ? "true" : "false",
  msi_version: msiVersion,
  artifact_prefix: `oxbase-${version}`,
};
for (const [k, v] of Object.entries(out)) console.log(`${k}=${v}`);
if (process.env.GITHUB_OUTPUT) {
  appendFileSync(process.env.GITHUB_OUTPUT, `${Object.entries(out).map(([k, v]) => `${k}=${v}`).join("\n")}\n`);
}
