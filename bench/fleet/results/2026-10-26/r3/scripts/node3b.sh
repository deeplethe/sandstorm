#!/usr/bin/env bash
set -u
cd /tmp/r3
N=$1
W="python3 fleet/workloads.py --master https://10.2.0.11:80 --ca ca.pem --token-file tok --node $N"
log() { echo "$(date +%T) $*" >> out/node.log; }
rm -f out/herd.jsonl out/mixed.jsonl
log "herd";   $W herd --template app64 --sizes 1,4,10,20,40 --create-workers 2 --out out/herd.jsonl > out/herd.log 2>&1
log "mixed";  $W mixed --template app64 --count 100 --create-workers 2 --rates 1,2,4,8 --phase-secs 180 --sample-secs 3600 --out out/mixed.jsonl > out/mixed.log 2>&1
log "DONE"
