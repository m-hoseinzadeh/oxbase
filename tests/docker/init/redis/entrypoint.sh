#!/bin/sh
# oxbase fixtures: Redis wrapper entrypoint.
# The image has no init-script hook, so this starts redis-server through the image's own
# entrypoint (arguments come from the compose `command`), loads /oxbase-init/seed.redis once (when
# the marker key is absent; data persists via AOF on the volume), and forwards stop signals.
# The marker key oxbase_test:fixture:seeded is set here, only after the whole seed ran without errors.
# M7: then /oxbase-init/seed-m7.redis once (marker oxbase_test:fixture:m7_seeded, so a volume
# seeded before M7 gets the M7 keys too), m7:json when RedisJSON is loaded, and on every start the
# ACL user m7_reader (ACL rules live in memory only, not in the AOF).
set -eu

# M7: the TLS port (--tls-port in compose) reads the test CA / server certificate / key from
# /etc/oxbase-tls. The bind-mounted host files keep the host user's owner and mode 0600, which
# the redis user the image drops to cannot read, so copies owned by redis are made first (as
# tests/docker/tls/entrypoint.sh does for the SQL engines).
if [ -d /oxbase-tls-src ]; then
    for f in ca.pem server.pem server-key.pem; do
        if [ ! -s "/oxbase-tls-src/$f" ]; then
            echo "oxbase-init: missing /oxbase-tls-src/$f - run scripts/fixtures-certs.sh" >&2
            exit 1
        fi
    done
    mkdir -p /etc/oxbase-tls
    cp /oxbase-tls-src/ca.pem /oxbase-tls-src/server.pem /oxbase-tls-src/server-key.pem /etc/oxbase-tls/
    chown -R redis:redis /etc/oxbase-tls
    chmod 0750 /etc/oxbase-tls
    chmod 0644 /etc/oxbase-tls/ca.pem /etc/oxbase-tls/server.pem
    chmod 0600 /etc/oxbase-tls/server-key.pem
fi

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

# Pipes a seed file (comments and blank lines dropped) into redis-cli. redis-cli exits 0 even
# when a piped command fails, so the "(error)" prefix is forced with --no-raw and checked.
load_seed() {
    out="$(grep -v '^#' "$1" | grep -v '^[[:space:]]*$' | cli --no-raw)"
    if printf '%s\n' "$out" | grep -q '^(error)'; then
        echo "oxbase-init: $1 failed:" >&2
        printf '%s\n' "$out" | grep '^(error)' >&2
        exit 1
    fi
}

# M7 (docs/m7-contracts.md §9.2): the read-only ACL user, on every start.
if [ "$(cli ACL SETUSER m7_reader reset on ">$OXBASE_REDIS_PASSWORD" '~m7:*' '+@read')" != "OK" ]; then
    echo "oxbase-init: could not create the ACL user m7_reader" >&2
    exit 1
fi

if [ "$(cli EXISTS oxbase_test:fixture:seeded)" = "0" ]; then
    # No marker: start from empty databases so a reseed after a partial run cannot duplicate list
    # entries or collide with existing stream IDs (the M7 seed also writes database 1).
    echo "oxbase-init: loading seed"
    cli FLUSHALL >/dev/null
    load_seed /oxbase-init/seed.redis
    # The marker key is written only after a clean run.
    if [ "$(cli -n 0 SET oxbase_test:fixture:seeded 1)" != "OK" ]; then
        echo "oxbase-init: could not write the seed marker" >&2
        exit 1
    fi
    echo "oxbase-init: seed complete"
else
    echo "oxbase-init: seed already present, skipping"
fi

if [ "$(cli -n 0 EXISTS oxbase_test:fixture:m7_seeded)" = "0" ]; then
    echo "oxbase-init: loading the M7 seed"
    load_seed /oxbase-init/seed-m7.redis
    # m7:json needs RedisJSON ([m7.redis] json_optional): set it when the command exists.
    if [ "$(cli -n 0 JSON.SET m7:json '$' '{"a": 1, "b": [true, null]}' 2>/dev/null)" = "OK" ]; then
        echo "oxbase-init: m7:json set (RedisJSON loaded)"
    else
        echo "oxbase-init: RedisJSON not loaded; m7:json skipped"
    fi
    if [ "$(cli -n 0 SET oxbase_test:fixture:m7_seeded 1)" != "OK" ]; then
        echo "oxbase-init: could not write the M7 seed marker" >&2
        exit 1
    fi
    echo "oxbase-init: M7 seed complete"
else
    echo "oxbase-init: M7 seed already present, skipping"
fi

# `wait` returns early when a trapped signal arrives; keep waiting until the server has exited,
# then return its own exit status.
while kill -0 "$pid" 2>/dev/null; do
    wait "$pid" || true
done
status=0
wait "$pid" || status=$?
exit "$status"
