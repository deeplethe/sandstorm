#!/usr/bin/env bash
# The same app, as plain processes on the host: no sandbox, no tiering.
# Start them until MemAvailable drops under the 512 MiB floor sandstorm uses,
# so the count is comparable with sandstorm's "every sandbox in memory".
set -u
cd /tmp/r3
out=out/bare.jsonl
# The idle template first: a sleeping interpreter, nothing to request.
n=0; pids=()
while :; do
  free=$(awk '/MemAvailable/{print int($2/1024)}' /proc/meminfo)
  [ "$free" -lt 512 ] && break
  python3 -c 'import time
while True: time.sleep(3600)' &
  pids+=($!)
  n=$((n + 1)); sleep 0.05
  [ $n -ge 3000 ] && break
done
rss=$(ps -o rss= -p "$(IFS=,; echo "${pids[*]}")" | awk '{s+=$1} END {print int(s/NR/1024)}')
echo "{\"heap_mib\": null, \"template\": \"idle\", \"processes\": $n, \"rss_mib_each\": $rss, \"free_mib_after\": $free}" | tee -a $out
kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null; sleep 5

for MIB in 16 64 256 512 1024; do
  n=0; pids=()
  while :; do
    free=$(awk '/MemAvailable/{print int($2/1024)}' /proc/meminfo)
    [ "$free" -lt 512 ] && break
    port=$((30000 + n))
    python3 /tmp/r3/srv_port.py $MIB $port >/dev/null 2>&1 &
    pids+=($!)
    for i in $(seq 1 100); do curl -s -o /dev/null -m 1 http://127.0.0.1:$port/ && break; sleep 0.1; done
    n=$((n + 1))
    [ $n -ge 400 ] && break
  done
  rss=$(ps -o rss= -p "$(IFS=,; echo "${pids[*]}")" | awk '{s+=$1} END {print int(s/NR/1024)}')
  lat=$(for p in $(seq 30000 $((30000 + n - 1))); do curl -s -o /dev/null -w "%{time_total}\n" http://127.0.0.1:$p/; done | sort -n | awk '{a[NR]=$1} END {print a[int(NR/2)+1]}')
  echo "{\"heap_mib\": $MIB, \"processes\": $n, \"rss_mib_each\": $rss, \"latency_p50_s\": $lat, \"free_mib_after\": $free}" | tee -a $out
  kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null
  sleep 5
done
