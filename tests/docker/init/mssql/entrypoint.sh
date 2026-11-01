#!/bin/bash
# oxbase fixtures: SQL Server wrapper entrypoint.
# The image has no init-script hook, so this starts sqlservr through the image's own launcher,
# applies /oxbase-init/*.sql once, then writes a marker file the compose healthcheck requires. Stop
# signals are forwarded to sqlservr itself.
# "Seeded" means oxbase_test carries the database-level extended property oxbase_fixture_seeded, which is
# added only after every script succeeded. A oxbase_test without it (an earlier seed failed partway)
# is dropped and rebuilt, so the container never reports healthy on a partial schema.
set -euo pipefail

SQLCMD="${OXBASE_SQLCMD:-/opt/mssql-tools18/bin/sqlcmd}"
MARKER=/tmp/oxbase-fixture-seeded
rm -f "$MARKER"

# M4 TLS (docs/m4-contracts.md §5.1, TEST-ONLY PKI): serve the fixture certificate. TLS is offered
# but not forced (forceencryption = 0), so plaintext logins keep working; the TLS-only login
# oxbase_tls is enforced by a logon trigger (03-tls-user.sql). Needs root (compose `user: root`)
# to read the bind-mounted 0600 key.
tls_src=/oxbase-tls-src
tls_dir=/etc/oxbase-tls
for f in ca.pem server.pem server-key.pem; do
    if [ ! -s "$tls_src/$f" ]; then
        echo "oxbase-init: missing $tls_src/$f - run scripts/fixtures-certs.sh (fixtures-up.sh does)" >&2
        exit 1
    fi
done
mkdir -p "$tls_dir"
cp "$tls_src/ca.pem" "$tls_src/server.pem" "$tls_src/server-key.pem" "$tls_dir/"
chmod 0755 "$tls_dir"
chmod 0644 "$tls_dir/ca.pem" "$tls_dir/server.pem"
chmod 0600 "$tls_dir/server-key.pem"
cat >/var/opt/mssql/mssql.conf <<EOF_CONF
[network]
tlscert = $tls_dir/server.pem
tlskey = $tls_dir/server-key.pem
forceencryption = 0
EOF_CONF

/opt/mssql/bin/launch_sqlservr.sh /opt/mssql/bin/sqlservr &
pid=$!
# launch_sqlservr.sh does not forward signals, so send SIGTERM straight to the (oldest) sqlservr
# process for a clean shutdown; the launcher exits once sqlservr has.
trap 'pkill -TERM -o -x sqlservr 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true' TERM INT

sql() {
    "$SQLCMD" -C -S 127.0.0.1 -U sa -P "$MSSQL_SA_PASSWORD" -b "$@"
}

echo "oxbase-init: waiting for SQL Server to accept connections"
ready=0
for _ in $(seq 1 120); do
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "oxbase-init: sqlservr exited during startup" >&2
        wait "$pid" || exit $?
        exit 1
    fi
    if sql -h -1 -Q "SET NOCOUNT ON; SELECT 1" >/dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 1
done
if [ "$ready" -ne 1 ]; then
    echo "oxbase-init: SQL Server did not become ready in time" >&2
    kill -TERM "$pid" 2>/dev/null || true
    wait "$pid" || true
    exit 1
fi

# 0 = no oxbase_test, 1 = oxbase_test without the completion marker (partial seed), 2 = fully seeded.
state="$(sql -h -1 -W -Q "SET NOCOUNT ON;
IF DB_ID('oxbase_test') IS NULL SELECT 0
ELSE IF EXISTS (SELECT 1 FROM oxbase_test.sys.extended_properties
                WHERE class = 0 AND name = N'oxbase_fixture_seeded') SELECT 2
ELSE SELECT 1" | tr -d '[:space:]')"
case "$state" in
    2)
        echo "oxbase-init: oxbase_test already seeded, skipping seed"
        ;;
    0 | 1)
        if [ "$state" = "1" ]; then
            echo "oxbase-init: oxbase_test is incomplete (no seed marker), dropping it and reseeding"
            sql -Q "ALTER DATABASE oxbase_test SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE oxbase_test;"
        fi
        for f in /oxbase-init/*.sql; do
            echo "oxbase-init: applying $f"
            sql -i "$f"
        done
        sql -d oxbase_test -Q "EXEC sys.sp_addextendedproperty @name = N'oxbase_fixture_seeded', @value = N'1';"
        echo "oxbase-init: seed complete"
        ;;
    *)
        echo "oxbase-init: could not determine seed state (got '$state')" >&2
        exit 1
        ;;
esac
touch "$MARKER"

# `wait` returns early when a trapped signal arrives; keep waiting until the server has exited,
# then return its own exit status.
while kill -0 "$pid" 2>/dev/null; do
    wait "$pid" || true
done
status=0
wait "$pid" || status=$?
exit "$status"
