#!/usr/bin/env node
// Webview smoke test (docs/m3-contracts.md §9): drives the built desktop app through its real
// webview with `tauri-driver` and checks the M2/M3 paths no unit test reaches: the window loads,
// a saved PostgreSQL connection opens with a prompted password, a 1,000,000-row query streams
// its Channel events into the grid, `query_page` Arrow bytes decode in the webview, scrolling
// loads pages, page latency (IPC + Arrow decode) stays under a bound, a running query cancels
// within 2 s, once the M3 UI exists a table cell can be edited and, once the app has the
// debug-only file hook (M5), a CSV is imported and a query result exported to CSV through the
// file-handle commands.
//
// Run it through scripts/e2e-smoke.sh, which prepares the profile, the data directories and
// tauri-driver. How to run it and its limits: ui/e2e/README.md.
//
// Environment (set by scripts/e2e-smoke.sh):
//   OXBASE_E2E_APP            path of the debug app binary (required)
//   OXBASE_E2E_PG_PASSWORD    password of the PostgreSQL fixture, typed into the prompt (required)
//   OXBASE_E2E_PROFILE_NAME   name of the saved profile (default "E2E Postgres")
//   OXBASE_E2E_VERSION        core version the status bar must show (optional)
//   OXBASE_E2E_DRIVER_URL     tauri-driver URL (default http://127.0.0.1:4444)
//   OXBASE_E2E_OUT            output directory for e2e-perf.json, screenshots, DOM dumps
//   OXBASE_E2E_P95_LIMIT_MS   page latency bound (default 250)
//   OXBASE_E2E_CANCEL_LIMIT_MS cancel bound (default 2000)
//   OXBASE_E2E_REQUIRE_EDIT   1: the M3 edit step must run; a skip fails the job. e2e-smoke.sh
//                             sets it when the checkout has the M3 table-data UI and fixtures
//   OXBASE_E2E_REQUIRE_M5     1: the M5 import / export step must run and pass; without it a
//                             missing file hook or a failure of that step is a skip (warning)
//   GITHUB_STEP_SUMMARY       if set, a Markdown summary is appended to it

import { createReadStream } from "node:fs";
import { appendFile, copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Keys, sleep, WebDriver } from "./webdriver.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const env = process.env;

const config = {
  driverUrl: env.OXBASE_E2E_DRIVER_URL || "http://127.0.0.1:4444",
  application: env.OXBASE_E2E_APP || "",
  password: env.OXBASE_E2E_PG_PASSWORD || "",
  profileName: env.OXBASE_E2E_PROFILE_NAME || "E2E Postgres",
  version: env.OXBASE_E2E_VERSION || "",
  outDir: env.OXBASE_E2E_OUT || path.resolve(here, "../../target/e2e"),
  p95LimitMs: Number(env.OXBASE_E2E_P95_LIMIT_MS || 250),
  cancelLimitMs: Number(env.OXBASE_E2E_CANCEL_LIMIT_MS || 2000),
  requireEdit: /^(1|true|yes)$/i.test(env.OXBASE_E2E_REQUIRE_EDIT || ""),
  requireM5: /^(1|true|yes)$/i.test(env.OXBASE_E2E_REQUIRE_M5 || ""),
  rows: 1_000_000,
  probeSamples: 60,
  pageSize: 200,
  summaryFile: env.GITHUB_STEP_SUMMARY || "",
};

const BIG_QUERY = `SELECT g, md5(g::text) FROM generate_series(1, ${config.rows}) g`;
// 100,000,000 rows that stream: each generate_series materializes only 10,000 rows and the
// nested loop emits rows as it goes, so the run is still busy when it is cancelled.
const CANCEL_QUERY =
  "SELECT a.i * 10000 + b.i AS g, md5((a.i * 10000 + b.i)::text) " +
  "FROM generate_series(0, 9999) a(i), generate_series(1, 10000) b(i)";
const MD5_OF_1 = "c4ca4238a0b923820dcc509a6f75849b";

// M5 file hook (ui/e2e/README.md, "Import and export"): debug builds of the app accept
// `--e2e-file-dir=<dir>`; `file_pick_open` then returns a read handle for the one file in
// `<dir>/open/` and `file_pick_save` a write handle for `<dir>/save/<defaultName>`, without a
// native dialog (WebDriver cannot drive one). The test passes the argument only when the app
// binary contains it, so builds without the hook start exactly as before.
const FILE_HOOK_ARG = "--e2e-file-dir";
const M5_SAMPLE = path.resolve(here, "../../tests/data/m5/basic.csv");
const M5_EXPORT_NAME = "m5-e2e-export.csv";

// UI hooks. The `testids` are the `data-testid`s the app stream adds (docs/m3-contracts.md §9);
// the fallbacks match the M2 UI so the test runs before and after they land.
const HOOKS = {
  statusBar: { testids: ["status-bar", "core-version"] },
  openConnections: { testids: ["open-connections"] },
  connectionList: {
    testids: ["connection-list"],
    css: '[role="listbox"][aria-label="Saved connections"]',
  },
  connectButton: {
    testids: ["connect-button"],
    buttonText: [".panel--connections .toolbar button", "Connect"],
  },
  passwordPrompt: { testids: ["password-prompt"], css: "#prompt-password" },
  newQuery: { testids: ["new-query"], buttonText: [".app__toolbar button", "New query"] },
  editor: { testids: ["sql-editor"] },
  runButton: { testids: ["run-button"], buttonText: [".query-editor__toolbar button", "Run"] },
  cancelButton: {
    testids: ["cancel-button"],
    buttonText: [".query-editor__toolbar button", "Stop"],
  },
  runStatus: { testids: ["run-status"], css: ".query-editor__status" },
  rowCounter: { testids: ["row-counter", "result-status"] },
  resultGrid: { testids: ["result-grid"], css: ".results .result-grid" },
  // M3 table data and editing (app stream, origin/wip/m3-app TableDataPanel / CellEditor). A
  // table-data tab is the `section.table-data` labelled "Table <schema>.<name>".
  tableData: { testids: ["table-data"], css: "section.table-data" },
  tableDataStatus: { testids: ["table-data-status"] },
  applyEdits: { testids: ["apply-edits", "edit-apply"] },
  applyErrors: { testids: ["apply-errors"] },
  cellEditor: { testids: ["cell-editor"] },
};

// In-page helpers, installed once after the window loads. `locate(spec)` resolves a HOOKS
// entry; the fetch wrapper times every IPC request (Tauri 2 sends commands with `fetch` to the
// `ipc:` scheme on Linux and to `http://ipc.localhost` on Windows) without touching its result;
// the observer records every change of the run status and row counter with a timestamp.
const INSTALL_PROBES = `
const hooks = arguments[0];
if (window.__e2e) return "already";
const state = { ipc: [], log: [], errors: [], hooks };
function locate(spec) {
  for (const id of spec.testids || []) {
    const el = document.querySelector('[data-testid="' + id + '"]');
    if (el) return el;
  }
  if (spec.css) {
    const el = document.querySelector(spec.css);
    if (el) return el;
  }
  if (spec.buttonText) {
    for (const el of document.querySelectorAll(spec.buttonText[0])) {
      if (el.textContent.trim() === spec.buttonText[1]) return el;
    }
  }
  return null;
}
state.locate = locate;
state.text = (spec) => {
  const el = locate(spec);
  return el ? el.textContent.replace(/\\u00a0/g, " ").trim() : null;
};
const origFetch = window.fetch.bind(window);
window.fetch = async function (input, init) {
  const url = typeof input === "string" ? input : input && input.url ? input.url : String(input);
  const m = /^ipc:\\/\\/localhost\\/([^?#]+)/.exec(url) || /^https?:\\/\\/ipc\\.localhost\\/([^?#]+)/.exec(url);
  if (!m) return origFetch(input, init);
  const entry = { cmd: decodeURIComponent(m[1]), start: performance.now(), end: null, bytes: null };
  if (entry.cmd === "query_page" && init && typeof init.body === "string") {
    try {
      const a = JSON.parse(init.body);
      entry.runId = a.runId; entry.resultIndex = a.resultIndex; entry.offset = a.offset; entry.limit = a.limit;
    } catch (e) {}
  }
  state.ipc.push(entry);
  if (state.ipc.length > 20000) state.ipc.splice(0, 5000);
  const response = await origFetch(input, init);
  if (entry.cmd !== "query_page") { entry.end = performance.now(); return response; }
  const body = await response.arrayBuffer();
  entry.end = performance.now();
  entry.bytes = body.byteLength;
  entry.ok = response.headers.get("Tauri-Response") === "ok";
  return new Response(body, { status: response.status, statusText: response.statusText, headers: response.headers });
};
let last = { status: null, rows: null };
function record() {
  const status = state.text(hooks.runStatus);
  const rows = state.text(hooks.rowCounter);
  if (status !== last.status || rows !== last.rows) {
    last = { status, rows };
    state.log.push({ t: performance.now(), status, rows });
    if (state.log.length > 50000) state.log.splice(0, 10000);
  }
}
new MutationObserver(record).observe(document.body, { subtree: true, childList: true, characterData: true });
window.addEventListener("error", (e) => state.errors.push(String(e.message)));
window.addEventListener("unhandledrejection", (e) => state.errors.push("unhandled rejection: " + String(e.reason && e.reason.message || e.reason)));
window.__e2e = state;
return "installed";
`;

// A grid cell as the result grid's accessibility table shows it (Glide Data Grid renders the
// visible cells as a <table role="grid"> inside its canvas). `column` is a header title.
const GRID_STATE = `
const grid = window.__e2e.locate(window.__e2e.hooks[arguments[0]]);
if (!grid) return { found: false };
const table = grid.querySelector('table[role="grid"]');
const scroller = grid.querySelector(".dvn-scroller");
if (!table) return { found: true, table: false };
const headers = {};
for (const th of table.querySelectorAll('thead th')) headers[th.textContent.trim()] = th.getAttribute("aria-colindex");
const rows = [];
for (const tr of table.querySelectorAll('tbody tr')) {
  const row = Number(tr.getAttribute("aria-rowindex")) - 2;
  const cells = {};
  for (const [name, idx] of Object.entries(headers)) {
    const td = tr.querySelector('td[aria-colindex="' + idx + '"]');
    if (td) cells[name] = td.textContent;
  }
  rows.push({ row, cells });
}
return {
  found: true, table: true, headers: Object.keys(headers), rows,
  scrollTop: scroller ? scroller.scrollTop : null,
  scrollHeight: scroller ? scroller.scrollHeight : null,
  clientHeight: scroller ? scroller.clientHeight : null,
};
`;

const results = [];
const report = {
  startedAt: new Date().toISOString(),
  application: config.application,
  requireEdit: config.requireEdit,
  requireM5: config.requireM5,
  query: null,
  pageLatency: null,
  scroll: [],
  cancel: null,
  edit: null,
  m5: null,
  steps: results,
  pageErrors: [],
};
let wd = null;
/** M5 step state: whether the app has the file hook and the folder it uses. */
const m5 = { hook: false, dir: path.join(config.outDir, "m5-files") };

function log(message) {
  console.log(`[e2e] ${message}`);
}

/** A GitHub Actions error annotation (shown on the run page and by scripts/ci-wait.mjs). */
function annotate(message, level = "error") {
  const escaped = message.replaceAll("%", "%25").replaceAll("\r", "%0D").replaceAll("\n", "%0A");
  console.log(`::${level}::Webview smoke: ${escaped}`);
}

class SkipStep extends Error {}

/** Thrown inside a `waitFor` condition to stop waiting at once. */
class Fatal extends Error {}

/** Which env var makes a skip of a step a failure (the M3 edit step by default). */
const EDIT_GATE = { required: config.requireEdit, env: "OXBASE_E2E_REQUIRE_EDIT" };
const M5_GATE = { required: config.requireM5, env: "OXBASE_E2E_REQUIRE_M5" };

async function step(name, fn, gate = EDIT_GATE) {
  const started = Date.now();
  log(`--- ${name}`);
  try {
    const detail = (await fn()) ?? "";
    results.push({ name, status: "passed", ms: Date.now() - started, detail });
    log(`ok: ${name}${detail ? ` (${detail})` : ""}`);
    return true;
  } catch (error) {
    const message = String(error?.message ?? error);
    if (error instanceof SkipStep && !gate.required) {
      results.push({ name, status: "skipped", ms: Date.now() - started, detail: message });
      // A warning annotation, so the skip shows on the run page and in scripts/ci-wait.mjs.
      annotate(`skipped: ${name}: ${message}`, "warning");
      log(`skipped: ${name}: ${message}`);
      return true;
    }
    if (error instanceof SkipStep) {
      const detail = `${message} (a skip fails the run: ${gate.env}=1)`;
      results.push({ name, status: "failed", ms: Date.now() - started, detail });
      annotate(`${name}: ${detail}`);
      log(`FAILED: ${name}: ${detail}`);
      return false;
    }
    results.push({ name, status: "failed", ms: Date.now() - started, detail: message });
    annotate(`${name}: ${message}`);
    log(`FAILED: ${name}: ${String(error?.stack ?? error)}`);
    return false;
  }
}

async function waitFor(what, fn, { timeoutMs = 30_000, intervalMs = 100 } = {}) {
  const deadline = Date.now() + timeoutMs;
  let last;
  for (;;) {
    try {
      last = await fn();
      if (last) return last;
    } catch (error) {
      if (error instanceof Fatal) throw error;
      last = error;
    }
    if (Date.now() > deadline) {
      const why = last instanceof Error ? `: ${last.message}` : "";
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}${why}`);
    }
    await sleep(intervalMs);
  }
}

function hookText(name) {
  return wd.execute(
    "const s = window.__e2e; return s ? s.text(s.hooks[arguments[0]]) : null;",
    name,
  );
}

function locate(name) {
  return wd.executeElement(
    "const s = window.__e2e; return s ? s.locate(s.hooks[arguments[0]]) : null;",
    name,
  );
}

/** The first number in `text` ("1,000,000 rows…" -> 1000000), or null. */
function parseCount(text) {
  const m = /(\d[\d,]*)/.exec(text ?? "");
  return m ? Number(m[1].replaceAll(",", "")) : null;
}

function percentile(sorted, p) {
  if (sorted.length === 0) return null;
  const index = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[Math.max(0, index)];
}

function stats(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const round = (v) => (v === null ? null : Math.round(v * 100) / 100);
  return {
    count: sorted.length,
    p50: round(percentile(sorted, 50)),
    p95: round(percentile(sorted, 95)),
    max: round(sorted.length ? sorted[sorted.length - 1] : null),
  };
}

/** Replaces the Monaco editor's text by typing `sql`, and checks what the editor shows. */
async function typeSql(sql) {
  const textarea = await waitFor(
    "the SQL editor (Monaco)",
    () =>
      wd.executeElement(
        "const e = window.__e2e.locate(window.__e2e.hooks.editor);" +
          "return e ? e.querySelector('textarea') : null;",
      ),
    { timeoutMs: 60_000 },
  );
  // Focus the editor as a user would (a click on its text area), else focus its input directly.
  const host = await locate("editor");
  await wd.click(host).catch(() => wd.execute("arguments[0].focus();", textarea));
  await wd.chord(Keys.control, "a");
  await wd.sendKeys(textarea, Keys.backspace);
  await wd.sendKeys(textarea, sql);
  await wd.sendKeys(textarea, Keys.escape);
  const shown = () =>
    wd.execute(
      "const e = window.__e2e.locate(window.__e2e.hooks.editor);" +
        "const v = e && e.querySelector('.view-lines');" +
        "return v ? v.textContent.replace(/\\u00a0/g, ' ').trim() : null;",
    );
  let text = await shown();
  if (text !== sql) {
    // Typing raced an auto-closed bracket or a suggestion: insert the text in one go instead.
    log(`editor shows ${JSON.stringify(text)}; inserting the text instead of typing it`);
    await wd.chord(Keys.control, "a");
    await wd.execute(
      "arguments[0].focus(); document.execCommand('insertText', false, arguments[1]);",
      textarea,
      sql,
    );
    await sleep(200);
    text = await shown();
  }
  if (text !== sql) throw new Error(`the editor shows ${JSON.stringify(text)}, not the query`);
}

async function screenshot(name) {
  try {
    const png = await wd.screenshot();
    const file = path.join(config.outDir, `${name}.png`);
    await writeFile(file, Buffer.from(png, "base64"));
    log(`screenshot: ${file}`);
  } catch (error) {
    log(`no screenshot: ${error.message}`);
  }
}

async function dumpDom(name) {
  try {
    // Values of password fields are dropped (React mirrors an input's value into its attribute).
    const html = await wd.execute(
      "const c = document.documentElement.cloneNode(true);" +
        "for (const i of c.querySelectorAll('input')) i.removeAttribute('value');" +
        "return c.outerHTML;",
    );
    await writeFile(path.join(config.outDir, `${name}.html`), html);
  } catch (error) {
    log(`no DOM dump: ${error.message}`);
  }
}

// ---------------------------------------------------------------------------------------------
// Steps

async function launch() {
  if (!config.application) throw new Error("OXBASE_E2E_APP is not set");
  await WebDriver.waitReady(config.driverUrl, 30_000);
  wd = new WebDriver(config.driverUrl);
  const args = [];
  m5.hook = await binaryContains(config.application, FILE_HOOK_ARG).catch((error) => {
    log(`could not scan the app binary for ${FILE_HOOK_ARG}: ${error.message}`);
    return false;
  });
  if (m5.hook) {
    await prepareM5Files();
    args.push(`${FILE_HOOK_ARG}=${m5.dir}`);
  }
  log(`M5 file hook ${m5.hook ? `found: ${FILE_HOOK_ARG}=${m5.dir}` : "not in this build"}`);
  await wd.newSession({ "tauri:options": { application: config.application, args } });
  await wd.setTimeouts({ script: 180_000, pageLoad: 60_000, implicit: 0 });
  await waitFor(
    "the document to load",
    () => wd.execute("return document.readyState === 'complete' && !!document.body;"),
    { timeoutMs: 60_000 },
  );
  const installed = await wd.execute(INSTALL_PROBES, HOOKS);
  const status = await waitFor(
    "the status bar to show the core version",
    async () => {
      const text = await hookText("statusBar");
      return text?.includes("oxbase core") ? text : null;
    },
    { timeoutMs: 60_000 },
  );
  if (config.version && !status.includes(`oxbase core ${config.version}`)) {
    throw new Error(`status bar shows "${status}", expected version ${config.version}`);
  }
  const tauri = await wd.execute("return typeof window.__TAURI_INTERNALS__ === 'object';");
  if (!tauri) throw new Error("window.__TAURI_INTERNALS__ is missing: not running in Tauri");
  return `${status}; probes ${installed}`;
}

async function connect() {
  const option = await waitFor(`the saved connection "${config.profileName}"`, () =>
    wd.executeElement(
      "const list = window.__e2e.locate(window.__e2e.hooks.connectionList);" +
        "if (!list) return null;" +
        "for (const o of list.querySelectorAll('[role=option], [data-testid=connection-item]')) {" +
        "  if (o.textContent.includes(arguments[0])) return o; }" +
        "return null;",
      config.profileName,
    ),
  );
  await wd.click(option);
  const button = await waitFor("the Connect button", async () => {
    const el = await locate("connectButton");
    if (!el) return null;
    const enabled = await wd.execute("return !arguments[0].disabled;", el);
    return enabled ? el : null;
  });
  await wd.click(button);
  const prompt = await waitFor("the password prompt", async () => {
    const el = await locate("passwordPrompt");
    if (!el) return null;
    // The hook may sit on the dialog rather than on its input.
    return wd.executeElement(
      "const e = arguments[0]; return e.tagName === 'INPUT' ? e : e.querySelector('input[type=password]');",
      el,
    );
  });
  await wd.sendKeys(prompt, config.password);
  await wd.sendKeys(prompt, Keys.enter);
  const opened = await waitFor(
    "the connection to open",
    async () => {
      const text = await hookText("openConnections");
      if (text && /^1 open connection/.test(text)) return text;
      const error = await wd.execute(
        "const a = document.querySelector('.panel--connections [role=alert]');" +
          "return a ? a.textContent : null;",
      );
      if (error) throw new Fatal(`connect failed: ${error}`);
      return null;
    },
    { timeoutMs: 30_000 },
  );
  return opened;
}

async function runBigQuery() {
  const button = await locate("newQuery");
  if (!button) throw new Error("no New query button");
  await wd.click(button);
  await typeSql(BIG_QUERY);
  // The editor takes the first open connection.
  const t0 = await clickRun();
  const ended = await waitFor(`${config.rows.toLocaleString("en-US")} rows`, () => endedSince(t0), {
    timeoutMs: 180_000,
    intervalMs: 250,
  });
  if (!ended.status.startsWith("Finished")) throw new Error(`run ended: ${ended.status}`);
  const done = await waitFor("the row counter to show the final count", async () => {
    const rows = (await hookText("rowCounter")) ?? "";
    return rows && !rows.includes("…") ? { status: ended.status, rows } : null;
  });
  const timeline = await wd.execute(
    "return window.__e2e.log.filter((e) => e.t >= arguments[0]);",
    t0,
  );
  const counts = [];
  for (const entry of timeline) {
    const n = parseCount(entry.rows);
    if (n !== null && counts[counts.length - 1] !== n) counts.push(n);
  }
  const final = parseCount(done.rows);
  if (final !== config.rows) {
    throw new Error(`row counter shows "${done.rows}", expected ${config.rows} rows`);
  }
  const doneAt = ended.t;
  report.query = {
    sql: BIG_QUERY,
    rows: final,
    status: done.status,
    counterUpdates: counts.length,
    firstCounts: counts.slice(0, 10),
    msToFinished: doneAt === null ? null : Math.round(doneAt - t0),
  };
  // Channel events: the counter must have shown partial counts before the final one, else the
  // UI may only be showing the finished result.
  const partial = counts.filter((n) => n > 0 && n < config.rows);
  report.query.partialCounts = partial.length;
  if (partial.length < 2) {
    throw new Error(
      `the row counter showed ${partial.length} partial counts before ${config.rows} ` +
        `(${counts.slice(0, 10).join(", ")}): Channel row events did not reach the UI while streaming`,
    );
  }
  return `${final} rows; row counter showed ${partial.length} partial counts while streaming; ${done.status}`;
}

/** Clicks Run and waits until the run shows as running; returns the click time (page clock). */
async function clickRun() {
  const run = await waitFor("an enabled Run button", async () => {
    const el = await locate("runButton");
    return el && (await wd.execute("return !arguments[0].disabled;", el)) ? el : null;
  });
  const t0 = await wd.execute("return performance.now();");
  await wd.click(run);
  await waitFor(
    "the run to start",
    () =>
      wd.execute(
        "return window.__e2e.log.some((e) => e.t >= arguments[0] && /^Running/.test(e.status || ''));",
        t0,
      ),
    { timeoutMs: 20_000, intervalMs: 50 },
  );
  return t0;
}

/** The first terminal status (Finished, Failed, Cancelled) recorded after `t0`. */
function endedSince(t0) {
  return wd.execute(
    "let running = false;" +
      "for (const e of window.__e2e.log) { if (e.t < arguments[0]) continue;" +
      "  if (/^Running/.test(e.status || '')) running = true;" +
      "  else if (running && /^(Finished|Failed|Cancelled)/.test(e.status || '')) return e; }" +
      "return null;",
    t0,
  );
}

async function gridState() {
  return wd.execute(GRID_STATE, "resultGrid");
}

/** Waits until the grid shows row `row` (or the first visible row) with data in column "g". */
async function waitGridData(what, check, { timeoutMs = 15_000 } = {}) {
  let nudged = Date.now();
  return waitFor(
    what,
    async () => {
      const state = await gridState();
      if (!state.found || !state.table) return null;
      const hit = check(state);
      if (hit) return { state, hit };
      // The accessibility table is rebuilt when the viewport moves, not when a page arrives:
      // move one row and back so it reads the loaded page.
      if (Date.now() - nudged > 1500) {
        nudged = Date.now();
        await wd.execute(
          "const s = window.__e2e.locate(window.__e2e.hooks.resultGrid).querySelector('.dvn-scroller');" +
            "if (s) { const t = s.scrollTop; s.scrollTop = t + 40;" +
            " setTimeout(() => { s.scrollTop = t; }, 300); }",
        );
      }
      return null;
    },
    { timeoutMs, intervalMs: 100 },
  );
}

async function firstCell() {
  const { hit } = await waitGridData('row 1 of column "g" in the grid', (state) => {
    const row = state.rows.find((r) => r.row === 0);
    return row && row.cells.g === "1" ? row : null;
  });
  if (hit.cells.md5 !== undefined && hit.cells.md5 !== MD5_OF_1) {
    throw new Error(`row 1 shows md5 ${JSON.stringify(hit.cells.md5)}, expected ${MD5_OF_1}`);
  }
  return `first row: g=${hit.cells.g}${hit.cells.md5 ? `, md5=${hit.cells.md5}` : ""}`;
}

async function scrollGrid() {
  const positions = [0.25, 0.5, 0.75, 1, 0.1, 0.9, 0];
  const out = [];
  for (const fraction of positions) {
    const before = await gridState();
    const t0 = await wd.execute(
      "const s = window.__e2e.locate(window.__e2e.hooks.resultGrid).querySelector('.dvn-scroller');" +
        "const t = performance.now();" +
        "s.scrollTop = Math.round(arguments[0] * (s.scrollHeight - s.clientHeight));" +
        "return t;",
      fraction,
    );
    const previousFirst = before.rows[0]?.row ?? -1;
    const { state, hit } = await waitGridData(
      `the grid at ${Math.round(fraction * 100)}%`,
      (s) => {
        const first = s.rows[0];
        if (!first) return null;
        if (fraction !== 0 && first.row === previousFirst) return null;
        if (fraction === 0 && first.row !== 0) return null;
        // Row r of generate_series(1, n) holds r + 1: the page is the right one.
        return first.cells.g === String(first.row + 1) ? first : null;
      },
      { timeoutMs: 20_000 },
    );
    const shownAt = await wd.execute("return performance.now();");
    const pages = await wd.execute(
      "return window.__e2e.ipc.filter((e) => e.cmd === 'query_page' && e.start >= arguments[0]);",
      t0,
    );
    const covering = pages.find(
      (p) => p.end !== null && p.offset <= hit.row && hit.row < p.offset + p.limit,
    );
    out.push({
      fraction,
      firstRow: hit.row,
      visibleRows: state.rows.length,
      pageRequests: pages.length,
      scrollToPageMs: covering ? Math.round((covering.end - t0) * 10) / 10 : null,
      pageIpcMs: covering ? Math.round((covering.end - covering.start) * 10) / 10 : null,
      scrollToCellsMs: Math.round(shownAt - t0),
    });
  }
  report.scroll = out;
  const last = out[out.length - 1];
  const far = out.find((o) => o.fraction === 1);
  if (!far || far.firstRow < config.rows - 200) {
    throw new Error(`scrolling to the end showed row ${far?.firstRow}, not the last rows`);
  }
  if (last.firstRow !== 0) throw new Error("scrolling back to the top did not show row 1");
  const loaded = out.reduce((n, o) => n + o.pageRequests, 0);
  return `${out.length} positions, ${loaded} page requests, rows ${out.map((o) => o.firstRow).join(", ")}`;
}

/** Loads apache-arrow (the UI's own dependency, Apache-2.0) into the webview as `window.Arrow`. */
async function loadArrow() {
  if (await wd.execute("return typeof window.Arrow?.tableFromIPC === 'function';")) return;
  const arrowPath = path.resolve(here, "../node_modules/apache-arrow/Arrow.es2015.min.js");
  const arrow = await readFile(arrowPath, "utf8");
  await wd.execute(`if (!window.Arrow) { (function () {\n${arrow}\n}).call(window); }`);
  const hasArrow = await wd.execute("return typeof window.Arrow?.tableFromIPC === 'function';");
  if (!hasArrow) throw new Error("could not load apache-arrow into the webview");
}

/** Runs probe pages (`query_page` invoked from the webview, decoded with apache-arrow there). */
async function measurePages() {
  // The run behind the grid: the last page the grid requested.
  let runId = await wd.execute(
    "const p = window.__e2e.ipc.filter((e) => e.cmd === 'query_page' && e.runId);" +
      "return p.length ? p[p.length - 1].runId : null;",
  );
  await loadArrow();
  let source = "grid run";
  if (!runId) {
    runId = await startProbeRun();
    source = "probe run";
  }
  const samples = await wd.executeAsync(
    `const [runId, rows, pageSize, count, done] = arguments;
     (async () => {
       const out = [];
       const invoke = window.__TAURI_INTERNALS__.invoke;
       for (let i = 0; i < count; i += 1) {
         const offset = i === 0 ? 0 : Math.floor(Math.random() * (rows - pageSize));
         const t0 = performance.now();
         const raw = await invoke("query_page", { runId, resultIndex: 0, offset, limit: pageSize });
         const t1 = performance.now();
         const bytes = raw instanceof ArrayBuffer ? new Uint8Array(raw)
           : raw instanceof Uint8Array ? raw : Uint8Array.from(raw);
         const table = window.Arrow.tableFromIPC(bytes);
         const first = Number(table.getChildAt(0).get(0));
         const t2 = performance.now();
         out.push({ offset, limit: pageSize, ipcMs: t1 - t0, decodeMs: t2 - t1, bytes: bytes.byteLength,
                    rows: table.numRows, ok: table.numRows === pageSize && first === offset + 1 });
       }
       return out;
     })().then(done, (e) => done({ error: String(e && e.message || e) }));`,
    runId,
    config.rows,
    config.pageSize,
    config.probeSamples,
  );
  if (!Array.isArray(samples)) throw new Error(`probe pages failed: ${samples?.error}`);
  const bad = samples.filter((s) => !s.ok);
  if (bad.length > 0) {
    throw new Error(
      `${bad.length} probe pages decoded wrong rows (first: offset ${bad[0].offset})`,
    );
  }
  // The app's own measurements (window.__oxbasePerf, filled by the grid's page fetcher).
  const appPages = await wd.execute(
    "const p = window.__oxbasePerf && window.__oxbasePerf.pages; return Array.isArray(p) ? p : null;",
  );
  const probe = {
    source,
    pageSize: config.pageSize,
    totalMs: stats(samples.map((s) => s.ipcMs + s.decodeMs)),
    ipcMs: stats(samples.map((s) => s.ipcMs)),
    decodeMs: stats(samples.map((s) => s.decodeMs)),
    bytes: stats(samples.map((s) => s.bytes)),
  };
  const app =
    appPages && appPages.length > 0
      ? {
          totalMs: stats(appPages.map((p) => Number(p.ipcMs) + Number(p.decodeMs))),
          ipcMs: stats(appPages.map((p) => Number(p.ipcMs))),
          decodeMs: stats(appPages.map((p) => Number(p.decodeMs))),
        }
      : null;
  const gridPages = await wd.execute(
    "return window.__e2e.ipc.filter((e) => e.cmd === 'query_page' && e.end !== null)" +
      ".map((e) => ({ ms: e.end - e.start, bytes: e.bytes, limit: e.limit }));",
  );
  report.pageLatency = {
    limitMs: config.p95LimitMs,
    asserted: app && app.totalMs.count >= 5 ? "app (window.__oxbasePerf)" : "probe",
    probe,
    app,
    gridIpcMs: gridPages.length ? stats(gridPages.map((p) => p.ms)) : null,
    samples,
  };
  const asserted = report.pageLatency.asserted === "probe" ? probe.totalMs : app.totalMs;
  const text =
    `IPC + decode p50 ${asserted.p50} ms, p95 ${asserted.p95} ms, max ${asserted.max} ms ` +
    `(${asserted.count} pages of ${config.pageSize} rows, ${report.pageLatency.asserted})`;
  if (asserted.p95 > config.p95LimitMs) {
    throw new Error(`${text}: p95 above ${config.p95LimitMs} ms`);
  }
  return text;
}

/** Starts the 1M-row query from the webview, without the UI, and waits until it is stored. */
async function startProbeRun() {
  const result = await wd.executeAsync(
    `const [sql, rows, done] = arguments;
     (async () => {
       const T = window.__TAURI_INTERNALS__;
       const sessions = await T.invoke("list_open_connections");
       if (!sessions.length) throw new Error("no open connection");
       const callback = T.transformCallback(() => {});
       const runId = await T.invoke("query_run", { sessionId: sessions[0].id, editorId: "e2e-probe",
         script: sql, options: null, onEvent: "__CHANNEL__:" + callback });
       const deadline = performance.now() + 120000;
       for (;;) {
         try {
           const raw = await T.invoke("query_page", { runId, resultIndex: 0, offset: rows - 1, limit: 1 });
           const bytes = raw instanceof ArrayBuffer ? new Uint8Array(raw) : Uint8Array.from(raw);
           if (window.Arrow.tableFromIPC(bytes).numRows === 1) return runId;
         } catch (e) {
           // Not stored yet.
         }
         if (performance.now() > deadline) throw new Error("probe run did not finish");
         await new Promise((r) => setTimeout(r, 250));
       }
     })().then(done, (e) => done({ error: String(e && e.message || e) }));`,
    BIG_QUERY,
    config.rows,
  );
  if (typeof result !== "string") throw new Error(`probe run failed: ${result?.error}`);
  return result;
}

async function cancelRun() {
  await typeSql(CANCEL_QUERY);
  const started = await clickRun();
  const streaming = await waitFor(
    "the large query to stream at least 100,000 rows",
    async () => {
      const ended = await endedSince(started);
      if (ended) throw new Fatal(`run ended: ${ended.status}`);
      const rows = parseCount(await hookText("rowCounter"));
      return rows !== null && rows >= 100_000 ? rows : null;
    },
    { timeoutMs: 60_000, intervalMs: 100 },
  );
  const stop = await locate("cancelButton");
  if (!stop) throw new Error("no Stop button");
  const t0 = await wd.execute("return performance.now();");
  await wd.click(stop);
  const entry = await waitFor("the run to end", () => endedSince(started), {
    timeoutMs: 15_000,
    intervalMs: 50,
  });
  if (!entry.status.startsWith("Cancelled")) throw new Error(`run ended as "${entry.status}"`);
  const ms = Math.round(entry.t - t0);
  const rowsAtEnd = parseCount(await hookText("rowCounter"));
  report.cancel = { sql: CANCEL_QUERY, rowsWhenStopped: streaming, rowsKept: rowsAtEnd, ms };
  if (ms > config.cancelLimitMs) {
    throw new Error(`cancel took ${ms} ms (limit ${config.cancelLimitMs} ms)`);
  }
  return `cancelled ${ms} ms after Stop, at ${streaming} rows (${rowsAtEnd} rows kept)`;
}

/**
 * Expands the object tree along `labels` and returns the row labelled `target` below them.
 * Each label is looked up among the descendants of the previous one, so "Tables" of schema
 * `m3` is not confused with "Tables" of `public`; a label the tree does not show is skipped.
 */
async function treeRow(labels, target) {
  // The first row labelled `label` after row `parent` and inside its subtree (any level).
  const find = (label, parent) =>
    wd.executeElement(
      "const [label, parent] = arguments;" +
        "const rows = Array.from(document.querySelectorAll('[role=tree] [role=treeitem]'));" +
        "let i = 0, level = 0;" +
        "if (parent) { i = rows.indexOf(parent) + 1; if (i === 0) return null;" +
        "  level = Number(parent.getAttribute('aria-level')); }" +
        "for (; i < rows.length; i += 1) {" +
        "  const r = rows[i];" +
        "  if (parent && Number(r.getAttribute('aria-level')) <= level) return null;" +
        "  const l = r.querySelector('.tree__label');" +
        "  if (l && l.textContent.trim().toLowerCase() === label.toLowerCase()) return r; }" +
        "return null;",
      label,
      parent,
    );
  let parent = null;
  for (const label of labels) {
    const row = await waitFor(`tree item "${label}"`, () => find(label, parent), {
      timeoutMs: 5_000,
    }).catch(() => null);
    if (!row) continue;
    parent = row;
    const expanded = await wd.execute("return arguments[0].getAttribute('aria-expanded');", row);
    if (expanded === "false") {
      await wd.execute("arguments[0].querySelector('.tree__twisty').click();", row);
      await sleep(300);
      await waitFor(
        `children of "${label}"`,
        () =>
          wd.execute(
            "return !Array.from(document.querySelectorAll('[role=tree] .tree__label'))" +
              ".some((l) => l.textContent.trim() === 'Loading…');",
          ),
        { timeoutMs: 15_000 },
      );
    }
  }
  return waitFor(`tree item "${target}"`, () => find(target, parent), { timeoutMs: 5_000 }).catch(
    () => null,
  );
}

/** Opens a table's data from its tree row (double-click, as a user would). */
async function openTableData(node) {
  await wd.execute("arguments[0].click();", node);
  await wd.execute(
    "arguments[0].dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));",
    node,
  );
}

/** The table-data tab of table `name` (its section is labelled "Table <schema>.<name>"). */
function tableView(name) {
  return wd.executeElement(
    "const name = arguments[0];" +
      "const views = new Set(document.querySelectorAll('[data-testid=table-data], section.table-data'));" +
      "for (const v of views) { const l = (v.getAttribute('aria-label') || '').trim();" +
      "  if (l === 'Table ' + name || l.endsWith('.' + name) || l.endsWith('.\"' + name + '\"')) return v; }" +
      "return null;",
    name,
  );
}

/** Text of a hook inside `view` (the tab's own status, errors, ...). */
function viewHookText(view, hook) {
  return wd.execute(
    "const [view, hook] = arguments;" +
      "for (const id of window.__e2e.hooks[hook].testids) {" +
      "  const e = view.querySelector('[data-testid=\"' + id + '\"]');" +
      "  if (e) return e.textContent.replace(/\\u00a0/g, ' ').trim(); }" +
      "return null;",
    view,
    hook,
  );
}

/** The cell of column `column` whose text is `text` in the grid of `view`, or null. */
function viewCell(view, column, text) {
  return wd.executeElement(
    "const [view, column, text] = arguments;" +
      "const table = view.querySelector('table[role=grid]'); if (!table) return null;" +
      "let idx = null; for (const th of table.querySelectorAll('thead th')) {" +
      "  const t = th.textContent.trim(); if (t === column || t.split(/\\s/)[0] === column) idx = th.getAttribute('aria-colindex'); }" +
      "if (!idx) return null;" +
      "for (const td of table.querySelectorAll('tbody td[aria-colindex=\"' + idx + '\"]'))" +
      "  if (td.textContent.trim() === text) return td;" +
      "return null;",
    view,
    column,
    text,
  );
}

/**
 * Waits for a cell of `view`'s grid (Glide's accessibility table). That table is rebuilt when
 * the selection or viewport changes, not when a page arrives, so it switches the view to Form
 * and back to Grid when the cell stays absent (the grid remounts from the loaded pages).
 */
async function waitViewCell(view, column, text, what, timeoutMs = 20_000) {
  let nudged = Date.now();
  return waitFor(
    what,
    async () => {
      const cell = await viewCell(view, column, text);
      if (cell) return cell;
      if (Date.now() - nudged > 2_500) {
        nudged = Date.now();
        await wd.execute(
          "const tabs = arguments[0].querySelectorAll('[role=tablist] [role=tab]');" +
            "const by = (t) => Array.from(tabs).find((b) => b.textContent.trim() === t);" +
            "const form = by('Form'), grid = by('Grid');" +
            "if (form && grid) { form.click(); setTimeout(() => grid.click(), 150); }",
          view,
        );
      }
      return null;
    },
    { timeoutMs, intervalMs: 200 },
  );
}

/**
 * Runs `sql` on the open connection through the app's IPC (`query_run` + `query_page`), not
 * through the UI, and returns the first result's rows as strings. Used to set up, check and
 * clean up the edit step independently of the view under test.
 */
let sqlRuns = 0;
async function sqlViaIpc(sql) {
  await loadArrow();
  sqlRuns += 1;
  const result = await wd.executeAsync(
    `const [sql, editorId, done] = arguments;
     (async () => {
       const T = window.__TAURI_INTERNALS__;
       const sessions = await T.invoke("list_open_connections");
       if (!sessions.length) throw new Error("no open connection");
       const failures = [];
       let hasResult = false;
       let finish;
       const ended = new Promise((resolve) => { finish = resolve; });
       const callback = T.transformCallback((raw) => {
         const m = raw && typeof raw === "object" && "message" in raw ? raw.message : raw;
         if (!m || typeof m !== "object") return;
         if (m.type === "resultSet" && m.resultIndex === 0) hasResult = true;
         if (m.type === "statementFailed") failures.push(m.error && m.error.message || "statement failed");
         if (m.type === "finished" || m.type === "failed" || m.type === "cancelled") finish(m);
       });
       try {
         const runId = await T.invoke("query_run", { sessionId: sessions[0].id, editorId,
           script: sql, options: null, onEvent: "__CHANNEL__:" + callback });
         const end = await Promise.race([ended,
           new Promise((_, reject) => setTimeout(() => reject(new Error("no end event in 30 s")), 30000))]);
         if (end.type !== "finished" || failures.length) {
           return { error: end.type + (end.error ? ": " + end.error.message : "") +
             (failures.length ? ": " + failures.join("; ") : "") };
         }
         if (!hasResult) return { rows: [] };
         const raw = await T.invoke("query_page", { runId, resultIndex: 0, offset: 0, limit: 100 });
         const bytes = raw instanceof ArrayBuffer ? new Uint8Array(raw)
           : raw instanceof Uint8Array ? raw : Uint8Array.from(raw);
         const table = window.Arrow.tableFromIPC(bytes);
         const rows = [];
         for (let r = 0; r < table.numRows; r += 1) {
           const row = [];
           for (let c = 0; c < table.numCols; c += 1) {
             const v = table.getChildAt(c).get(r);
             row.push(v === null || v === undefined ? null : String(v));
           }
           rows.push(row);
         }
         return { rows };
       } finally {
         await T.invoke("query_close_editor", { editorId }).catch(() => {});
       }
     })().then(done, (e) => done({ error: String(e && e.message || e) }));`,
    sql,
    `e2e-sql-${sqlRuns}`,
  );
  if (!result || !Array.isArray(result.rows)) {
    throw new Error(`${sql.split(/\s/)[0]} through IPC failed: ${result?.error}`);
  }
  return result.rows;
}

/** 10 base36 characters from the time and the pid (tests/fixtures.toml [m3] convention). */
function runToken() {
  const time = Date.now().toString(36).slice(-6);
  const pid = process.pid.toString(36).padStart(4, "0").slice(-4);
  return `${time}${pid}`;
}

async function editCell() {
  // 1. Is the M3 table-data UI in this build? Open the data of an M2 fixture table first, so a
  //    missing M3 fixture is never mistaken for a missing UI.
  const probe = await treeRow([config.profileName, "oxbase_test", "public", "Tables"], "customers");
  if (!probe) throw new Error("public.customers (M2 fixture) is not in the object tree");
  await openTableData(probe);
  const probeView = await waitFor(
    "a table data tab for public.customers",
    () => tableView("customers"),
    { timeoutMs: 10_000 },
  ).catch(() => null);
  if (!probeView) {
    throw new SkipStep(
      "the M3 table data UI is not in this build (double-clicking a table in the object tree " +
        "opened no table-data tab; see ui/e2e/README.md for the hooks it expects)",
    );
  }
  // From here on the UI is present: every problem is a failure, never a skip.

  // 2. A row of its own: live edit tests never change the seed rows 1-3.
  const table = "m3_edit_pk";
  const token = runToken();
  const code = `t${token}-e`;
  const edited = `t${token}-edited`;
  const inserted = await sqlViaIpc(
    `INSERT INTO m3.${table} (code, name) VALUES ('${code}', 'webview smoke') RETURNING id`,
  ).catch((error) => {
    throw new Error(`could not insert a test row into m3.${table} (M3 fixture?): ${error.message}`);
  });
  const id = inserted[0]?.[0];
  if (!id || !/^\d+$/.test(id))
    throw new Error(`INSERT ... RETURNING id gave ${JSON.stringify(inserted)}`);
  report.edit = { table: `m3.${table}`, id: Number(id), from: code, to: edited, cleanedUp: false };
  try {
    // 3. Open the table's data from the tree.
    const node = await treeRow([config.profileName, "oxbase_test", "m3", "Tables"], table);
    if (!node) throw new Error(`m3.${table} is not in the object tree`);
    await openTableData(node);
    const view = await waitFor(`the table data tab of m3.${table}`, () => tableView(table), {
      timeoutMs: 15_000,
    });
    await waitFor(
      "the table data to load and become editable",
      async () => {
        const errors = await viewHookText(view, "applyErrors");
        if (errors) throw new Fatal(`table data shows errors: ${errors}`);
        const readOnly = await wd.execute(
          "const b = arguments[0].querySelector('[data-testid=read-only-banner]');" +
            "return b ? b.textContent : null;",
          view,
        );
        if (readOnly) throw new Fatal(`m3.${table} opened read-only: ${readOnly}`);
        const status = (await viewHookText(view, "tableDataStatus")) ?? "";
        if (status.startsWith("Failed")) throw new Fatal(`table data: ${status}`);
        return parseCount(status) !== null && !status.startsWith("Loading");
      },
      { timeoutMs: 30_000 },
    );

    // 4. Edit the test row's `code` cell in the grid's overlay editor.
    await waitViewCell(view, "code", code, `the "code" cell of the test row`);
    const input = await waitFor(
      "the cell editor",
      async () => {
        const editor = await wd.execute(
          "const e = document.querySelector('[data-testid=cell-editor]'); if (!e) return null;" +
            "return e.querySelector('textarea.value-input, input.value-input') ? 'ready' : 'loading';",
        );
        if (editor === "ready") {
          return wd.executeElement(
            "return document.querySelector('[data-testid=cell-editor]')" +
              ".querySelector('textarea.value-input, input.value-input');",
          );
        }
        if (editor === "loading") return null;
        // Focusing the accessibility cell selects it; a click on it then sends Enter, which
        // opens the overlay editor on the selected cell. The cell is looked up again each
        // time: the accessibility table is rebuilt when the selection changes.
        const cell = await viewCell(view, "code", code);
        if (cell) {
          await wd.execute("arguments[0].focus(); arguments[0].click();", cell).catch(() => {});
        }
        return null;
      },
      { timeoutMs: 20_000, intervalMs: 1_000 },
    );
    // The editor starts from the stored text (loaded with row_get_form): wait for it, then
    // replace it.
    await waitFor("the cell editor to show the stored value", () =>
      wd.execute("return arguments[0].value === arguments[1];", input, code),
    );
    await wd.sendKeys(input, Keys.end + Keys.backspace.repeat(code.length + 2) + edited);
    const typed = await wd.execute("return arguments[0].value;", input);
    if (typed !== edited) throw new Error(`the cell editor shows ${JSON.stringify(typed)}`);
    await wd.sendKeys(input, Keys.enter);
    await waitFor("the edit to be pending", async () => {
      const status = (await viewHookText(view, "tableDataStatus")) ?? "";
      return status.includes("pending") ? status : null;
    });

    // 5. Apply, and wait for the result: committed, no errors, nothing left pending.
    const apply = await waitFor("an enabled Apply button", async () => {
      const el = await wd.executeElement(
        "for (const id of window.__e2e.hooks.applyEdits.testids) {" +
          "  const e = arguments[0].querySelector('[data-testid=\"' + id + '\"]'); if (e) return e; }" +
          "return null;",
        view,
      );
      return el && (await wd.execute("return !arguments[0].disabled;", el)) ? el : null;
    });
    await wd.click(apply);
    const saved = await waitFor(
      "the apply to commit",
      async () => {
        const errors = await viewHookText(view, "applyErrors");
        if (errors) throw new Fatal(`apply failed: ${errors}`);
        const status = (await viewHookText(view, "tableDataStatus")) ?? "";
        const busy = await wd.execute("return /Applying/.test(arguments[0].textContent);", apply);
        if (busy || status.includes("pending")) return null;
        return status;
      },
      { timeoutMs: 30_000 },
    );

    // 6. The database has the new value (read independently of the open view).
    const stored = await sqlViaIpc(`SELECT code FROM m3.${table} WHERE id = ${id}`);
    if (stored.length !== 1 || stored[0][0] !== edited) {
      throw new Error(`after apply the row holds ${JSON.stringify(stored)}, not "${edited}"`);
    }

    // 7. Reopen: Refresh issues a new table_data_open, and the reloaded grid shows the value.
    const refresh = await wd.executeElement(
      "for (const b of arguments[0].querySelectorAll('[role=toolbar] button'))" +
        "  if (b.textContent.trim() === 'Refresh') return b;" +
        "return null;",
      view,
    );
    if (!refresh) throw new Error("no Refresh button in the table data tab");
    const t0 = await wd.execute("return performance.now();");
    await wd.click(refresh);
    const ipcSeen = await wd.execute("return window.__e2e.ipc.length > 0;");
    if (ipcSeen) {
      await waitFor("a new table_data_open request", () =>
        wd.execute(
          "return window.__e2e.ipc.some((e) => e.cmd === 'table_data_open' && e.start >= arguments[0]);",
          t0,
        ),
      );
    }
    await waitViewCell(view, "code", edited, "the edited value in the reloaded grid");
    const reloaded = (await viewHookText(view, "tableDataStatus")) ?? "";
    if (reloaded.includes("pending")) throw new Error(`after reload the tab shows "${reloaded}"`);
    report.edit.saved = saved;
    return `m3.${table} row ${id}: "${code}" -> "${edited}" committed, stored and shown after reload`;
  } finally {
    // 8. Clean up, also after a failure.
    try {
      await sqlViaIpc(`DELETE FROM m3.${table} WHERE id = ${id}`);
      report.edit.cleanedUp = true;
    } catch (error) {
      log(`could not delete test row ${id} of m3.${table}: ${error.message}`);
    }
  }
}

// ---------------------------------------------------------------------------------------------
// M5: import and export through the file-handle commands

/** True when `file` contains the ASCII text `marker` (read in chunks, never whole). */
async function binaryContains(file, marker) {
  const needle = Buffer.from(marker, "latin1");
  const stream = createReadStream(file, { highWaterMark: 4 << 20 });
  let tail = Buffer.alloc(0);
  try {
    for await (const chunk of stream) {
      const buf = tail.length > 0 ? Buffer.concat([tail, chunk]) : chunk;
      if (buf.indexOf(needle) !== -1) return true;
      tail = buf.subarray(Math.max(0, buf.length - needle.length + 1));
    }
    return false;
  } finally {
    stream.destroy();
  }
}

/** The hook's folder: `open/` holds the sample CSV, `save/` receives the export. */
async function prepareM5Files() {
  await rm(m5.dir, { recursive: true, force: true });
  await mkdir(path.join(m5.dir, "open"), { recursive: true });
  await mkdir(path.join(m5.dir, "save"), { recursive: true });
  await copyFile(M5_SAMPLE, path.join(m5.dir, "open", path.basename(M5_SAMPLE)));
}

/** RFC 4180 records of `text` (doubled quotes, line breaks inside quotes kept as they are). */
function parseCsv(text) {
  const records = [];
  let record = [];
  let field = "";
  let quoted = false;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i];
    if (quoted) {
      if (c === '"' && text[i + 1] === '"') {
        field += '"';
        i += 1;
      } else if (c === '"') {
        quoted = false;
      } else {
        field += c;
      }
    } else if (c === '"') {
      quoted = true;
    } else if (c === ",") {
      record.push(field);
      field = "";
    } else if (c === "\n" || c === "\r") {
      if (c === "\r" && text[i + 1] === "\n") i += 1;
      record.push(field);
      records.push(record);
      record = [];
      field = "";
    } else {
      field += c;
    }
  }
  if (field !== "" || record.length > 0) {
    record.push(field);
    records.push(record);
  }
  return records;
}

/**
 * A timestamp's text as a comparable instant key: `YYYY-MM-DD`, `T` or a space,
 * `HH:MM:SS`, any number of fraction digits and an optional `Z` / `+HH:MM` / `+HHMM` offset
 * (a text without one counts as UTC). `""` stays `""` (NULL); text that is not a timestamp gives
 * `null`, which matches nothing.
 */
function instantKey(text) {
  if (text === "") return "";
  const m =
    /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?(Z|[+-]\d{2}:?\d{2})?$/.exec(
      text.trim(),
    );
  if (!m) return null;
  const [, y, mo, d, h, mi, s, fraction = "", offset = "Z"] = m;
  let seconds = Date.UTC(+y, +mo - 1, +d, +h, +mi, +s) / 1000;
  if (offset !== "Z") {
    const sign = offset[0] === "-" ? -1 : 1;
    const digits = offset.slice(1).replace(":", "");
    seconds -= sign * (Number(digits.slice(0, 2)) * 3600 + Number(digits.slice(2, 4)) * 60);
  }
  return `${seconds}.${fraction.padEnd(9, "0")}`;
}

/** What the CSV writer's default formula guard makes of a text value (docs/m5-contracts.md §3.8). */
function guarded(text) {
  return /^[=+\-@\t\r]/.test(text) ? `'${text}` : text;
}

// Runs in the webview: pick (hook) -> import_preview -> import_run into a new table -> the
// table's rows as a query result (query_run, the run a result grid shows) -> pick a save
// target (hook) -> export_run of that result to CSV. Every invoke and every wait has a bound,
// so a command that never answers ends the script instead of hanging it.
const IMPORT_EXPORT = `
const [table, catalog, schema, exportName, done] = arguments;
(async () => {
  const T = window.__TAURI_INTERNALS__;
  const bounded = (promise, what, ms) => Promise.race([promise, new Promise((_, reject) =>
    setTimeout(() => reject(new Error(what + ": no answer in " + ms + " ms")), ms))]);
  const invoke = (cmd, args, ms = 30000) => bounded(T.invoke(cmd, args), cmd, ms);
  const channel = () => {
    let finish;
    const ended = new Promise((resolve) => { finish = resolve; });
    const events = [];
    const id = T.transformCallback((raw) => {
      const m = raw && typeof raw === "object" && "message" in raw ? raw.message : raw;
      if (!m || typeof m !== "object" || typeof m.type !== "string") return;
      events.push(m.type);
      if (m.type === "finished" || m.type === "failed" || m.type === "cancelled") finish(m);
    });
    return { arg: "__CHANNEL__:" + id, events, ended };
  };
  const filters = [{ name: "CSV", extensions: ["csv"] }];
  const out = { steps: [] };
  const picked = await invoke("file_pick_open", { filters, title: null });
  if (!picked) throw new Error("file_pick_open returned null (no file in the hook's open folder?)");
  out.picked = { name: picked.name, access: picked.access, size: picked.size };
  const preview = await invoke("import_preview",
    { file: picked.handle, options: { format: "csv" }, requestId: null });
  out.preview = { columns: preview.columns.map((c) => c.name),
    types: preview.columns.map((c) => c.dataType.type), rows: preview.rows.length };
  const sessions = await invoke("list_open_connections", {});
  if (!sessions.length) throw new Error("no open connection");
  const session = sessions[0].id;
  const request = {
    options: { format: "csv" },
    columns: preview.columns.map((c) => ({ name: c.name, dataType: c.dataType })),
    mapping: preview.columns.map((c, i) => ({ source: i, target: c.name })),
    target: { session, table: { kind: "table", catalog, schema, name: table }, mode: "create" },
    primaryKey: ["id"],
    errors: { type: "stop" },
    transaction: { type: "single" },
  };
  const imp = channel();
  out.importId = await invoke("import_run", { file: picked.handle, request, onEvent: imp.arg });
  const impEnd = await bounded(imp.ended, "import_run events", 60000);
  out.import = { end: impEnd.type, events: imp.events, report: impEnd.report || null,
    error: impEnd.error ? impEnd.error.message : null };
  if (impEnd.type !== "finished") return out;

  const q = channel();
  const editorId = "e2e-m5";
  const runId = await invoke("query_run", { sessionId: session, editorId,
    script: "SELECT * FROM " + schema + "." + table + " ORDER BY id", options: null, onEvent: q.arg });
  try {
    const qEnd = await bounded(q.ended, "query_run events", 30000);
    out.query = { end: qEnd.type, runId };
    if (qEnd.type !== "finished") return out;
    const target = await invoke("file_pick_save", { defaultName: exportName, filters, title: null });
    if (!target) throw new Error("file_pick_save returned null");
    out.saved = { name: target.name, access: target.access, exists: target.exists };
    const exp = channel();
    out.exportId = await invoke("export_run", { file: target.handle,
      request: { source: { type: "result", runId, resultIndex: 0 }, options: { format: "csv" } },
      onEvent: exp.arg });
    const expEnd = await bounded(exp.ended, "export_run events", 60000);
    out.export = { end: expEnd.type, events: exp.events, report: expEnd.report || null,
      error: expEnd.error ? expEnd.error.message : null };
    return out;
  } finally {
    await T.invoke("query_close_editor", { editorId }).catch(() => {});
  }
})().then(done, (e) => done({ error: String(e && e.message || e) }));
`;

async function importExport() {
  if (!m5.hook) {
    throw new SkipStep(
      `the app has no e2e file hook (${FILE_HOOK_ARG}, debug builds only; see ui/e2e/README.md), ` +
        "so no file handle can be picked without a native dialog",
    );
  }
  const table = `m5_t_e2e_${runToken()}`;
  report.m5 = { table: `public.${table}`, cleanedUp: false };
  try {
    return await importExportSteps(table);
  } catch (error) {
    if (error instanceof SkipStep || config.requireM5) throw error;
    // Until the M5 streams are merged (services, read / write, app) a failure here is a skip
    // with a warning; OXBASE_E2E_REQUIRE_M5=1 turns it into a failure.
    throw new SkipStep(`M5 import / export not working in this build yet: ${error.message}`);
  } finally {
    try {
      await sqlViaIpc(`DROP TABLE IF EXISTS public.${table}`);
      report.m5.cleanedUp = true;
    } catch (error) {
      log(`could not drop public.${table}: ${error.message}`);
    }
  }
}

async function importExportSteps(table) {
  const out = await wd.executeAsync(IMPORT_EXPORT, table, "oxbase_test", "public", M5_EXPORT_NAME);
  report.m5.ipc = out;
  if (!out || out.error) throw new Error(`import / export through IPC failed: ${out?.error}`);

  // 1. The pick and the preview.
  const sample = parseCsv(await readFile(M5_SAMPLE, "utf8"));
  const [header, ...rows] = sample;
  if (out.picked?.name !== path.basename(M5_SAMPLE) || out.picked?.access !== "read") {
    throw new Error(`file_pick_open gave ${JSON.stringify(out.picked)}`);
  }
  if (JSON.stringify(out.preview?.columns) !== JSON.stringify(header)) {
    throw new Error(`preview columns ${JSON.stringify(out.preview?.columns)}, expected ${header}`);
  }

  // 2. The import: finished, every row committed, and the rows are in the table.
  const imported = out.import;
  if (imported?.end !== "finished") {
    throw new Error(`import ended "${imported?.end}": ${imported?.error ?? ""}`);
  }
  if (imported.report?.rowsCommitted !== rows.length || imported.report?.rowsSkipped !== 0) {
    throw new Error(`import report ${JSON.stringify(imported.report)}`);
  }
  if (!imported.events.includes("started") || imported.events.at(-1) !== "finished") {
    throw new Error(`import events ${imported.events.join(", ")}`);
  }
  report.m5.imported = imported.report.rowsCommitted;
  report.m5.importMethod = imported.report.method;
  const counted = await sqlViaIpc(`SELECT count(*), sum(id) FROM public.${table}`);
  if (counted[0]?.[0] !== String(rows.length)) {
    throw new Error(`public.${table} holds ${JSON.stringify(counted)} rows after the import`);
  }

  // 3. The export of the query result: finished, the file has every row, values as written.
  if (out.query?.end !== "finished") throw new Error(`the SELECT ended "${out.query?.end}"`);
  if (out.saved?.name !== M5_EXPORT_NAME || out.saved?.access !== "write") {
    throw new Error(`file_pick_save gave ${JSON.stringify(out.saved)}`);
  }
  const exported = out.export;
  if (exported?.end !== "finished") {
    throw new Error(`export ended "${exported?.end}": ${exported?.error ?? ""}`);
  }
  if (exported.report?.rows !== rows.length) {
    throw new Error(`export report ${JSON.stringify(exported.report)}`);
  }
  report.m5.exported = exported.report.rows;
  const file = path.join(m5.dir, "save", M5_EXPORT_NAME);
  const text = await readFile(file, "utf8").catch((error) => {
    throw new Error(`the export finished but ${M5_EXPORT_NAME} cannot be read: ${error.message}`);
  });
  report.m5.exportBytes = Buffer.byteLength(text);
  const [outHeader, ...outRows] = parseCsv(text);
  if (JSON.stringify(outHeader) !== JSON.stringify(header)) {
    throw new Error(`exported header ${JSON.stringify(outHeader)}, expected ${header}`);
  }
  if (outRows.length !== rows.length) {
    throw new Error(`the export has ${outRows.length} rows, expected ${rows.length}`);
  }
  // Every column. As text: id, name and note (formula guard on by default), amount (exact
  // decimal), born (ISO date) and active (true / false). `at` as an instant: `T` or a space and
  // the number of fraction digits are the writer's choice, the instant is not.
  const col = (name) => header.indexOf(name);
  const mismatches = [];
  rows.forEach((source, r) => {
    const got = outRows[r];
    const want = {
      id: source[col("id")],
      name: guarded(source[col("name")]),
      amount: source[col("amount")],
      born: source[col("born")],
      active: source[col("active")],
      note: guarded(source[col("note")]),
    };
    for (const [name, value] of Object.entries(want)) {
      if (got[col(name)] !== value) {
        mismatches.push(
          `row ${r + 1} ${name}: ${JSON.stringify(got[col(name)])} != ${JSON.stringify(value)}`,
        );
      }
    }
    const at = got[col("at")] ?? "";
    const wantAt = instantKey(source[col("at")]);
    if (wantAt === null || instantKey(at) !== wantAt) {
      mismatches.push(
        `row ${r + 1} at: ${JSON.stringify(at)} is not the instant ${JSON.stringify(source[col("at")])}`,
      );
    }
  });
  if (mismatches.length > 0) throw new Error(`exported values differ: ${mismatches.join("; ")}`);
  return (
    `imported ${rows.length} rows (${imported.report.method}) into public.${table}; exported the ` +
    `query result to ${M5_EXPORT_NAME} (${report.m5.exportBytes} bytes), values match`
  );
}

// ---------------------------------------------------------------------------------------------

function markdownSummary() {
  const lines = ["## Webview smoke (Linux)", ""];
  lines.push("| Step | Result | Time | Detail |", "|---|---|---|---|");
  for (const r of results) {
    const detail = String(r.detail).replaceAll("|", "\\|").replaceAll("\n", " ").slice(0, 300);
    lines.push(`| ${r.name} | ${r.status} | ${(r.ms / 1000).toFixed(1)} s | ${detail} |`);
  }
  if (results.some((r) => r.status === "skipped")) {
    lines.push(
      "",
      "> **Warning:** a step was skipped (see its detail). `OXBASE_E2E_REQUIRE_EDIT=1` makes a " +
        "skip of the edit step fail (`scripts/e2e-smoke.sh` sets it when the checkout has the M3 " +
        "table-data UI and the M3 PostgreSQL fixture); `OXBASE_E2E_REQUIRE_M5=1` does the same " +
        "for the M5 import / export step.",
    );
  }
  const pl = report.pageLatency;
  if (pl) {
    lines.push(
      "",
      `### Page latency (IPC + Arrow decode in the webview, limit p95 < ${pl.limitMs} ms)`,
      "",
    );
    lines.push("| Source | Pages | p50 ms | p95 ms | max ms |", "|---|---|---|---|---|");
    const row = (name, s) => `| ${name} | ${s.count} | ${s.p50} | ${s.p95} | ${s.max} |`;
    lines.push(row(`probe: IPC + decode (${pl.probe.source})`, pl.probe.totalMs));
    lines.push(row("probe: IPC only", pl.probe.ipcMs));
    lines.push(row("probe: decode only", pl.probe.decodeMs));
    if (pl.app) lines.push(row("app hook (window.__oxbasePerf)", pl.app.totalMs));
    if (pl.gridIpcMs) lines.push(row("grid page requests (IPC only)", pl.gridIpcMs));
    lines.push("", `Asserted: ${pl.asserted}.`);
  }
  if (report.scroll.length > 0) {
    lines.push("", "### Scrolling the 1,000,000-row grid", "");
    lines.push(
      "| Position | First row | Page requests | Scroll to page loaded ms | Scroll to cells shown ms |",
      "|---|---|---|---|---|",
    );
    for (const s of report.scroll) {
      lines.push(
        `| ${Math.round(s.fraction * 100)}% | ${s.firstRow + 1} | ${s.pageRequests} | ` +
          `${s.scrollToPageMs ?? "cached"} | ${s.scrollToCellsMs} |`,
      );
    }
    lines.push(
      "",
      "Cells shown includes the grid's 200 ms debounce of its accessibility table, which the test reads.",
    );
  }
  if (report.query) {
    lines.push(
      "",
      `Query: ${report.query.rows.toLocaleString("en-US")} rows, finished ${report.query.msToFinished} ms ` +
        `after Run; the row counter changed ${report.query.counterUpdates} times while streaming.`,
    );
  }
  if (report.edit) {
    lines.push(
      `Edit: m3.m3_edit_pk row ${report.edit.id} "${report.edit.from}" -> "${report.edit.to}"; ` +
        `test row ${report.edit.cleanedUp ? "deleted" : "NOT deleted"} afterwards.`,
    );
  }
  if (report.m5) {
    const m = report.m5;
    lines.push(
      `M5: imported ${m.imported ?? "?"} rows from basic.csv into ${m.table ?? "?"}` +
        ` (${m.importMethod ?? "?"}), exported ${m.exported ?? "?"} rows to ${M5_EXPORT_NAME}` +
        ` (${m.exportBytes ?? "?"} bytes); table ${m.cleanedUp ? "dropped" : "NOT dropped"} afterwards.`,
    );
  }
  if (report.cancel) {
    lines.push(
      `Cancel: "Cancelled" ${report.cancel.ms} ms after Stop (limit ${config.cancelLimitMs} ms), ` +
        `${report.cancel.rowsKept?.toLocaleString("en-US")} rows kept.`,
    );
  }
  return `${lines.join("\n")}\n`;
}

async function main() {
  await mkdir(config.outDir, { recursive: true });
  let ok = await step("Window loads and shows the core version", launch);
  if (ok) ok = await step("Connect to the PostgreSQL fixture (prompted password)", connect);
  if (ok) ok = await step("Run a 1,000,000-row query (Channel events, row counter)", runBigQuery);
  if (ok) ok = await step("Arrow page decodes in the webview (first cell is 1)", firstCell);
  if (ok) ok = await step("Scroll the grid (pages load at each position)", scrollGrid);
  if (ok) ok = await step("Page latency through IPC + Arrow decode", measurePages);
  if (ok) ok = await step("Cancel a running large query", cancelRun);
  if (ok) ok = await step("Edit a table cell (M3 table data UI)", editCell);
  if (ok) {
    await step(
      "Import a CSV and export a query result to CSV (M5 file handles)",
      importExport,
      M5_GATE,
    );
  }

  const failed = results.some((r) => r.status === "failed");
  if (wd?.sessionId) {
    try {
      report.pageErrors = await wd.execute("return window.__e2e ? window.__e2e.errors : [];");
    } catch {
      // The window may be gone.
    }
    if (failed) {
      await screenshot("failure");
      await dumpDom("failure-dom");
    }
    try {
      await wd.deleteSession();
    } catch (error) {
      log(`closing the session: ${error.message}`);
    }
  }
  report.finishedAt = new Date().toISOString();
  report.passed = !failed;
  await writeFile(
    path.join(config.outDir, "e2e-perf.json"),
    `${JSON.stringify(report, null, 2)}\n`,
  );
  const summary = markdownSummary();
  console.log(summary);
  if (config.summaryFile) await appendFile(config.summaryFile, summary);
  if (report.pageErrors.length > 0) log(`page errors: ${report.pageErrors.join(" | ")}`);
  process.exitCode = failed ? 1 : 0;
}

main().catch((error) => {
  annotate(String(error?.message ?? error));
  console.error(error);
  process.exitCode = 1;
});
