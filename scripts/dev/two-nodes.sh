#!/usr/bin/env bash
# Layer two (per-sandbox access token) and two nodes under one master.
#
# Two nodes on ONE machine: same fingerprint, so same pool -- which is what
# makes it a placement test rather than a pool test. It does not prove
# cross-machine anything; it proves the master treats two nodes as two.
set -uo pipefail
B=/home/ubuntu/sandstorm/target/release/sandstormd
W=/tmp/l2-run; LOG=/tmp/l2
pass=0; fail=0
ok(){ echo "  PASS  $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL  $1"; fail=$((fail+1)); }
nocolor(){ [ -f "$1" ] && sed 's/\x1b\[[0-9;]*m//g' "$1" || true; }

pkill -x sandstormd 2>/dev/null; sleep 1
rm -rf "$W"; mkdir -p "$W"

# master, TLS on
$B serve --master --bind 127.0.0.1:9543 \
  --state-db "$W/state.db" --pki-dir "$W/pki" --work-dir "$W/m" \
  > "$LOG-master.log" 2>&1 &
for _ in $(seq 1 120); do [ -f "$W/pki/api-token" ] && break; sleep 0.1; done
sleep 2
TOK=$(cat "$W/pki/api-token")
A(){ curl -s --cacert "$W/pki/ca.pem" -H "authorization: Bearer $TOK" \
       -H 'content-type: application/json' "$@"; }

NT(){ A -X POST -d '{"pool":"default"}' "https://127.0.0.1:9543/v1/node-tokens" \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])'; }

# two nodes: separate work dirs, runsc roots, pki dirs and address ranges
for n in a b; do
  case $n in a) net=10.90.0.0;; b) net=10.91.0.0;; esac
  $B serve --node --master-url "https://127.0.0.1:9543" --node-id "n-$n" \
    --node-token "$(NT)" --api-token-file "$W/pki/api-token" \
    --state-db "$W/state.db" --pki-dir "$W/pki-$n" --work-dir "$W/$n" \
    --runsc-root "/var/run/sandstorm-runsc-$n" --net-base "$net" \
    > "$LOG-node-$n.log" 2>&1 &
done
for _ in $(seq 1 300); do
  [ "$(nocolor "$LOG-node-a.log" | grep -c 'accepting scheduling')" -gt 0 ] \
    && [ "$(nocolor "$LOG-node-b.log" | grep -c 'accepting scheduling')" -gt 0 ] && break
  sleep 0.1
done
sleep 4

NODES=$(A "https://127.0.0.1:9543/v1/nodes")
n_conn=$(echo "$NODES" | python3 -c 'import json,sys;print(sum(1 for x in json.load(sys.stdin) if x["connected"]))')
[ "$n_conn" = 2 ] && ok "both nodes connected over mTLS" || bad "connected=$n_conn, want 2: $NODES"

pools=$(echo "$NODES" | python3 -c 'import json,sys;print(len({x["pool"] for x in json.load(sys.stdin)}))')
[ "$pools" = 1 ] && ok "same machine, same fingerprint -> one pool" \
  || bad "$pools pools: $(echo "$NODES" | python3 -c 'import json,sys;print([(x["id"],x["pool"],x["fingerprint"]) for x in json.load(sys.stdin)])')"

# The template's rootfs has to exist on BOTH nodes: the master records the
# template, the node needs the tree on disk. /usr /lib /lib64 /bin /sbin
# arrive as read-only binds, so only the mount points and the app are here.
for n in a b; do
  TPL="$W/$n/templates/py/rootfs"
  mkdir -p "$TPL"/{proc,dev,sys,tmp,etc,var,root,app,usr,lib,lib64,bin,sbin}
  printf 'root:x:0:0:root:/root:/bin/sh\n' > "$TPL/etc/passwd"
  cat > "$TPL/app/s.py" <<'PYEOF'
import time
while True:
    time.sleep(3600)
PYEOF
done

A -X POST -d '{"name":"py","rootfs":"rootfs","spec":{"args":["/usr/bin/python3","/app/s.py"],"memory_max_mib":128}}' \
  "https://127.0.0.1:9543/v1/templates" -o /dev/null

# ---- placement across two nodes
declare -A on
ids=(); toks=()
for i in $(seq 1 6); do
  v=$(A -X POST -d '{"template":"py"}' "https://127.0.0.1:9543/v1/sandboxes")
  nid=$(echo "$v" | python3 -c 'import json,sys;print(json.load(sys.stdin)["node_id"])' 2>/dev/null)
  sid=$(echo "$v" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])' 2>/dev/null)
  stk=$(echo "$v" | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])' 2>/dev/null)
  [ -n "${nid:-}" ] || { bad "create $i failed: $v"; break; }
  on[$nid]=$(( ${on[$nid]:-0} + 1 ))
  ids+=("$sid"); toks+=("$stk")
done
echo "      placement: $(for k in "${!on[@]}"; do printf '%s=%s ' "$k" "${on[$k]}"; done)"
[ "${#on[@]}" = 2 ] && ok "6 sandboxes spread over both nodes" || bad "all landed on one node"
[ "${on[n-a]:-0}" = 3 ] && [ "${on[n-b]:-0}" = 3 ] && ok "least-loaded split them 3/3" \
  || bad "uneven: a=${on[n-a]:-0} b=${on[n-b]:-0}"

# ---- 128-bit ids
sid=${ids[0]}; stk=${toks[0]}
hex=${sid#sb-}
[ ${#hex} -eq 32 ] && ok "sandbox id is 128 bits (${sid:0:14}…)" || bad "id is ${#hex} hex chars: $sid"
[ ${#stk} -eq 32 ] && ok "access token is 128 bits" || bad "token is ${#stk} chars"

# ---- layer two
code=$(A -o /dev/null -w '%{http_code}' -X POST -d '{"args":["true"]}' \
       "https://127.0.0.1:9543/v1/sandboxes/$sid/exec")
[ "$code" = 403 ] && ok "exec with the API token but no sandbox token: 403" \
  || bad "exec without the sandbox token gave $code"

code=$(A -o /dev/null -w '%{http_code}' -H "x-sandstorm-sandbox-token: ${toks[1]}" \
       -X POST -d '{"args":["true"]}' "https://127.0.0.1:9543/v1/sandboxes/$sid/exec")
[ "$code" = 403 ] && ok "another sandbox's token does not open this one: 403" \
  || bad "a foreign sandbox token gave $code"

# wait for it to be hot, then the real thing
for _ in $(seq 1 300); do
  t=$(A "https://127.0.0.1:9543/v1/sandboxes/$sid" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("tier",""))' 2>/dev/null)
  [ "$t" = "hot" ] && break; sleep 0.1
done
out=$(A -H "x-sandstorm-sandbox-token: $stk" -X POST \
      -d '{"args":["/bin/echo","layer-two"]}' "https://127.0.0.1:9543/v1/sandboxes/$sid/exec")
echo "$out" | grep -q 'layer-two' && ok "with its own token, exec runs in the sandbox" \
  || bad "exec with the right token: $out"

code=$(A -o /dev/null -w '%{http_code}' "https://127.0.0.1:9543/v1/sandboxes/$sid/ping")
[ "$code" = 403 ] && ok "ping needs it too" || bad "ping without the token gave $code"

echo
echo "=== node warnings ==="; for n in a b; do nocolor "$LOG-node-$n.log" | grep -i 'WARN\|ERROR' | tail -2; done
echo "=== verdict: $pass passed, $fail failed ==="
pkill -x sandstormd 2>/dev/null
