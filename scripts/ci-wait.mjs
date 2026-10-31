#!/usr/bin/env node
// Push a branch (optional) and wait for GitHub Actions to finish on that commit. Prints a one-line
// status per job and, for each failed job, only the relevant part of its log (compiler errors,
// failed tests, panics, `##[error]` lines), so the result fits in a terminal or an agent context.
//
// This is how changes are verified without building locally: push to a `wip/<milestone>-<stream>`
// branch, wait, fix, push again.
//
// Usage:
//   node scripts/ci-wait.mjs [--push] [--branch <name>] [--sha <sha>] [--workflow <name>]...
//                            [--timeout <minutes>] [--context <lines>]
//
//   --push            git push HEAD to origin/<branch> first (never to main).
//   --branch <name>   branch to push (default: the current branch).
//   --sha <sha>       commit to watch (default: HEAD).
//   --workflow <name> workflow name to wait for; repeatable (default: CI).
//   --timeout <min>   give up after this many minutes (default: 90).
//   --context <n>     log lines shown before each error line (default: 6).
//
// Exit codes: 0 all watched runs succeeded, 1 a run failed or was cancelled, 2 timeout or usage
// error, 3 GitHub API or git error.
//
// Token: GITHUB_TOKEN or GH_TOKEN, otherwise `git credential fill` for github.com (the same
// credential `git push` uses). The token is never printed.

import { execFileSync } from "node:child_process";

const args = process.argv.slice(2);
const opts = { push: false, branch: null, sha: null, workflows: [], timeout: 90, context: 6 };
for (let i = 0; i < args.length; i++) {
  const a = args[i];
  const next = () => {
    const v = args[++i];
    if (v === undefined) die(2, `missing value for ${a}`);
    return v;
  };
  if (a === "--push") opts.push = true;
  else if (a === "--branch") opts.branch = next();
  else if (a === "--sha") opts.sha = next();
  else if (a === "--workflow") opts.workflows.push(next());
  else if (a === "--timeout") opts.timeout = Number(next());
  else if (a === "--context") opts.context = Number(next());
  else if (a === "-h" || a === "--help") {
    console.log("usage: node scripts/ci-wait.mjs [--push] [--branch <name>] [--sha <sha>] [--workflow <name>] [--timeout <min>] [--context <n>]");
    process.exit(0);
  } else die(2, `unknown argument: ${a}`);
}
if (opts.workflows.length === 0) opts.workflows = ["CI"];
if (!Number.isFinite(opts.timeout) || opts.timeout <= 0) die(2, "--timeout must be a positive number");
if (!Number.isInteger(opts.context) || opts.context < 0) die(2, "--context must be a non-negative integer");

function die(code, msg) {
  console.error(`ci-wait: ${msg}`);
  process.exit(code);
}

function git(...a) {
  try {
    return execFileSync("git", a, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
  } catch (e) {
    die(3, `git ${a[0]} failed: ${(e.stderr || e.message).toString().trim()}`);
  }
}

function repoSlug() {
  const url = git("remote", "get-url", "origin");
  const m = url.match(/github\.com[:/]([^/]+)\/([^/]+?)(?:\.git)?\/?$/);
  if (!m) die(3, "origin is not a github.com remote");
  return `${m[1]}/${m[2]}`;
}

function token() {
  const env = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
  if (env) return env;
  try {
    const out = execFileSync("git", ["credential", "fill"], {
      input: "protocol=https\nhost=github.com\n\n",
      encoding: "utf8",
      stdio: ["pipe", "pipe", "ignore"],
      env: { ...process.env, GIT_TERMINAL_PROMPT: "0" },
    });
    const line = out.split("\n").find((l) => l.startsWith("password="));
    if (line) return line.slice("password=".length);
  } catch {
    // fall through
  }
  die(3, "no GitHub token: set GITHUB_TOKEN or store a github.com credential for git");
}

const REPO = repoSlug();
const TOKEN = token();
const API = `https://api.github.com/repos/${REPO}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function api(path, { raw = false } = {}) {
  for (let attempt = 1; ; attempt++) {
    let res;
    try {
      res = await fetch(path.startsWith("http") ? path : `${API}${path}`, {
        headers: {
          Authorization: `Bearer ${TOKEN}`,
          Accept: "application/vnd.github+json",
          "X-GitHub-Api-Version": "2022-11-28",
          "User-Agent": "oxbase-ci-wait",
        },
        redirect: "follow",
      });
    } catch (e) {
      if (attempt < 5) {
        await sleep(5000 * attempt);
        continue;
      }
      die(3, `request failed: ${e.message}`);
    }
    if (res.status >= 500 && attempt < 5) {
      await sleep(5000 * attempt);
      continue;
    }
    if (!res.ok) die(3, `GitHub API ${res.status} for ${path}`);
    return raw ? res.text() : res.json();
  }
}

// --- push -------------------------------------------------------------------------------------
const branch = opts.branch || git("rev-parse", "--abbrev-ref", "HEAD");
if (opts.push) {
  if (branch === "main" || branch === "HEAD") die(2, `refusing to push to '${branch}'; use a wip/* branch`);
  if (git("status", "--porcelain", "--untracked-files=no")) {
    console.error("ci-wait: note: uncommitted changes to tracked files are not pushed");
  }
  try {
    execFileSync("git", ["push", "--force-with-lease", "origin", `HEAD:refs/heads/${branch}`], {
      stdio: ["ignore", "inherit", "inherit"],
    });
  } catch {
    die(3, "git push failed");
  }
}
const sha = git("rev-parse", "--verify", `${opts.sha || "HEAD"}^{commit}`);
console.log(`ci-wait: ${REPO} ${branch} @ ${sha.slice(0, 12)}; waiting for: ${opts.workflows.join(", ")}`);

// --- wait -------------------------------------------------------------------------------------
const deadline = Date.now() + opts.timeout * 60_000;
const APPEAR_MS = 5 * 60_000; // a run normally appears within seconds of the push
const started = Date.now();
let lastLine = "";
let runs = [];

while (true) {
  const data = await api(`/actions/runs?head_sha=${sha}&per_page=50`);
  // Latest attempt per workflow name (re-runs and PR + push runs share a sha).
  const byName = new Map();
  for (const r of data.workflow_runs || []) {
    if (!opts.workflows.includes(r.name)) continue;
    const prev = byName.get(r.name);
    if (!prev || new Date(r.created_at) > new Date(prev.created_at)) byName.set(r.name, r);
  }
  runs = [...byName.values()];
  const missing = opts.workflows.filter((w) => !byName.has(w));
  const pending = runs.filter((r) => r.status !== "completed");

  const line =
    runs.map((r) => `${r.name}: ${r.status === "completed" ? r.conclusion : r.status}`).join(", ") +
    (missing.length ? `${runs.length ? ", " : ""}not started: ${missing.join(", ")}` : "");
  if (line !== lastLine) {
    console.log(`  [${Math.round((Date.now() - started) / 1000)}s] ${line}`);
    lastLine = line;
  }

  if (missing.length && Date.now() - started > APPEAR_MS) {
    die(2, `no run of ${missing.join(", ")} for ${sha.slice(0, 12)} after 5 minutes (is the branch covered by the workflow's triggers?)`);
  }
  if (!missing.length && !pending.length) break;
  if (Date.now() > deadline) die(2, `timed out after ${opts.timeout} minutes`);
  await sleep(missing.length ? 10_000 : 30_000);
}

// --- report -----------------------------------------------------------------------------------
const ERROR_RE =
  /##\[error\]|^error(\[E\d+\])?:|^warning: unused|panicked at|^test .* \.\.\. FAILED|^failures:|FAIL |✗|× |Error: |error TS\d+|thread '.*' panicked|could not compile|exited \(\d+\)|"s":"F"/;
const STRIP_TS = /^\d{4}-\d\d-\d\dT[\d:.]+Z ?/;
const MAX_LINES_PER_JOB = 150;

let failed = false;
for (const run of runs) {
  console.log(`\n${run.name}: ${run.conclusion}  ${run.html_url}`);
  if (run.conclusion !== "success") failed = true;
  const jobs = (await api(`/actions/runs/${run.id}/jobs?per_page=100`)).jobs || [];
  for (const job of jobs) {
    const badSteps = (job.steps || []).filter((s) => s.conclusion === "failure").map((s) => s.name);
    console.log(`  ${(job.conclusion || job.status).padEnd(9)} ${job.name}${badSteps.length ? `  [failed step: ${badSteps.join("; ")}]` : ""}`);
  }
  for (const job of jobs.filter((j) => j.conclusion === "failure" || j.conclusion === "timed_out")) {
    console.log(`\n----- ${job.name} (${job.conclusion}) -----`);
    let text;
    try {
      text = await api(`/actions/jobs/${job.id}/logs`, { raw: true });
    } catch {
      console.log("  (log unavailable)");
      continue;
    }
    // ANSI colours make the error regex miss; strip them and the timestamps.
    // biome-ignore lint/suspicious/noControlCharactersInRegex: matching ANSI escapes
    const lines = text.split("\n").map((l) => l.replace(STRIP_TS, "").replace(/\x1b\[[0-9;]*m/g, ""));
    const keep = new Set();
    lines.forEach((l, i) => {
      if (keep.size >= MAX_LINES_PER_JOB) return;
      if (ERROR_RE.test(l)) for (let k = Math.max(0, i - opts.context); k <= Math.min(lines.length - 1, i + 2); k++) keep.add(k);
    });
    const truncated = keep.size >= MAX_LINES_PER_JOB;
    // Always show the end of the log: it usually has the summary (test result, exit code).
    // Stop at GitHub's own post-job cleanup, which never explains a failure.
    let end = lines.findIndex((l) => l.startsWith("Post job cleanup."));
    if (end < 0) end = lines.length;
    for (let k = Math.max(0, end - 25); k < end; k++) keep.add(k);
    let prev = -2;
    for (const i of [...keep].sort((a, b) => a - b)) {
      if (i !== prev + 1) console.log("  ...");
      console.log(`  ${lines[i]}`);
      prev = i;
    }
    if (truncated) console.log(`  (more error lines omitted; full log: ${job.html_url})`);
  }
}
console.log(failed ? "\nci-wait: FAILED" : "\nci-wait: all green");
process.exit(failed ? 1 : 0);
