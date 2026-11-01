#!/usr/bin/env bash
# The gateway: does the access token actually guard a reachable port.
set -uo pipefail
B=/home/ubuntu/sandstorm/target/release/sandstormd
W=/tmp/gw-run; LOG=/tmp/gw
pass=0; fail=0
ok(){ echo "  PASS  $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL  $1"; fail=$((fail+1)); }
nc(){ [ -f "$1" ] && sed 's/\x1b\[[0-9;]*m//g' "$1" || true; }

pkill -x sandstormd 2>/dev/null; sleep 1
rm -rf "$W"; mkdir -p "$W"

# a template that echoes the headers it received, so we can check the
# gateway strips the credential before forwarding
TPL="$W/w/templates/py/rootfs"
mkdir -p "$TPL"/{proc,dev,sys,tmp,etc,var,root,app,usr,lib,lib64,bin,sbin}
printf 'root:x:0:0:root:/root:/bin/sh\n' > "$TPL/etc/passwd"
cat > "$TPL/app/s.py" <<'PY'
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_GET(s):
        body = json.dumps({"hello": "from-sandbox",
                           "headers": {k.lower(): v for k, v in s.headers.items()}}).encode()
        s.send_response(200); s.send_header("Content-Type","application/json")
        s.send_header("Content-Length", str(len(body))); s.end_headers(); s.wfile.write(body)
    def log_message(s,*a): pass
HTTPServer(("0.0.0.0", 8080), H).serve_forever()
PY

$B serve --master --node --bind 127.0.0.1:9743 \
  --state-db "$W/state.db" --pki-dir "$W/pki" --work-dir "$W/w" \
  --port-min 31000 --port-max 31100 \
  > "$LOG.log" 2>&1 &
for _ in $(seq 1 200); do nc "$LOG.log" | grep -q 'accepting scheduling' && break; sleep 0.1; done
sleep 3
TOK=$(cat "$W/pki/api-token")
A(){ curl -s --cacert "$W/pki/ca.pem" -H "authorization: Bearer $TOK" \
       -H 'content-type: application/json' "$@"; }

A -X POST -d '{"name":"py","rootfs":"rootfs","spec":{"args":["/usr/bin/python3","/app/s.py"],"memory_max_mib":128}}' \
  "https://127.0.0.1:9743/v1/templates" -o /dev/null

V=$(A -X POST -d '{"template":"py"}' "https://127.0.0.1:9743/v1/sandboxes")
SID=$(echo "$V"  | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
STK=$(echo "$V"  | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
GWP=$(echo "$V"  | python3 -c 'import json,sys;print(json.load(sys.stdin).get("gateway_port",""))')
echo "      sandbox=$SID gateway_port=${GWP:-none}"
[ -n "$GWP" ] && ok "the master allocated a host gateway port ($GWP)" \
  || { bad "no gateway_port in the create response: $V"; echo "=== verdict: $pass passed, $fail failed ==="; exit 1; }
[ "$GWP" -ge 31000 ] && [ "$GWP" -le 31100 ] && ok "it came from --port-min/--port-max" \
  || bad "$GWP is outside the configured range"

# wait for the listener
for _ in $(seq 1 400); do nc "$LOG.log" | grep -q 'gateway listening' && break; sleep 0.1; done
nc "$LOG.log" | grep -q 'gateway listening' && ok "the node opened the gateway" || bad "no 'gateway listening' in the log"
ss -tln | grep -q ":$GWP" && ok "the port is actually bound on the host" || bad "$GWP is not listening"

G(){ curl -s -o /dev/null -w '%{http_code}' -m 8 "$@" "http://127.0.0.1:$GWP/"; }

code=$(G)
[ "$code" = 401 ] && ok "no token: 401" || bad "no token gave $code"
code=$(G -H "x-sandstorm-sandbox-token: 00000000000000000000000000000000")
[ "$code" = 401 ] && ok "wrong token: 401" || bad "wrong token gave $code"
code=$(G -H "x-sandstorm-sandbox-token: ${STK:0:16}")
[ "$code" = 401 ] && ok "a prefix of the right token: 401" || bad "prefix gave $code"

# Wait for the sandbox itself, not just the listener: the gateway opens as
# soon as the sandbox is observed serving, but the process inside needs a
# moment to bind 8080.
for _ in $(seq 1 400); do
  curl -s -m 5 -H "x-sandstorm-sandbox-token: $STK" "http://127.0.0.1:$GWP/" 2>/dev/null | grep -q 'from-sandbox' && break
  sleep 0.1
done
body=$(curl -s -m 8 -H "x-sandstorm-sandbox-token: $STK" "http://127.0.0.1:$GWP/")
echo "$body" | grep -q 'from-sandbox' && ok "right token: the sandbox's own response came back" \
  || bad "right token: $body"

# the credential must not reach the sandbox
echo "$body" | python3 -c '
import json,sys
h=json.load(sys.stdin)["headers"]
sys.exit(0 if "x-sandstorm-sandbox-token" not in h else 1)
' && ok "the gateway did not forward the credential to the sandbox" \
  || bad "the sandbox saw the token: $(echo "$body" | python3 -c 'import json,sys;print(sorted(json.load(sys.stdin)["headers"]))')"

# cold: a clear 503, not a hang
A -X POST -d '{"tier":"frozen"}' "https://127.0.0.1:9743/v1/sandboxes/$SID/tier" -o /dev/null
sleep 3
t0=$(date +%s%N)
out=$(curl -s -m 10 -w '\n%{http_code}' -H "x-sandstorm-sandbox-token: $STK" "http://127.0.0.1:$GWP/" 2>&1)
t1=$(date +%s%N); ms=$(( (t1-t0)/1000000 ))
code=$(echo "$out" | tail -1)
# RFC section 8: "cold 沙箱的暴露端口返回连接拒绝". The node withdraws the
# listener when a sandbox stops serving, so the port closes and the caller
# is refused -- the same answer #327 gives on the sandbox's own port. curl
# reports a refusal as 000, so what matters is that it is fast.
case "$code" in
  000) [ "$ms" -lt 1000 ] && ok "frozen: the exposed port refuses, in ${ms} ms (RFC §8)"          || bad "refused but took ${ms} ms" ;;
  503) echo "$out" | grep -q 'not serving'          && ok "frozen: 503 'not serving' in ${ms} ms" || bad "503 but unclear: $out" ;;
  *)   bad "frozen gave $code in ${ms} ms: $out" ;;
esac
ss -tln | grep -q ":$GWP" && bad "the listener stayed open on a cold sandbox" \
  || ok "and the listener was withdrawn, so nothing is left answering"

# back to hot
A -X POST "https://127.0.0.1:9743/v1/sandboxes/$SID/touch" -o /dev/null
for _ in $(seq 1 400); do
  curl -s -m 5 -H "x-sandstorm-sandbox-token: $STK" "http://127.0.0.1:$GWP/" 2>/dev/null | grep -q 'from-sandbox' && break
  sleep 0.1
done
curl -s -m 5 -H "x-sandstorm-sandbox-token: $STK" "http://127.0.0.1:$GWP/" | grep -q 'from-sandbox' \
  && ok "promoted again: the gateway serves it" || bad "still not serving after touch"

# and the port closes with the sandbox
A -X DELETE "https://127.0.0.1:9743/v1/sandboxes/$SID" -o /dev/null
for _ in $(seq 1 300); do ss -tln | grep -q ":$GWP" || break; sleep 0.1; done
ss -tln | grep -q ":$GWP" && bad "the port outlived the sandbox" || ok "deleting the sandbox closed the port"

echo; echo "=== node warnings ==="; nc "$LOG.log" | grep -iE 'WARN|ERROR' | tail -3
echo "=== verdict: $pass passed, $fail failed ==="
pkill -x sandstormd 2>/dev/null
