# Webview smoke test

`ui/e2e/smoke.mjs` drives the built desktop app through its real webview, the way a user
would, and checks the paths that unit tests cannot reach: the Tauri IPC, the `Channel` events,
the binary `query_page` responses and their Arrow decoding inside WebKitGTK. The contract is
in `docs/m3-contracts.md` §9. CI runs it as the job **Webview smoke (Linux)** in
`.github/workflows/ci.yml`. The job runs on the full lane and on the fast lane of the
`wip/*-e2e`, `wip/*-app` and `wip/*-integrate` branches. Since M3 the job sets
`OXBASE_E2E_REQUIRE_EDIT=1`, so the edit step must run.

## What it checks

1. **The window loads.** The status bar shows `oxbase core <workspace version>`.
2. **Connect.** The test uses a saved PostgreSQL profile whose password is set to _prompt_. It
   selects the profile, clicks Connect and types the fixture password into the app's prompt.
3. **A 1,000,000-row query.** It runs `SELECT g, md5(g::text) FROM generate_series(1, 1000000) g`
   from a new query editor. The row counter, fed by the run's `Channel` events, must reach
   1,000,000 and the run must finish. The counter must also have shown at least two partial
   counts before the final one, so the `Channel` row events reached the UI while the rows
   streamed (not only the finished result).
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
8. **Edit a cell (M3).**
   - **Is the UI there?** The test double-clicks the M2 fixture table `public.customers` in the
     object tree. If no table-data tab opens, the M3 UI is not in the build and the step is
     **skipped** with a `::warning::` annotation. With `OXBASE_E2E_REQUIRE_EDIT=1` a skip
     **fails** the run instead. `scripts/e2e-smoke.sh` sets it on its own when the checkout has
     both `ui/src/workbench/data/TableDataPanel.tsx` and
     `tests/docker/init/postgres/05-m3.sql`, so it is on once the app and pg streams are merged.
   - Once the UI is detected, every later problem **fails** the step, including a missing M3
     fixture.
   - The test inserts a row of its own into `m3.m3_edit_pk` through the app's IPC
     (`query_run`). The row's `code` is `t<token>-e`, where the token is 10 base36 characters
     from the time and the pid (the `tests/fixtures.toml` `[m3]` convention). Seed rows 1-3
     are never changed.
   - It opens `m3.m3_edit_pk` from the tree (the path is followed level by level, so the `m3`
     schema's "Tables" is not confused with the `public` one). It edits the row's `code` cell in
     the grid's cell editor to `t<token>-edited` and clicks Apply.
   - It waits until the apply has finished: no `apply-errors` banner, nothing pending.
   - It reads the row back with `SELECT` through IPC, independently of the open view.
   - It clicks the tab's Refresh, which must send a new `table_data_open`, and the reloaded
     grid must show the new value.
   - Finally it deletes its row, also when the step fails.

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
| M3 table data tab      | `table-data`                                                  | `section.table-data` labelled "Table m3.m3_edit_pk"           |
| M3 tab status / errors | `table-data-status`, `apply-errors`, `read-only-banner`       | (none)                                                        |
| M3 apply / cell editor | `apply-edits` (or `edit-apply`), `cell-editor`                | toolbar button "Refresh"; tree row double-click opens a table |

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
`failure-dom.html` (input values removed). Neither tauri-driver nor WebKitWebDriver has a
verbose mode, so `webdriver.log` is usually empty. The screenshot, the DOM dump and the step
log in the job output are the evidence of a failure. No file holds the fixture password.

## Limits

- **Linux only.** macOS has no WebDriver for WKWebView, so there is no macOS run. Windows could
  use `tauri-driver` with an `msedgedriver` that matches the runner's WebView2. This is not
  wired up: the job needs the PostgreSQL fixture, and Windows runners cannot run the Linux
  container. A Windows job needs PostgreSQL installed on the runner first. The optional Windows
  job that `docs/m3-contracts.md` §9 mentions is therefore not added in M3.
- **WebKitGTK under Xvfb uses software rendering.** The latencies are those of a CI VM, not a
  desktop, so the bounds (p95 < 250 ms per page, cancel < 2 s) are generous on purpose.
- **Grid cells are read from Glide's accessibility table.** That table is rebuilt 200 ms after
  the viewport moves, not when a page arrives, so the test moves the grid by one row when the
  cells stay empty. The "scroll to cells shown" times include that delay. The latency the test
  asserts on does not.
- **Channel events are seen through the row counter.** The test needs at least two partial
  counts before the final one. A UI that shows fewer than two partial counts while the
  1,000,000 rows stream fails that check (the last green run showed about 20).
- The test drives one window and one query editor. It does not test the dockview layout,
  keyboard shortcuts or the dialogs other than the password prompt.
