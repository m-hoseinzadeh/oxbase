#!/usr/bin/env bash
# Runs the webview smoke test (ui/e2e/smoke.mjs) against a built debug app. Linux (WebKitGTK +
# WebKitWebDriver) is the supported platform; see ui/e2e/README.md for the setup and limits.
#
#   scripts/e2e-smoke.sh
#
# Expects:
#   - the app built with the UI embedded:
#       (cd ui && pnpm install --frozen-lockfile && pnpm run build)
#       cargo build -p app-tauri --locked --features tauri/custom-protocol
#   - `tauri-driver` and `WebKitWebDriver` on PATH (apt: webkit2gtk-driver);
#   - the PostgreSQL fixture running (scripts/e2e-fixtures.sh up);
#   - an X display; without DISPLAY the script re-runs itself under `xvfb-run`.
#
# Env (all optional):
#   OXBASE_E2E_APP   app binary (default target/debug/app-tauri)
#   OXBASE_E2E_OUT   output directory (default target/e2e): e2e-perf.json, logs, failure
#                    screenshot and DOM dump
#   OXBASE_E2E_P95_LIMIT_MS, OXBASE_E2E_CANCEL_LIMIT_MS   bounds (defaults 250 and 2000)
#
# The app runs with its own temporary config and data directories (OXBASE_CONFIG_DIR,
# OXBASE_DATA_DIR, XDG_*), holding one saved profile for the fixture whose password is
# "prompt": the test types the fixture password into the app's prompt. Nothing touches the
# user's profiles, history or OS keychain, and the app has no test-only code path.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# A display: Xvfb when there is none (CI). D-Bus session for GTK when there is none.
if [[ -z "${DISPLAY:-}" && -z "${OXBASE_E2E_UNDER_XVFB:-}" ]]; then
  if ! command -v xvfb-run >/dev/null 2>&1; then
    echo "e2e-smoke: no DISPLAY and no xvfb-run (apt: xvfb)" >&2
    exit 1
  fi
  export OXBASE_E2E_UNDER_XVFB=1
  exec xvfb-run --auto-servernum --server-args="-screen 0 1440x900x24" bash "$0" "$@"
fi
if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" && -z "${OXBASE_E2E_UNDER_DBUS:-}" ]] \
  && command -v dbus-run-session >/dev/null 2>&1; then
  export OXBASE_E2E_UNDER_DBUS=1
  exec dbus-run-session -- bash "$0" "$@"
fi

app="${OXBASE_E2E_APP:-$repo_root/target/debug/app-tauri}"
out="${OXBASE_E2E_OUT:-$repo_root/target/e2e}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

for tool in tauri-driver WebKitWebDriver node; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "e2e-smoke: $tool not found on PATH (see ui/e2e/README.md)" >&2
    exit 1
  fi
done
if [[ ! -x "$app" ]]; then
  echo "e2e-smoke: app binary not found: $app (see ui/e2e/README.md)" >&2
  exit 1
fi
if [[ ! -f "$repo_root/ui/node_modules/apache-arrow/Arrow.es2015.min.js" ]]; then
  echo "e2e-smoke: ui/node_modules is missing; run pnpm install in ui/" >&2
  exit 1
fi

# `key = value` of a section of tests/fixtures.toml (strings unquoted, comments dropped).
fixture_value() {
  local section="$1" key="$2"
  awk -v section="[$section]" -v key="$key" '
    /^\[/ { in_section = ($0 == section); next }
    in_section {
      line = $0
      sub(/[ \t]+#.*$/, "", line)
      if (match(line, "^[ \t]*" key "[ \t]*=[ \t]*")) {
        value = substr(line, RLENGTH + 1)
        gsub(/^"|"$/, "", value)
        print value
        exit
      }
    }' "$repo_root/tests/fixtures.toml"
}

pg_host="$(fixture_value engines.postgres host)"
pg_port="$(fixture_value engines.postgres port)"
pg_user="$(fixture_value engines.postgres user)"
pg_database="$(fixture_value engines.postgres database)"
pg_password="$(fixture_value engines.postgres password)"
if [[ -z "$pg_host" || -z "$pg_port" || -z "$pg_user" || -z "$pg_database" || -z "$pg_password" ]]; then
  echo "e2e-smoke: could not read [engines.postgres] from tests/fixtures.toml" >&2
  exit 1
fi
version="$(awk -F'"' '/^\[workspace.package\]/ { s = 1; next } /^\[/ { s = 0 } s && /^version *=/ { print $2; exit }' "$repo_root/Cargo.toml")"

work="$(mktemp -d "${TMPDIR:-/tmp}/oxbase-e2e.XXXXXX")"
driver_pid=""
cleanup() {
  if [[ -n "$driver_pid" ]] && kill -0 "$driver_pid" 2>/dev/null; then
    kill "$driver_pid" 2>/dev/null || true
    wait "$driver_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

# The app's own directories: one saved profile, password asked on connect.
umask 077
mkdir -p "$work/config" "$work/data" "$work/xdg/config" "$work/xdg/data" "$work/xdg/cache"
profile_name="E2E Postgres"
cat >"$work/config/connections.json" <<JSON
{
  "schema_version": 1,
  "profiles": [
    {
      "schema_version": 1,
      "id": "0196f000-0000-7000-8000-00000000e2e0",
      "name": "$profile_name",
      "engine": "postgres",
      "address": { "kind": "network", "host": "$pg_host", "port": $pg_port },
      "database": "$pg_database",
      "user": "$pg_user",
      "password": { "source": "prompt" },
      "tls": { "mode": "disable" }
    }
  ]
}
JSON
umask 022

export OXBASE_CONFIG_DIR="$work/config"
export OXBASE_DATA_DIR="$work/data"
export XDG_CONFIG_HOME="$work/xdg/config"
export XDG_DATA_HOME="$work/xdg/data"
export XDG_CACHE_HOME="$work/xdg/cache"
# Lets WebKitWebDriver attach to the app's webview (read by tauri-runtime-wry).
export TAURI_WEBVIEW_AUTOMATION=true
# Software rendering under Xvfb.
export WEBKIT_DISABLE_DMABUF_RENDERER=1
export LIBGL_ALWAYS_SOFTWARE=1
export NO_AT_BRIDGE=1

echo "e2e-smoke: app $app"
echo "e2e-smoke: tauri-driver $(tauri-driver --help >/dev/null 2>&1 && echo found), WebKitWebDriver $(WebKitWebDriver --version 2>/dev/null || echo '?')"
echo "e2e-smoke: output $out"

tauri-driver --port 4444 --native-port 4445 >"$out/webdriver.log" 2>&1 &
driver_pid=$!

status=0
OXBASE_E2E_APP="$app" \
  OXBASE_E2E_OUT="$out" \
  OXBASE_E2E_PROFILE_NAME="$profile_name" \
  OXBASE_E2E_VERSION="$version" \
  OXBASE_E2E_PG_PASSWORD="$pg_password" \
  OXBASE_E2E_DRIVER_URL="http://127.0.0.1:4444" \
  node "$repo_root/ui/e2e/smoke.mjs" || status=$?

if [[ "$status" -ne 0 ]]; then
  echo "e2e-smoke: FAILED (exit $status); WebDriver log: $out/webdriver.log" >&2
  echo "--- last lines of the WebDriver log ---" >&2
  tail -n 60 "$out/webdriver.log" >&2 || true
fi
exit "$status"
