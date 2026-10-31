#!/usr/bin/env node
// Webview smoke test (docs/m3-contracts.md §9): drives the built desktop app through its real
// webview with `tauri-driver` and checks the M2/M3 paths no unit test reaches: the window loads,
// a saved PostgreSQL connection opens with a prompted password, a 1,000,000-row query streams
// its Channel events into the grid, `query_page` Arrow bytes decode in the webview, scrolling
// loads pages, page latency (IPC + Arrow decode) stays under a bound, a running query cancels
// within 2 s and, once the M3 UI exists, a table cell can be edited.
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
//   GITHUB_STEP_SUMMARY       if set, a Markdown summary is appended to it

import { appendFile, mkdir, readFile, writeFile } from "node:fs/promises";
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
  // M3 table data and editing (app stream). Absent hooks make the edit step skip.
  openTableData: { testids: ["open-table-data"] },
  tableData: { testids: ["table-data"] },
  applyEdits: { testids: ["edit-apply"] },
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
  query: null,
  pageLatency: null,
  scroll: [],
  cancel: null,
  edit: null,
  steps: results,
  pageErrors: [],
};
let wd = null;

function log(message) {
  console.log(`[e2e] ${message}`);
}

/** A GitHub Actions error annotation (shown on the run page and by scripts/ci-wait.mjs). */
function annotate(message) {
  const escaped = message.replaceAll("%", "%25").replaceAll("\r", "%0D").replaceAll("\n", "%0A");
  console.log(`::error::Webview smoke: ${escaped}`);
}

class SkipStep extends Error {}

/** Thrown inside a `waitFor` condition to stop waiting at once. */
class Fatal extends Error {}

async function step(name, fn) {
  const started = Date.now();
  log(`--- ${name}`);
  try {
    const detail = (await fn()) ?? "";
    results.push({ name, status: "passed", ms: Date.now() - started, detail });
    log(`ok: ${name}${detail ? ` (${detail})` : ""}`);
    return true;
  } catch (error) {
    const message = String(error?.message ?? error);
    if (error instanceof SkipStep) {
      results.push({ name, status: "skipped", ms: Date.now() - started, detail: message });
      log(`skipped: ${name}: ${message}`);
      return true;
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
  return wd.execute(
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
      wd.execute(
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
  await wd.newSession({ "tauri:options": { application: config.application, args: [] } });
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
    wd.execute(
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
    return wd.execute(
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
  if (counts.length < 2) {
    log("note: the row counter changed only once; the run streamed faster than the UI redrew");
  }
  return `${final} rows; row counter updated ${counts.length} times while streaming; ${done.status}`;
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

/** Runs probe pages (`query_page` invoked from the webview, decoded with apache-arrow there). */
async function measurePages() {
  // The run behind the grid: the last page the grid requested.
  let runId = await wd.execute(
    "const p = window.__e2e.ipc.filter((e) => e.cmd === 'query_page' && e.runId);" +
      "return p.length ? p[p.length - 1].runId : null;",
  );
  // apache-arrow (the UI's own dependency, Apache-2.0) decodes the pages in the webview.
  const arrowPath = path.resolve(here, "../node_modules/apache-arrow/Arrow.es2015.min.js");
  const arrow = await readFile(arrowPath, "utf8");
  await wd.execute(`if (!window.Arrow) { (function () {\n${arrow}\n}).call(window); }`);
  const hasArrow = await wd.execute("return typeof window.Arrow?.tableFromIPC === 'function';");
  if (!hasArrow) throw new Error("could not load apache-arrow into the webview");
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

/** Expands the object tree along `labels` (labels not shown are skipped) and returns the row. */
async function treeRow(labels, target) {
  const find = (label) =>
    wd.execute(
      "for (const r of document.querySelectorAll('[role=tree] [role=treeitem]')) {" +
        "  const l = r.querySelector('.tree__label');" +
        "  if (l && l.textContent.trim().toLowerCase() === arguments[0].toLowerCase()) return r; }" +
        "return null;",
      label,
    );
  for (const label of labels) {
    const row = await waitFor(`tree item "${label}"`, () => find(label), {
      timeoutMs: 5_000,
    }).catch(() => null);
    if (!row) continue;
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
  return waitFor(`tree item "${target}"`, () => find(target), { timeoutMs: 5_000 }).catch(
    () => null,
  );
}

/** Opens a table's data from its tree row: the app's open button, else a double-click. */
async function openTableData(node) {
  await wd.execute("arguments[0].click();", node);
  const opener = await locate("openTableData");
  if (opener) {
    await wd.click(opener);
    return;
  }
  await wd.execute(
    "arguments[0].dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));",
    node,
  );
}

async function editCell() {
  const table = "m3_edit_pk";
  const code = `e2e-${Date.now().toString(36)}`;
  // A row of its own: live edit tests never change the seed rows 1-3.
  await typeSql(`INSERT INTO m3.${table} (code) VALUES ('${code}')`);
  const t0 = await clickRun();
  const inserted = (
    await waitFor("the INSERT to finish", () => endedSince(t0), { timeoutMs: 30_000 })
  ).status;
  if (!inserted.startsWith("Finished")) {
    throw new SkipStep(`could not insert a test row into m3.${table} (${inserted}); M3 fixtures?`);
  }
  const node = await treeRow([config.profileName, "oxbase_test", "m3", "Tables"], table);
  if (!node) throw new SkipStep(`m3.${table} is not in the object tree (M3 fixtures not merged?)`);
  await openTableData(node);
  const view = await waitFor("the table data view", () => locate("tableData"), {
    timeoutMs: 10_000,
  }).catch(() => null);
  if (!view) {
    throw new SkipStep(
      'the M3 table data UI is not in this build (no [data-testid="table-data"] after ' +
        "opening the table; see ui/e2e/README.md for the hooks it expects)",
    );
  }
  const edited = `${code}-edited`;
  const cell = await waitFor(
    `the "code" cell of the test row`,
    () =>
      wd.execute(
        "const view = arguments[0];" +
          "const table = view.querySelector('table[role=grid]'); if (!table) return null;" +
          "let idx = null; for (const th of table.querySelectorAll('thead th'))" +
          "  if (th.textContent.trim() === 'code') idx = th.getAttribute('aria-colindex');" +
          "if (!idx) return null;" +
          "for (const td of table.querySelectorAll('tbody td[aria-colindex=\"' + idx + '\"]'))" +
          "  if (td.textContent === arguments[1]) return td;" +
          "return null;",
        view,
        code,
      ),
    { timeoutMs: 15_000 },
  );
  // Clicking the accessibility cell opens Glide's overlay editor for it.
  await wd.execute("arguments[0].click();", cell);
  const input = await waitFor(
    "the cell editor",
    () =>
      wd.execute(
        "return document.querySelector('.gdg-growing-entry textarea, #portal textarea, " +
          ".result-grid__portal textarea, [data-testid=cell-editor] textarea, [data-testid=cell-editor] input');",
      ),
    { timeoutMs: 5_000 },
  );
  await wd.chord(Keys.control, "a");
  await wd.sendKeys(input, edited);
  await wd.sendKeys(input, Keys.enter);
  const apply = await waitFor("the Apply button", () => locate("applyEdits"), {
    timeoutMs: 5_000,
  }).catch(() => null);
  if (!apply) throw new SkipStep('no [data-testid="edit-apply"] button in the table data view');
  await wd.click(apply);
  await sleep(500);
  // Reopen and look for the new value.
  await openTableData(node);
  await waitFor(
    "the edited value after reopening",
    () =>
      wd.execute(
        "for (const v of document.querySelectorAll('[data-testid=table-data]'))" +
          "  for (const td of v.querySelectorAll('table[role=grid] tbody td'))" +
          "    if (td.textContent === arguments[0]) return true;" +
          "return false;",
        edited,
      ),
    { timeoutMs: 15_000 },
  );
  report.edit = { table: `m3.${table}`, from: code, to: edited };
  return `m3.${table}: "${code}" -> "${edited}" applied and read back`;
}

// ---------------------------------------------------------------------------------------------

function markdownSummary() {
  const lines = ["## Webview smoke (Linux)", ""];
  lines.push("| Step | Result | Time | Detail |", "|---|---|---|---|");
  for (const r of results) {
    const detail = String(r.detail).replaceAll("|", "\\|").replaceAll("\n", " ").slice(0, 300);
    lines.push(`| ${r.name} | ${r.status} | ${(r.ms / 1000).toFixed(1)} s | ${detail} |`);
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
  if (ok) await step("Edit a table cell (M3 table data UI)", editCell);

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
