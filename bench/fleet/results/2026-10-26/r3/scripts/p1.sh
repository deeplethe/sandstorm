#!/usr/bin/env bash
# Round 3 on the master: idle timers, a sampler for the whole round, hot walls.
set -u
cd /tmp/r3
M=https://10.2.0.11:80
API="curl -sS --cacert ca.pem -H @auth.hdr"
L="python3 fleet/loadtest.py --master $M --ca ca.pem --token-file tok --pool default --allow-existing"
mkdir -p out
log() { echo "$(date +%T) $*" | tee -a out/run.log; }
timers() {
  log "timers frozen=$1 snapshot=$2"
  sudo mkdir -p /etc/systemd/system/sandstorm-master.service.d
  printf '[Service]\nExecStart=\nExecStart=/usr/local/bin/sandstormd serve --master --bind 10.2.0.11:80 --state-db /var/lib/sandstorm/state.db --pki-dir /var/lib/sandstorm/pki --work-dir /var/lib/sandstorm --idle-frozen-secs %s --idle-snapshot-secs %s --idle-detached-secs 86400\n' "$1" "$2" \
    | sudo tee /etc/systemd/system/sandstorm-master.service.d/round3.conf >/dev/null
  sudo systemctl daemon-reload && sudo systemctl restart sandstorm-master
  for i in $(seq 1 60); do
    n=$($API $M/v1/nodes 2>/dev/null | python3 -c 'import json,sys; print(sum(1 for n in json.load(sys.stdin) if n.get("connected")))' 2>/dev/null || echo 0)
    [ "$n" = 6 ] && { log "6 nodes connected"; return; }
    sleep 2
  done
  log "WARNING only $n nodes connected"
}
drain() {
  $API $M/v1/sandboxes | python3 -c 'import json,sys; [print(s["id"]) for s in json.load(sys.stdin)]' \
    | xargs -P 16 -I{} $API -X DELETE $M/v1/sandboxes/{} -o /dev/null
  for i in $(seq 1 120); do
    c=$($API $M/v1/sandboxes | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
    [ "$c" = 0 ] && return
    sleep 2
  done
  log "WARNING drain left $c"
}
case "${1:-}" in
  walls)
    log "start $(sha256sum /usr/local/bin/sandstormd | cut -c1-12)"
    timers 15 60
    nohup python3 fleet/workloads.py watch --master $M --ca ca.pem --token-file tok --node-addrs f3=x,f5=x,f7=x,f8=x,f9=x,f10=x \
      --duration 10800 --sample-secs 5 --out out/samples.jsonl >/dev/null 2>&1 &
    for T in app256 app64 app16 idle; do
      log "wall $T"
      $L --template $T --total 700 --concurrency 4 --touch --hold-secs 20 --out out/wall_$T.json > out/wall_$T.log 2>&1
      drain
    done
    log "walls done";;
  ceiling)
    timers 5 15
    log "ceiling"
    $L --template idle --total 14000 --concurrency 8 --rate 15 --through-refusals --keep --settle-secs 30 --out out/ceiling.json > out/ceiling.log 2>&1
    sleep 90
    log "ceiling ramp done"
    drain
    log "ceiling done";;
  ceiling2)
    timers 5 15
    log "ceiling2"
    $L --template idle --total 20000 --concurrency 8 --rate 20 --through-refusals --keep --settle-secs 30 --out out/ceiling2.json > out/ceiling2.log 2>&1
    sleep 90
    log "ceiling2 ramp done"
    drain
    log "ceiling2 done";;
esac
