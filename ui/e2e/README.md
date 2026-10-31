# Webview smoke test

`ui/e2e/smoke.mjs` drives the built desktop app through its real webview, the way a user
would, and checks the paths that unit tests cannot reach: the Tauri IPC, the `Channel` events,
the binary `query_page` responses and their Arrow decoding inside WebKitGTK. The contract is
in `docs/m3-contracts.md` §9. CI runs it as the job **Webview smoke (Linux)** in
`.github/workflows/ci.yml`. The job runs on the full lane and on the fast lane of the
`wip/*-e2e` and `wip/*-app` branches.

## What it checks

1. **The window loads.** The status bar shows `oxbase core <workspace version>`.
2. **Connect.** The test uses a saved PostgreSQL profile whose password is set to _prompt_. It
   selects the profile, clicks Connect and types the fixture password into the app's prompt.
3. **A 1,000,000-row query.** It runs `SELECT g, md5(g::text) FROM generate_series(1, 1000000) g`
   from a new query editor. The row counter, fed by the run's `Channel` events, must reach
   1,000,000 and the run must finish. The summary reports how many times the counter changed
   while the rows streamed.
4. **Arrow decodes in the webview.** The grid's first row shows `g = 1` and the md5 of `1`.
5. **Scrolling.** The grid is scrolled to 25 %, 50 %, 75 %, the end, 10 %, 90 % and back to
   the top. At each position the first visible row must show its own value (row _r_ holds
   _r + 1_), so the right page was loaded. The test records how long after the scroll the
   covering `query_page` response arrived.
6. **Page latency.** The test sends 60 `query_page` requests of 200 rows at random offsets
   from inside the webview and decodes each one with `apache-arrow`. Each page is timed from
   the request to the decoded table. If the app provides `window.__oxbasePerf.pages`
   (`{offset, limit, ipcMs, decodeMs}[]`, filled by the grid's page fetcher), the test asserts
   on those numbers instead. The step fails when p95 is above 250 ms
   (`OXBASE_E2E_P95_LIMIT_MS`). p50, p95 and max go to the job summary and to `e2e-perf.json`.
7. **Cancel.** The test starts a query that streams 100,000,000 rows and clicks Stop once it
   has 100,000 rows. The run must end as _Cancelled_ within 2 s of the click
   (`OXBASE_E2E_CANCEL_LIMIT_MS`).
8. **Edit a cell (M3).** The test inserts a row of its own into `m3.m3_edit_pk`, so seed rows
   1-3 are never changed. It opens the table's data from the object tree, edits the row's
   `code` cell, applies the change, reopens the table and looks for the new value. The step is
   **skipped, with a message**, when the M3 fixtures or the M3 table-data UI are not in the
   build.

How the test finds things in the UI: it first looks for a `data-testid`, then falls back to
the M2 markup, so it works both before and after the app adds its test ids. The test ids are
listed in `HOOKS` at the top of `smoke.mjs`:

| Hook                   | `data-testid`                                                 | M2 fallback                                                   |
| ---------------------- | ------------------------------------------------------------- | ------------------------------------------------------------- |
| status bar             | `status-bar`, `core-version`                                  | (none)                                                        |
| connection list / item | `connection-list`, `connection-item`                          | `role=listbox` "Saved connections"                            |
| connect button         | `connect-button`                                              | toolbar button "Connect"                                      |
| password prompt        | `password-prompt` (the input, or the dialog that contains it) | `#prompt-password`                                            |
| new query              | `new-query`                                                   | toolbar button "New query"                                    |
| SQL editor             | `sql-editor`                                                  | (none)                                                        |
| run / cancel           | `run-button`, `cancel-button`                                 | buttons "Run" / "Stop"                                        |
| run status             | `run-status`                                                  | `.query-editor__status` ("Running…", "Finished", "Cancelled") |
| row counter            | `row-counter`, `result-status`                                | (none)                                                        |
| result grid            | `result-grid`                                                 | `.results .result-grid`                                       |
| M3 table data          | `open-table-data`, `table-data`, `edit-apply`                 | double-click on the tree row                                  |

The test reads grid cells from the accessibility table that Glide Data Grid renders inside its
canvas (`table[role=grid]`). For timing, it wraps `window.fetch` for `ipc:` requests (Tauri 2
sends commands that way). The wrapper does not change the requests or their results. The app
has no test-only code path. The only setup is a temporary config directory with one saved
profile.

## Run it locally (Linux)

You need the Tauri build dependencies (`scripts/ci-install-linux-deps.sh`), Docker,
`webkit2gtk-driver` (it provides `WebKitWebDriver`), `xvfb` if you have no display, and
`tauri-driver`:

```sh
sudo apt-get install webkit2gtk-driver xvfb
cargo install tauri-driver --locked --version 2.1.0

(cd ui && pnpm install --frozen-lockfile && pnpm run build)
cargo build -p app-tauri --locked --features tauri/custom-protocol   # debug app with ui/dist embedded
bash scripts/e2e-fixtures.sh up                                     # PostgreSQL fixture only
bash scripts/e2e-smoke.sh                                           # results in target/e2e/
bash scripts/e2e-fixtures.sh down
```

`scripts/e2e-smoke.sh` creates a temporary `OXBASE_CONFIG_DIR`, `OXBASE_DATA_DIR` and `XDG_*`
directories. It writes the prompt-password profile, sets `TAURI_WEBVIEW_AUTOMATION=true` so
WebKitWebDriver can attach to the webview, starts `tauri-driver` on port 4444 and runs
`node ui/e2e/smoke.mjs`. Your own profiles, history and keychain are not touched. If `DISPLAY`
is not set, the script runs itself again under `xvfb-run`.

Output in `target/e2e/` (CI uploads it as the `webview-smoke-linux` artifact):
`e2e-perf.json` (every measurement, written every run), `webdriver.log` (tauri-driver,
WebKitWebDriver and the app's own output) and, after a failure, `failure.png` and
`failure-dom.html` (input values removed).

## Limits

- **Linux only.** macOS has no WebDriver for WKWebView, so there is no macOS run. Windows could
  use `tauri-driver` with an `msedgedriver` that matches the runner's WebView2. This is not
  wired up: the job needs the PostgreSQL fixture, and Windows runners cannot run the Linux
  container. A Windows job needs PostgreSQL installed on the runner first.
- **WebKitGTK under Xvfb uses software rendering.** The latencies are those of a CI VM, not a
  desktop, so the bounds (p95 < 250 ms per page, cancel < 2 s) are generous on purpose.
- **Grid cells are read from Glide's accessibility table.** That table is rebuilt 200 ms after
  the viewport moves, not when a page arrives, so the test moves the grid by one row when the
  cells stay empty. The "scroll to cells shown" times include that delay. The latency the test
  asserts on does not.
- **Channel events are seen through the row counter.** A very fast run may show fewer
  intermediate counts. The final count must be exact.
- The test drives one window and one query editor. It does not test the dockview layout,
  keyboard shortcuts or the dialogs other than the password prompt.
