#!/bin/sh
# oxbase fixtures: Redis Sentinel (tests/fixtures.toml [m7.redis_sentinel], docs/m7-contracts.md
# §9.3). TEST-ONLY.
#
# One container (network_mode: host) runs a master, one replica (`--replicaof`) and one sentinel
# (quorum 1, `sentinel auth-pass`, no password of its own) as separate processes on
# 127.0.0.1, so the sentinel reports a master address the tests can dial. Nothing is persisted:
# the seed (m7:sentinel = "master" on the master) is written on every start. No failover is
# exercised (down-after-milliseconds is long so a slow runner does not trigger one).
#
#   sentinel.sh          start the three processes, seed, wait for them
#   sentinel.sh health   exit 0 when the replica is linked, the sentinel names the master and
#                        the seed key is on the master
#
# Env: OXBASE_REDIS_PASSWORD, OXBASE_SENTINEL_MASTER_PORT (15511), OXBASE_SENTINEL_REPLICA_PORT
#      (15512), OXBASE_SENTINEL_PORT (15526), OXBASE_SENTINEL_MASTER_NAME (oxmaster),
#      OXBASE_SENTINEL_SEED_KEY (m7:sentinel).
set -eu

pw="$OXBASE_REDIS_PASSWORD"
master_port="$OXBASE_SENTINEL_MASTER_PORT"
replica_port="$OXBASE_SENTINEL_REPLICA_PORT"
sentinel_port="$OXBASE_SENTINEL_PORT"
master_name="$OXBASE_SENTINEL_MASTER_NAME"
seed_key="$OXBASE_SENTINEL_SEED_KEY"

cli() {
    port="$1"
    shift
    redis-cli --no-auth-warning -h 127.0.0.1 -p "$port" -a "$pw" "$@"
}

# The sentinel has no password (profile option sentinel_auth = none).
sentinel_cli() {
    redis-cli -h 127.0.0.1 -p "$sentinel_port" "$@"
}

if [ "${1:-}" = "health" ]; then
    cli "$replica_port" INFO replication 2>/dev/null | grep -q '^master_link_status:up' || exit 1
    addr="$(sentinel_cli SENTINEL GET-MASTER-ADDR-BY-NAME "$master_name" 2>/dev/null | tr '\n' ' ')"
    [ "$addr" = "127.0.0.1 $master_port " ] || exit 1
    [ "$(cli "$master_port" GET "$seed_key" 2>/dev/null)" = "master" ] || exit 1
    exit 0
fi

root=/data/sentinel
mkdir -p "$root/master" "$root/replica" "$root/sentinel"

redis-server --port "$master_port" --dir "$root/master" --bind 127.0.0.1 \
    --requirepass "$pw" --masterauth "$pw" --appendonly no --save "" \
    --maxmemory 32mb --maxmemory-policy noeviction &
pids="$!"
redis-server --port "$replica_port" --dir "$root/replica" --bind 127.0.0.1 \
    --replicaof 127.0.0.1 "$master_port" --replica-announce-ip 127.0.0.1 \
    --requirepass "$pw" --masterauth "$pw" --appendonly no --save "" \
    --maxmemory 32mb --maxmemory-policy noeviction &
pids="$pids $!"

# The sentinel rewrites its configuration file, so it gets a fresh writable copy on every start.
cat >"$root/sentinel/sentinel.conf" <<EOF
port $sentinel_port
bind 127.0.0.1
dir $root/sentinel
sentinel announce-ip 127.0.0.1
sentinel monitor $master_name 127.0.0.1 $master_port 1
sentinel auth-pass $master_name $pw
sentinel down-after-milliseconds $master_name 60000
sentinel failover-timeout $master_name 180000
EOF
redis-server "$root/sentinel/sentinel.conf" --sentinel &
pids="$pids $!"
trap 'for pid in $pids; do kill -TERM "$pid" 2>/dev/null || true; done' TERM INT

wait_for() {
    what="$1"
    shift
    i=0
    until "$@" 2>/dev/null | grep -q PONG; do
        i=$((i + 1))
        if [ "$i" -gt 60 ]; then
            echo "oxbase-sentinel: $what did not become ready in time" >&2
            exit 1
        fi
        sleep 1
    done
}
wait_for master cli "$master_port" PING
wait_for replica cli "$replica_port" PING
wait_for sentinel sentinel_cli PING

if [ "$(cli "$master_port" SET "$seed_key" master)" != "OK" ]; then
    echo "oxbase-sentinel: could not write $seed_key" >&2
    exit 1
fi
echo "oxbase-sentinel: master 127.0.0.1:$master_port, replica :$replica_port, sentinel :$sentinel_port ($master_name)"

# Wait for the processes (see cluster.sh).
for pid in $pids; do
    while kill -0 "$pid" 2>/dev/null; do
        wait "$pid" || true
    done
done
