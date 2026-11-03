#!/bin/sh
# oxbase fixtures: Redis Cluster (tests/fixtures.toml [m7.redis_cluster], docs/m7-contracts.md
# §9.3). TEST-ONLY.
#
# One container (network_mode: host) runs the three master nodes as separate redis-server
# processes on 127.0.0.1:<port> (cluster bus <port> + 10000), each announcing 127.0.0.1, so
# CLUSTER SLOTS answers addresses the tests can dial. On the first start the slots are assigned
# with `redis-cli --cluster create ... --cluster-replicas 0 --cluster-yes`; a restarted
# container keeps its nodes.conf (in /data) and the cluster re-forms. Keys are not persisted:
# the seed (OXBASE_CLUSTER_SEED_KEYS string keys <prefix>0 .. <prefix>N-1, nothing else under
# the prefix) is written on every start, followed by the marker oxbase_test:fixture:cluster_seeded.
#
#   cluster.sh          start the nodes, create and seed the cluster, wait for the nodes
#   cluster.sh health   exit 0 when every node reports cluster_state:ok and the seed marker exists
#
# Env: OXBASE_REDIS_PASSWORD, OXBASE_CLUSTER_PORTS ("15501 15502 15503"),
#      OXBASE_CLUSTER_SEED_KEYS (100), OXBASE_CLUSTER_KEY_PREFIX ("m7:cluster:").
set -eu

pw="$OXBASE_REDIS_PASSWORD"
ports="$OXBASE_CLUSTER_PORTS"
seed_keys="${OXBASE_CLUSTER_SEED_KEYS:-100}"
prefix="${OXBASE_CLUSTER_KEY_PREFIX:-m7:cluster:}"
marker="oxbase_test:fixture:cluster_seeded"
first="${ports%% *}"

cli() {
    port="$1"
    shift
    redis-cli --no-auth-warning -h 127.0.0.1 -p "$port" -a "$pw" "$@"
}

cluster_ok() {
    for p in $ports; do
        cli "$p" CLUSTER INFO 2>/dev/null | grep -q '^cluster_state:ok' || return 1
    done
}

if [ "${1:-}" = "health" ]; then
    cluster_ok || exit 1
    [ "$(cli "$first" -c EXISTS "$marker" 2>/dev/null)" = "1" ] || exit 1
    exit 0
fi

pids=""
for p in $ports; do
    mkdir -p "/data/cluster/$p"
    redis-server \
        --port "$p" \
        --bind 127.0.0.1 \
        --dir "/data/cluster/$p" \
        --cluster-enabled yes \
        --cluster-config-file "/data/cluster/$p/nodes.conf" \
        --cluster-announce-ip 127.0.0.1 \
        --cluster-node-timeout 5000 \
        --requirepass "$pw" \
        --masterauth "$pw" \
        --appendonly no \
        --save "" \
        --maxmemory 64mb \
        --maxmemory-policy noeviction &
    pids="$pids $!"
done
trap 'for pid in $pids; do kill -TERM "$pid" 2>/dev/null || true; done' TERM INT

alive() {
    for pid in $pids; do
        kill -0 "$pid" 2>/dev/null || return 1
    done
}

for p in $ports; do
    i=0
    until cli "$p" PING 2>/dev/null | grep -q PONG; do
        if ! alive; then
            echo "oxbase-cluster: a node exited during startup" >&2
            exit 1
        fi
        i=$((i + 1))
        if [ "$i" -gt 60 ]; then
            echo "oxbase-cluster: node $p did not become ready in time" >&2
            exit 1
        fi
        sleep 1
    done
done

# A node that knows only itself has never joined a cluster: create it (first start only).
known="$(cli "$first" CLUSTER INFO | tr -d '\r' | sed -n 's/^cluster_known_nodes://p')"
if [ "$known" = "1" ]; then
    nodes=""
    for p in $ports; do
        nodes="$nodes 127.0.0.1:$p"
    done
    echo "oxbase-cluster: creating the cluster:$nodes"
    # shellcheck disable=SC2086 # one argument per node
    redis-cli --no-auth-warning -a "$pw" --cluster create $nodes --cluster-replicas 0 --cluster-yes
fi

i=0
until cluster_ok; do
    i=$((i + 1))
    if [ "$i" -gt 60 ]; then
        echo "oxbase-cluster: cluster_state did not become ok in time" >&2
        cli "$first" CLUSTER INFO >&2 || true
        exit 1
    fi
    sleep 1
done

# The seed: string keys spread over every master's slots (-c follows MOVED redirects).
n=0
while [ "$n" -lt "$seed_keys" ]; do
    if [ "$(cli "$first" -c SET "$prefix$n" "v$n")" != "OK" ]; then
        echo "oxbase-cluster: could not write $prefix$n" >&2
        exit 1
    fi
    n=$((n + 1))
done
if [ "$(cli "$first" -c SET "$marker" 1)" != "OK" ]; then
    echo "oxbase-cluster: could not write the seed marker" >&2
    exit 1
fi
echo "oxbase-cluster: cluster ready, $seed_keys keys under $prefix"

# Wait for the nodes. A stop signal (the trap) ends them all; `wait` returns early when a trapped
# signal arrives, so each node is waited for until it has really exited. A node that dies on its
# own makes the health check fail (its cluster_state is gone).
for pid in $pids; do
    while kill -0 "$pid" 2>/dev/null; do
        wait "$pid" || true
    done
done
