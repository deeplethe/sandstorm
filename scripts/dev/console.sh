#!/usr/bin/env bash
# The console, and the one-liner it hands out, end to end.
set -uo pipefail
B=/home/ubuntu/sandstorm/target/release/sandstormd
W=/tmp/console-run
pass=0; fail=0
ok(){ echo "  PASS  $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL  $1"; fail=$((fail+1)); }

pkill -x sandstormd 2>/dev/null; systemctl stop sandstorm-node 2>/dev/null
rm -rf "$W" /etc/systemd/system/sandstorm-node.service* /usr/local/bin/sandstormd /var/lib/sandstorm
mkdir -p "$W"

# master only, so install.sh has a real node to add
$B serve --master --bind 0.0.0.0:9843 \
  --state-db "$W/state.db" --pki-dir "$W/pki" --work-dir "$W/m" \
  > "$W/master.log" 2>&1 &
for _ in $(seq 1 120); do [ -f "$W/pki/api-token" ] && break; sleep 0.1; done
sleep 2
TOK=$(cat "$W/pki/api-token")
M=https://127.0.0.1:9843
A(){ curl -s --cacert "$W/pki/ca.pem" -H "authorization: Bearer $TOK" -H 'content-type: application/json' "$@"; }

# ---- the console
h=$(curl -s --cacert "$W/pki/ca.pem" "$M/")
echo "$h" | grep -q '<title>Sandstorm console</title>' && ok "GET / serves the console" || bad "no console at /"
echo "$h" | grep -q 'sessionStorage' && ok "it keeps the token in sessionStorage only" || bad "no sessionStorage"
for marker in 'Add a node' 'node-bootstrap' 'pairing_code'; do
  echo "$h" | grep -q "$marker" || bad "the console is missing '$marker'"
done
echo "$h" | grep -q 'pairing_code' && ok "both ways of adding a node are there"
code=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$W/pki/ca.pem" "$M/")
[ "$code" = 200 ] && ok "the console needs no token to load (it asks for one)" || bad "/ gave $code"

# ---- ca.pem
curl -s --cacert "$W/pki/ca.pem" "$M/ca.pem" > "$W/got-ca.pem"
cmp -s "$W/got-ca.pem" "$W/pki/ca.pem" && ok "GET /ca.pem is byte-identical to pki/ca.pem" \
  || bad "/ca.pem differs from the real CA"

# ---- the binary
curl -s --cacert "$W/pki/ca.pem" -o "$W/got-bin" "$M/sandstormd"
chmod +x "$W/got-bin"
if cmp -s "$W/got-bin" "$B"; then ok "GET /sandstormd is the master's own binary, byte for byte"
else bad "served binary differs ($(stat -c%s "$W/got-bin") vs $(stat -c%s "$B") bytes)"; fi
"$W/got-bin" --version >/dev/null 2>&1 && ok "and it runs" || bad "the served binary does not run"

# ---- install.sh
s=$(curl -s --cacert "$W/pki/ca.pem" "$M/install.sh")
echo "$s" | grep -q '@MASTER@' && bad "install.sh still has the @MASTER@ placeholder" \
  || ok "install.sh has the master's address substituted"
echo "$s" | grep -q 'https://127.0.0.1:9843' && ok "and it is the address we asked through" \
  || bad "substituted the wrong host: $(echo "$s" | grep -m1 '^MASTER=')"

# ---- the one line the console shows
BS=$(A -X POST -d "{\"pool\":\"default\",\"ttl_secs\":900,\"master_url\":\"$M\"}" "$M/v1/node-bootstrap")
CMD=$(echo "$BS" | python3 -c 'import json,sys;print(json.load(sys.stdin)["install_command"])')
PIN=$(echo "$BS" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("ca_fingerprint",""))')
UNV=$(echo "$BS" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("first_fetch_is_unverified"))')
echo "      $(echo "$CMD" | head -1)"
[ -n "$PIN" ] && ok "the response states the CA fingerprint" || bad "no ca_fingerprint"
[ "$UNV" = "True" ] && ok "and says the first fetch is unverified" || bad "first_fetch_is_unverified=$UNV"
real=$(openssl x509 -in "$W/pki/ca.pem" -outform der | openssl dgst -sha256 -hex | sed 's/.*= *//')
[ "$PIN" = "$real" ] && ok "the fingerprint matches the real CA" || bad "pin $PIN != $real"

# the exact first hop the OLD command 404'd on
code=$(curl -s -o /dev/null -w '%{http_code}' -k "$M/install.sh")
[ "$code" = 200 ] && ok "the first hop of the command now works (was a 404 before)" \
  || bad "curl -k $M/install.sh gave $code"

# ---- run it for real
echo "      running the one-liner…"
eval "$CMD" > "$W/install.log" 2>&1
rc=$?
[ $rc = 0 ] && ok "install.sh ran to completion" || { bad "install.sh exited $rc"; tail -6 "$W/install.log" | sed 's/^/        /'; }
grep -q 'CA fingerprint matches the token' "$W/install.log" && ok "it verified the CA against the token's pin" \
  || bad "no fingerprint check in the log"
[ -x /usr/local/bin/sandstormd ] && ok "the binary is installed" || bad "no /usr/local/bin/sandstormd"
systemctl is-active sandstorm-node >/dev/null 2>&1 && ok "sandstorm-node is running" \
  || { bad "unit not active: $(systemctl is-active sandstorm-node 2>&1)"; journalctl -u sandstorm-node -n 6 --no-pager 2>/dev/null | sed 's/^/        /'; }
[ "$(stat -c '%a' /etc/systemd/system/sandstorm-node.service.d/token.conf 2>/dev/null)" = 600 ] \
  && ok "the token drop-in is 0600" || bad "token.conf mode is $(stat -c '%a' /etc/systemd/system/sandstorm-node.service.d/token.conf 2>/dev/null)"

for _ in $(seq 1 150); do
  n=$(A "$M/v1/nodes" | python3 -c 'import json,sys;print(sum(1 for x in json.load(sys.stdin) if x["connected"]))' 2>/dev/null)
  [ "${n:-0}" -ge 1 ] && break; sleep 0.4
done
[ "${n:-0}" -ge 1 ] && ok "the node joined and the console would show it connected" \
  || bad "no connected node after 60s"
A "$M/v1/nodes" | python3 -c 'import json,sys
for x in json.load(sys.stdin): print(f"      {x[\"id\"]} pool={x[\"pool\"]} connected={x[\"connected\"]}")' 2>/dev/null

# ---- clean up
systemctl disable --now sandstorm-node >/dev/null 2>&1
rm -rf /etc/systemd/system/sandstorm-node.service* /usr/local/bin/sandstormd /var/lib/sandstorm
systemctl daemon-reload
pkill -x sandstormd 2>/dev/null
echo
echo "=== verdict: $pass passed, $fail failed ==="
