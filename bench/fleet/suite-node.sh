#!/usr/bin/env bash
# One node's share of a fleet round: drive this node's own sandboxes
# through its own gateway, over loopback, so no network is in any latency.
#
#   suite-node.sh NODE_ID MASTER_URL [REPEATS]
#
# Run from a directory holding fleet/workloads.py, ca.pem and tok (the
# master's api-token). Writes out/*.jsonl, one file per run, and logs each
# step to out/node.log. Every node runs it at once; the parameters are the
# ones the 2026-10-26 round used, so the two can be compared directly.
set -u
N=$1 M=$2 R=${3:-2}
W="python3 fleet/workloads.py --master $M --ca ca.pem --token-file tok --node $N"
mkdir -p out
log() { echo "$(date +%T) $*" >> out/node.log; }
run() { local name=$1; shift; log "$name"; $W "$@" --out "out/$name.jsonl" > "out/$name.log" 2>&1; }

for i in $(seq 1 $((R + 1))); do
  run "mixed40-$i" mixed --template app64 --count 40 --rates 1,2,4,8 --phase-secs 150 --zipf 1.1 \
    --max-inflight 256 --create-workers 1 --sample-secs 3600
done
for i in $(seq 1 "$R"); do
  run "mixed80-$i" mixed --template app64 --count 80 --rates 1,2,4,8 --phase-secs 150 --zipf 1.1 \
    --max-inflight 256 --create-workers 1 --sample-secs 3600
  run "sleep-$i" sleep --template app64 --count 14 --sample-secs 3600
  run "sizes-$i" sizes --templates app16,app64,app256 --count 5 --sample-secs 3600
  run "herd-$i" herd --template app64 --sizes 1,4,10,20,40 --sample-secs 3600
done
run heavy sizes --templates app512,app1024 --count 1 --sample-secs 3600
log DONE
