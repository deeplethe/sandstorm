#!/usr/bin/env bash
# #327: a connect to a sandbox that is not hot should be refused at once,
# not left to time out.
#
# Before: `curl -m 5` against a snapshotted sandbox took 5011 ms, and the
# duration was purely the caller's deadline. The pass condition is that a
# generous timeout is NOT consumed -- so every probe here uses -m 5, and a
# result anywhere near 5000 ms is a failure.
set -u
ROOT=/var/lib/sandstorm-rej; PORT=18081
BIN=/home/ubuntu/sandstorm/target/release/sandstormd; TPL=$ROOT/work/templates/py/rootfs
P=""; pass=0; fail=0
ok(){ echo "  PASS  $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL  $1"; fail=$((fail+1)); }
now(){ date +%s%N; }
cleanup(){
  [ -n "$P" ] && kill "$P" 2>/dev/null; wait "$P" 2>/dev/null
  for d in /sys/fs/cgroup/sandstorm/*/; do [ -d "$d" ] && { echo 0 > "$d/cgroup.freeze" 2>/dev/null; echo 1 > "$d/cgroup.kill" 2>/dev/null; }; done
  sleep 1
  for n in $(ip netns list 2>/dev/null | awk '/^sandstorm-/{print $1}'); do ip netns del "$n" 2>/dev/null; done
  for l in $(ip -o link show | awk -F': ' '/sst[0-9]+h/{print $2}' | cut -d@ -f1); do ip link del "$l" 2>/dev/null; done
  for m in $(mount | awk -v r="$ROOT" 'index($3,r)==1{print $3}'); do umount -l "$m" 2>/dev/null; done
  rmdir /sys/fs/cgroup/sandstorm/* /sys/fs/cgroup/sandstorm 2>/dev/null
  iptables -D OUTPUT -j SANDSTORM-COLD 2>/dev/null; iptables -F SANDSTORM-COLD 2>/dev/null; iptables -X SANDSTORM-COLD 2>/dev/null
  rm -rf "$ROOT"
}
trap cleanup EXIT
cleanup 2>/dev/null
ss -tln | grep -q ":$PORT " && { echo REFUSING; exit 2; }
mkdir -p "$TPL"/{proc,dev,sys,tmp,etc,var,root,app,usr,lib,lib64,bin,sbin}
printf 'root:x:0:0:root:/root:/bin/sh\n' > "$TPL/etc/passwd"
cat > "$TPL/app/s.py" <<'PY'
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_GET(s):
        s.send_response(200); s.send_header("Content-Length","2"); s.end_headers(); s.wfile.write(b"ok")
    def log_message(s,*a): pass
HTTPServer(("0.0.0.0", 8080), H).serve_forever()
PY
"$BIN" serve --master --node --bind "127.0.0.1:$PORT" --state-db "$ROOT/state.db" \
  --pki-dir "$ROOT/pki" --work-dir "$ROOT/work" --volume-root "$ROOT/volumes" \
  --runsc-root "$ROOT/runsc" --node-id rej-1 \
  --idle-frozen-secs 86400 --idle-snapshot-secs 86400 --idle-detached-secs 86400 >/tmp/sandstorm-rej.log 2>&1 &
P=$!
for _ in $(seq 1 300); do [ "$(curl -s -m 1 http://127.0.0.1:$PORT/healthz)" = ok ] && break; sleep 0.1; done
for _ in $(seq 1 300); do grep -q "accepting scheduling" /tmp/sandstorm-rej.log && break; sleep 0.1; done
iptables -n -L SANDSTORM-COLD >/dev/null 2>&1 && ok "SANDSTORM-COLD chain created at node start" || bad "no chain"
iptables -C OUTPUT -j SANDSTORM-COLD 2>/dev/null && ok "OUTPUT jumps to it" || bad "no jump from OUTPUT"
T=$(cat "$ROOT/pki/api-token"); A(){ curl -s -m 120 -H "Authorization: Bearer $T" "$@"; }
API="http://127.0.0.1:$PORT"
A -X POST "$API/v1/templates" -H 'content-type: application/json' \
  -d '{"name":"py","rootfs":"rootfs","spec":{"args":["/usr/bin/python3","/app/s.py"]}}' -o /dev/null
VOL=$ROOT/volumes/sb-rej/workspace; mkdir -p "$VOL"
ID=$(A -X POST "$API/v1/sandboxes" -H 'content-type: application/json' -d "{\"template\":\"py\",\"volume\":\"$VOL\"}" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
for _ in $(seq 1 900); do IP=$(A "$API/v1/sandboxes/$ID" | sed -n 's/.*"ip":"\([^"]*\)".*/\1/p'); [ -n "$IP" ] && break; sleep 0.1; done
for _ in $(seq 1 900); do [ -n "$(curl -s -m 2 "http://$IP:8080/" 2>/dev/null)" ] && break; sleep 0.05; done
echo "sandbox $ID at $IP"

# every probe uses a GENEROUS timeout; consuming it is the failure
probe(){ local t0=$(now); curl -s -m 5 "http://$IP:8080/" >/dev/null 2>&1; echo $(( ($(now) - t0) / 1000000 )); }
rules(){ iptables -S SANDSTORM-COLD 2>/dev/null | grep -F -- "$IP" | wc -l | tr -d ' '; }

MS=$(probe); [ "$MS" -lt 500 ] && ok "hot: answers in ${MS} ms, no rule ($(rules) for this ip)" || bad "hot: ${MS} ms"

for tier in frozen snapshot detached; do
  A -X POST "$API/v1/sandboxes/$ID/tier" -H 'content-type: application/json' -d "{\"tier\":\"$tier\"}" -o /dev/null
  for _ in $(seq 1 600); do [ "$(A "$API/v1/sandboxes/$ID" | sed -n 's/.*"tier":"\([^"]*\)".*/\1/p')" = "$tier" ] && break; sleep 0.05; done
  # let the node finish the transition before probing
  for _ in $(seq 1 400); do [ "$(rules)" -gt 0 ] && break; sleep 0.02; done
  MS=$(probe)
  if [ "$MS" -lt 500 ]; then
    ok "$tier: refused in ${MS} ms (was 5011 ms before), $(rules) rule(s)"
  else
    bad "$tier: took ${MS} ms of a 5000 ms budget, $(rules) rule(s)"
  fi
  A -X POST "$API/v1/sandboxes/$ID/touch" -o /dev/null
  UP=no
  for _ in $(seq 1 1200); do [ -n "$(curl -s -m 1 "http://$IP:8080/" 2>/dev/null)" ] && { UP=yes; break; }; sleep 0.01; done
  [ "$UP" = yes ] && ok "$tier -> hot: answering again, $(rules) rule(s) left" || bad "$tier -> hot: never came back"
  [ "$(rules)" = "0" ] && ok "$tier -> hot: the rule was removed" || bad "$tier -> hot: $(rules) rule(s) linger"
done

echo
echo "=== the rule must not outlive the sandbox ==="
A -X POST "$API/v1/sandboxes/$ID/tier" -H 'content-type: application/json' -d '{"tier":"snapshot"}' -o /dev/null
for _ in $(seq 1 400); do [ "$(rules)" -gt 0 ] && break; sleep 0.02; done
[ "$(rules)" -gt 0 ] && ok "cold again, rule present" || bad "no rule while cold"
A -X DELETE "$API/v1/sandboxes/$ID" -o /dev/null
for _ in $(seq 1 600); do [ "$(rules)" = "0" ] && break; sleep 0.05; done
[ "$(rules)" = "0" ] && ok "deleting the sandbox removed its rule (the next holder of that slot is not refused)" \
                     || bad "$(rules) rule(s) outlived the sandbox"

echo
echo "=== node log warnings ==="
sed 's/\x1b\[[0-9;]*m//g' /tmp/sandstorm-rej.log | grep -E 'WARN|ERROR' | tail -4 | sed 's/^/  /' || echo "  (none)"
echo
echo "=== verdict: $pass passed, $fail failed ==="
[ "$fail" -eq 0 ]
