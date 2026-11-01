#!/usr/bin/env bash
# One machine's half of the cross-machine restore test. Plain runsc, no sandstorm.
set -u
X=/tmp/xr
R="sudo runsc --root $X/state --network=none --debug-log=$X/log/"
case "$1" in
  setup)
    sudo rm -rf $X; mkdir -p $X/bundle $X/img $X/log; sudo mkdir -p $X/state /opt/xr
    sudo cp /tmp/app.py /opt/xr/app.py
    cd $X/bundle && runsc spec -- /usr/bin/python3 /opt/xr/app.py
    python3 - <<'PY'
import json
p = "/tmp/xr/bundle/config.json"
c = json.load(open(p))
c["root"] = {"path": "/", "readonly": True}
c["process"]["terminal"] = False
c["mounts"].append({"destination": "/tmp", "type": "tmpfs", "source": "tmpfs"})
json.dump(c, open(p, "w"), indent=1)
PY
    echo "setup ok $(sha256sum $X/bundle/config.json | cut -c1-12)";;
  run)
    $R run --detach --bundle $X/bundle "$2" >/dev/null 2>&1 || { echo "run failed"; exit 1; }
    for i in $(seq 1 60); do s=$($R exec "$2" cat /tmp/state 2>/dev/null); case "$s" in *now=[0-9a-f]*) echo "running: $s"; exit 0;; esac; sleep 1; done
    echo "never ready: $s";;
  checkpoint)
    before=$($R exec "$2" cat /tmp/state 2>/dev/null)
    sudo rm -rf $X/img; mkdir -p $X/img
    t0=$(date +%s.%N)
    $R checkpoint --image-path=$X/img "$2" 2>&1 | tail -1
    t1=$(date +%s.%N)
    echo "checkpointed in $(echo "$t1 - $t0" | bc)s, image $(sudo du -sh $X/img | cut -f1); before: $before"
    $R delete -force "$2" >/dev/null 2>&1; true;;
  recv)
    sudo rm -rf $X/img; mkdir -p $X/img
    setsid nohup sudo sh -c "cd $X/img && timeout 120 nc -l 80 | tar x; echo done > $X/recv.done" > $X/recv.log 2>&1 < /dev/null &
    rm -f $X/recv.done; sleep 1; echo listening;;
  received)
    for i in $(seq 1 60); do [ -f $X/recv.done ] && break; sleep 0.5; done
    echo "received $(sudo du -sh $X/img | cut -f1) $(cat $X/recv.log)";;
  send)
    t0=$(date +%s.%N)
    sudo tar c -C $X/img . | nc -N "$2" 80
    t1=$(date +%s.%N)
    echo "sent in $(echo "$t1 - $t0" | bc)s";;
  restore)
    t0=$(date +%s.%N)
    sudo rm -f $X/restore.out
    sudo sh -c "runsc --root $X/state --network=none --debug-log=$X/log/ restore --detach --image-path=$X/img --bundle $X/bundle $2 > $X/restore.out 2>&1 < /dev/null"
    rc=$?; t1=$(date +%s.%N)
    echo "restore rc=$rc in $(echo "$t1 - $t0" | bc)s $(sudo tail -c 300 $X/restore.out)";;
  check)
    sleep "${3:-8}"
    echo "state: $($R state "$2" 2>&1 | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["status"])
except Exception as e: print("unreadable")')"
    # Wait for a beat that re-hashed the heap, so the check covers its contents.
    for i in $(seq 1 15); do a=$($R exec "$2" cat /tmp/state 2>&1 | head -2); case "$a" in *now=[0-9a-f]*) break;; esac; sleep 1; done
    echo "after: $a"
    case "$a" in *"ref="*) r=${a#*ref=}; r=${r%% *}; n=${a#*now=}; n=${n%% *}; [ "$r" = "$n" ] && echo "HEAP OK" || echo "HEAP MISMATCH or no hash beat";; esac
    grep -l -i -E "SIGSEGV|panic|fatal" $X/log/* 2>/dev/null | head -3 | sed 's/^/  log with a crash: /';;
  clean)
    $R delete -force "$2" >/dev/null 2>&1; true;;
  teardown)
    for c in $(sudo runsc --root $X/state list --quiet 2>/dev/null); do sudo runsc --root $X/state delete -force $c; done
    sudo rm -rf $X /opt/xr /tmp/app.py /tmp/xr.sh; echo "$(hostname) clean";;
esac
