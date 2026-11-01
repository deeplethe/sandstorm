#!/usr/bin/env bash
# The master's share of a fleet round, after every node has finished
# suite-node.sh: how many fit in memory, then how many fit at all and
# whether they still answer.
#
#   suite-master.sh MASTER_URL NODE1,NODE2,... [REPEATS]
#
# Run on the master from a directory holding fleet/{loadtest,workloads}.py,
# ca.pem and tok. The probes have to run on the nodes, where the gateways
# are, so this only fills and says when; see probe-nodes in the results
# README for the step that follows each fill.
set -u
M=$1 NODES=$2 R=${3:-2}
ADDRS=$(printf '%s\n' ${NODES//,/ } | sed 's/$/=x/' | paste -sd, -)
L="python3 fleet/loadtest.py --master $M --ca ca.pem --token-file tok --pool default --allow-existing"
W="python3 fleet/workloads.py --master $M --ca ca.pem --token-file tok --node-addrs $ADDRS"
API="curl -sS --cacert ca.pem -H @auth.hdr"
mkdir -p out
log() { echo "$(date +%T) $*" | tee -a out/run.log; }
drain() {
  $API "$M/v1/sandboxes" | python3 -c 'import json,sys; [print(s["id"]) for s in json.load(sys.stdin)]' \
    | xargs -P 16 -I{} $API -X DELETE "$M/v1/sandboxes/{}" -o /dev/null
  for _ in $(seq 1 300); do
    c=$($API "$M/v1/sandboxes" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
    [ "$c" = 0 ] && return
    sleep 2
  done
  log "WARNING drain left $c"
}

# The idle timers, shortened so a run fits in minutes. Restarting the
# master is safe mid-round: desired state is in SQLite and the nodes keep
# running what they have.
timers() {
  log "timers frozen=$1 snapshot=$2"
  local bind=${M#https://}
  sudo mkdir -p /etc/systemd/system/sandstorm-master.service.d
  printf '[Service]\nExecStart=\nExecStart=/usr/local/bin/sandstormd serve --master --bind %s --state-db /var/lib/sandstorm/state.db --pki-dir /var/lib/sandstorm/pki --work-dir /var/lib/sandstorm --idle-frozen-secs %s --idle-snapshot-secs %s --idle-detached-secs 86400\n' "$bind" "$1" "$2" \
    | sudo tee /etc/systemd/system/sandstorm-master.service.d/bench.conf >/dev/null
  sudo systemctl daemon-reload && sudo systemctl restart sandstorm-master
  local want n
  want=$(printf '%s\n' ${NODES//,/ } | wc -l)
  for _ in $(seq 1 60); do
    n=$($API "$M/v1/nodes" 2>/dev/null | python3 -c 'import json,sys; print(sum(1 for n in json.load(sys.stdin) if n.get("connected")))' 2>/dev/null || echo 0)
    [ "$n" = "$want" ] && { log "$n nodes connected"; return; }
    sleep 2
  done
  log "WARNING only $n of $want nodes connected"
}

case "${4:-walls}" in
  timers) timers "$5" "$6";;
  walls)
    # A fifth argument picks the templates, e.g. app1024,app512.
    for i in $(seq 1 "$R"); do
      for T in $(echo "${5:-app256,app64,app16,web}" | tr , ' '); do
        log "wall $T $i"
        # SETTLE: how long to wait before counting what came up. A create
        # the node held back is retried as memory frees, and is counted as
        # failed until then; 10 s is short of that for the small heaps.
        $L --template "$T" --total 900 --concurrency 4 --touch --hold-secs 20 --settle-secs "${SETTLE:-10}" --out "out/wall_$T-$i.json" > "out/wall_$T-$i.log" 2>&1
        drain
      done
    done;;
  fill)
    # 20,000 web sandboxes at 20/s, placed by the master, tokens kept for
    # the probes. A watcher samples the tier counts every 5 s.
    i=${5:-1}
    $W watch --duration 2400 --sample-secs 5 --out "out/ceiling-$i-samples.jsonl" >/dev/null 2>&1 &
    log "fill $i"
    $W fill --template web --count 20000 --rate 20 --tokens "tokens-$i.jsonl" --out "out/ceiling-$i.jsonl" > "out/ceiling-$i.log" 2>&1
    log "fill $i done";;
  drain) drain; log "drained";;
esac
