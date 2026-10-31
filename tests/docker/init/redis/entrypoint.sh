#!/bin/sh
# oxbase fixtures: Redis wrapper entrypoint.
# The image has no init-script hook, so this starts redis-server through the image's own
# entrypoint (arguments come from the compose `command`), loads /oxbase-init/seed.redis once (when
# the marker key is absent; data persists via AOF on the volume), and forwards stop signals.
# The marker key oxbase_test:fixture:seeded is set here, only after the whole seed ran without errors.
set -eu

docker-entrypoint.sh "$@" &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null || true' TERM INT

cli() {
    redis-cli --no-auth-warning -a "$OXBASE_REDIS_PASSWORD" "$@"
}

i=0
until cli ping 2>/dev/null | grep -q PONG; do
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "oxbase-init: redis-server exited during startup" >&2
        exit 1
    fi
    i=$((i + 1))
    if [ "$i" -gt 60 ]; then
        echo "oxbase-init: redis-server did not become ready in time" >&2
        exit 1
    fi
    sleep 1
done

if [ "$(cli EXISTS oxbase_test:fixture:seeded)" = "0" ]; then
    # No marker: start from an empty DB 0 so a reseed after a partial run cannot duplicate list
    # entries or collide with existing stream IDs.
    echo "oxbase-init: loading seed"
    cli -n 0 FLUSHDB >/dev/null
    # redis-cli exits 0 even when a piped command fails, so force the "(error)" prefix with
    # --no-raw and check for it; the marker key is written only after a clean run.
    out="$(grep -v '^#' /oxbase-init/seed.redis | grep -v '^[[:space:]]*$' | cli --no-raw)"
    if printf '%s\n' "$out" | grep -q '^(error)'; then
        echo "oxbase-init: seed failed:" >&2
        printf '%s\n' "$out" | grep '^(error)' >&2
        exit 1
    fi
    if [ "$(cli -n 0 SET oxbase_test:fixture:seeded 1)" != "OK" ]; then
        echo "oxbase-init: could not write the seed marker" >&2
        exit 1
    fi
    echo "oxbase-init: seed complete"
else
    echo "oxbase-init: seed already present, skipping"
fi

# `wait` returns early when a trapped signal arrives; keep waiting until the server has exited,
# then return its own exit status.
while kill -0 "$pid" 2>/dev/null; do
    wait "$pid" || true
done
status=0
wait "$pid" || status=$?
exit "$status"
