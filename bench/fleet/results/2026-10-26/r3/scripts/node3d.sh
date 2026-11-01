#!/usr/bin/env bash
# The heavy end: 512 MiB and 1 GiB heaps, then the same apps as bare processes.
set -u
cd /tmp/r3
N=$1
W="python3 fleet/workloads.py --master https://10.2.0.11:80 --ca ca.pem --token-file tok --node $N"
log() { echo "$(date +%T) $*" >> out/node.log; }
log "heavy"
$W sizes --templates app512 --count 2 --out out/heavy.jsonl > out/heavy.log 2>&1
$W sizes --templates app1024 --count 1 --out out/heavy.jsonl >> out/heavy.log 2>&1
log "bare"
cp /tmp/r3/srv_port.py /tmp/r3/srv_port.py 2>/dev/null
bash bare.sh > out/bare.log 2>&1
log "DONE2"
