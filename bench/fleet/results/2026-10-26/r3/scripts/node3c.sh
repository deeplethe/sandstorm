#!/usr/bin/env bash
set -u
cd /tmp/r3
N=$1
W="python3 fleet/workloads.py --master https://10.2.0.11:80 --ca ca.pem --token-file tok --node $N"
log() { echo "$(date +%T) $*" >> out/node.log; }
rm -f out/mixed.jsonl
log "mixed";  $W mixed --template app64 --count 40 --create-workers 1 --rates 1,2,4,8 --phase-secs 150 --sample-secs 3600 --out out/mixed.jsonl > out/mixed.log 2>&1
log "DONE"
