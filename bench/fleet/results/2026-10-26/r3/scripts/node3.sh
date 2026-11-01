#!/usr/bin/env bash
# Round 3 on one node: drive this node's own sandboxes through its gateway.
set -u
cd /tmp/r3
N=$1
W="python3 fleet/workloads.py --master https://10.2.0.11:80 --ca ca.pem --token-file tok --node $N"
mkdir -p out
log() { echo "$(date +%T) $*" >> out/node.log; }
log "sizes";  $W sizes --templates app16,app64,app256 --count 5 --out out/sizes.jsonl > out/sizes.log 2>&1
log "sleep";  $W sleep --template app64 --count 14 --out out/sleep.jsonl > out/sleep.log 2>&1
log "herd";   $W herd --template app64 --sizes 1,4,10,20,40 --out out/herd.jsonl > out/herd.log 2>&1
log "mixed";  $W mixed --template app64 --count 100 --rates 1,2,4,8 --phase-secs 180 --sample-secs 3600 --out out/mixed.jsonl > out/mixed.log 2>&1
log "DONE"
