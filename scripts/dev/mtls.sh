#!/usr/bin/env bash
# Does the control plane actually run on TLS, end to end, as two processes?
set -uo pipefail
B=/home/ubuntu/sandstorm/target/release/sandstormd
W=/tmp/mtls-run; LOG=/tmp/mtls
# tracing writes ANSI colour codes, which land between "ca=" and the hex.
nocolor(){ sed 's/\x1b\[[0-9;]*m//g' "$1"; }
pass=0; fail=0
ok(){ echo "  PASS  $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL  $1"; fail=$((fail+1)); }

pkill -x sandstormd 2>/dev/null; sleep 1
rm -rf "$W"; mkdir -p "$W"

# ---- master only, TLS on by default
$B serve --master --bind 127.0.0.1:9443 \
  --state-db "$W/state.db" --pki-dir "$W/pki" --work-dir "$W/work" \
  > "$LOG-master.log" 2>&1 &
for _ in $(seq 1 100); do [ -f "$W/pki/ca.pem" ] && break; sleep 0.1; done
sleep 2

nocolor "$LOG-master.log" | grep -q 'listening over TLS'"" && ok "master says it serves TLS" \
  || bad "no TLS line in the master log"

PIN=$(nocolor "$LOG-master.log" | grep -o 'ca=[0-9a-f]\{64\}' | head -1 | cut -d= -f2)
[ ${#PIN} -eq 64 ] && ok "master logged the CA fingerprint nodes pin (${PIN:0:12}…)" \
  || bad "no CA fingerprint logged"

TOK=$(cat "$W/pki/api-token")

# ---- plain HTTP must not work against a TLS port
code=$(curl -s -o /dev/null -w '%{http_code}' -m 5 "http://127.0.0.1:9443/healthz" 2>/dev/null)
[ "$code" = "000" ] && ok "plain HTTP to the TLS port gets nowhere (curl says $code)" \
  || bad "plain HTTP returned $code -- the port is not TLS-only"

# ---- TLS with the CA works
code=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$W/pki/ca.pem" \
       -m 5 "https://127.0.0.1:9443/healthz")
[ "$code" = "200" ] && ok "HTTPS with the CA: healthz 200" || bad "HTTPS healthz gave $code"

# ---- TLS without the CA is refused by the client
curl -s -o /dev/null -m 5 "https://127.0.0.1:9443/healthz" 2>/dev/null \
  && bad "an unverified client was allowed" \
  || ok "a client that does not trust the CA refuses the master"

# ---- the minted token carries the pin
NT=$(curl -s --cacert "$W/pki/ca.pem" -H "authorization: Bearer $TOK" \
     -H 'content-type: application/json' -d '{"pool":"default"}' \
     "https://127.0.0.1:9443/v1/node-tokens" | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
case "$NT" in
  ????????????????????????????????????????????????????????????????.*)
     [ "${NT%%.*}" = "$PIN" ] && ok "the registration token carries that exact CA pin" \
       || bad "token pin ${NT%%.*} != master pin $PIN" ;;
  *) bad "token has no pin: $NT" ;;
esac

# ---- a node with no certificate cannot use the node channel
code=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$W/pki/ca.pem" \
       -H "authorization: Bearer $TOK" -H 'content-type: application/json' \
       -d '{"node_id":"ghost","observed":[],"free_memory_mib":1,"pressure_some_avg10":0,"hold_ms":0}' \
       -m 5 "https://127.0.0.1:9443/v1/node/poll")
[ "$code" = "401" ] && ok "a certless caller with a valid bearer token is refused (401)" \
  || bad "certless poll gave $code, expected 401 -- the token alone must not be enough"

# ---- a real node registers over TLS and polls
$B serve --node --master-url "https://127.0.0.1:9443" --node-id bm-tls \
  --node-token "$NT" --api-token-file "$W/pki/api-token" \
  --state-db "$W/state.db" --pki-dir "$W/pki-node" --work-dir "$W/worknode" \
  > "$LOG-node.log" 2>&1 &
for _ in $(seq 1 200); do
  nocolor "$LOG-node.log" | grep -q 'accepting scheduling'"" && break; sleep 0.1
done
nocolor "$LOG-node.log" | grep -q 'registered'"" && ok "the node registered over TLS using the pinned token" \
  || bad "the node did not register"
[ -f "$W/pki-node/node/client.pem" ] && ok "it received a client certificate" || bad "no client cert"

sleep 3
N=$(curl -s --cacert "$W/pki/ca.pem" -H "authorization: Bearer $TOK" \
    "https://127.0.0.1:9443/v1/nodes")
echo "$N" | grep -q '"connected":true' && ok "the master shows it connected over mTLS" \
  || bad "node not connected: $N"

# the master resolved it from the certificate, not the body
nocolor "$LOG-master.log" | grep -q 'cert=[0-9a-f]\{64\}'"" && ok "the master recorded which certificate it issued" \
  || bad "no cert fingerprint recorded at registration"

# ---- revoke drops it
curl -s -o /dev/null --cacert "$W/pki/ca.pem" -H "authorization: Bearer $TOK" \
  -X POST "https://127.0.0.1:9443/v1/node/revoke/bm-tls"
sleep 4
nocolor "$LOG-node.log" | grep -qi 'refused this node'"" && ok "the revoked node is refused on its next poll" \
  || bad "the node did not see the revocation: $(tail -2 "$LOG-node.log" | tr '\n' ' ')"

pkill -x sandstormd 2>/dev/null
echo
echo "=== verdict: $pass passed, $fail failed ==="
