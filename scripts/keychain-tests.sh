#!/usr/bin/env bash
# Run the tests that use the real OS keychain (KeyringSecretStore and the connection manager on
# top of it): `keychain_smoke` and `keychain_long_values` in oxbase-store and
# `keychain_backed_profile_round_trip` in oxbase-services.
#
#   scripts/keychain-tests.sh                 # the default command below
#   scripts/keychain-tests.sh cargo test ...  # any other command, in the same environment
#
# macOS and Windows use the user's login keychain / Credential Manager directly. On Linux the
# tests need a Secret Service: when OXBASE_KEYCHAIN_USE_SESSION=1 the current D-Bus session is
# used as is (a desktop session with an unlocked keyring). Otherwise the command runs inside a
# private D-Bus session (dbus-run-session) with its own gnome-keyring-daemon, whose keyring files
# live in a temporary directory that is deleted afterwards, so the user's keyrings are never
# touched. Needs `dbus-run-session` and `gnome-keyring-daemon`
# (Debian/Ubuntu: `apt-get install dbus gnome-keyring`).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [ "$#" -eq 0 ]; then
  set -- cargo test -p oxbase-store -p oxbase-services --locked -- --ignored keychain
fi
export OXBASE_KEYCHAIN_TESTS=1

case "$(uname -s)" in
  Linux | *BSD) ;;
  *) exec "$@" ;;
esac

if [ "${OXBASE_KEYCHAIN_USE_SESSION:-0}" = "1" ]; then
  exec "$@"
fi

for tool in dbus-run-session gnome-keyring-daemon; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "keychain-tests: '$tool' not found (apt-get install dbus gnome-keyring)" >&2
    exit 1
  fi
done

# Build first, outside the private session, so a compile error is reported plainly and the
# keyring daemon does not sit idle during a long build.
if [ "$1" = "cargo" ] && [ "${2:-}" = "test" ]; then
  build=("$@")
  for i in "${!build[@]}"; do
    if [ "${build[$i]}" = "--" ]; then
      build=("${build[@]:0:$i}")
      break
    fi
  done
  "${build[@]}" --no-run
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/oxbase-keychain.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
mkdir -m 700 "$tmp/data" "$tmp/run"

# Inside the private session: start gnome-keyring with a throwaway login keyring (created and
# unlocked with a fixed password read from stdin), then run the command. The daemon exits when
# the private bus goes away.
# shellcheck disable=SC2016
XDG_DATA_HOME="$tmp/data" XDG_RUNTIME_DIR="$tmp/run" dbus-run-session -- bash -c '
  set -euo pipefail
  printf "%s" "oxbase-keychain-tests" \
    | gnome-keyring-daemon --unlock --components=secrets >/dev/null
  exec "$@"
' keychain-tests "$@"
