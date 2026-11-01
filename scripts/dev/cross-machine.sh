#!/usr/bin/env bash
# Cross-machine master/node, driven from the workstation.
#
# Topology is forced by reachability: only hk has a public address, and a
# sandstorm node DIALS OUT, so the master goes where it can be reached and the
# nodes go behind NAT. That is the shape RFC 15.5 describes, and it is also
# the lowest-risk split: install_rules() is node-side, so a master-only
# process touches no iptables, builds no namespace and runs no sandbox.
#
# hk is a production mail server and bj runs 1panel and postgres. Footprint
# on hk: one process, one temp dir, one listening port. On bj: iptables
# rules scoped to one /16, namespaces, cgroups -- all recorded before and
# diffed after.
set -uo pipefail
# 8090, found by probing: hk's security group is a strict allowlist, and
# nothing around 9000 is in it. An allowed-but-unused port refuses at once
# while a blocked one is dropped, so the time to fail distinguishes them --
# 43 ms against 3002 ms. 8090 was free on hk and reachable from both nodes.
PORT=8090
HK_PUB=43.161.236.94
W=/tmp/sandstorm-xm
BIN=/tmp/sandstormd
pass=0; fail=0
ok(){ echo "  PASS  $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL  $1"; fail=$((fail+1)); }
sshq(){ ssh -o BatchMode=yes -o ConnectTimeout=20 "$@"; }

echo "═══ 0. before: what is running, so it can be checked afterwards"
for h in bj hk; do
  sshq $h "systemctl list-units --type=service --state=running --no-legend --no-pager 2>/dev/null | awk '{print \$1}' | sort > $W-services-before.txt 2>/dev/null || true; mkdir -p /tmp; sudo iptables-save > /tmp/sandstorm-xm-ipt-before.txt 2>/dev/null; echo '  '$h': '\$(wc -l < /tmp/sandstorm-xm-ipt-before.txt)' iptables lines, '\$(systemctl list-units --type=service --state=running --no-legend --no-pager 2>/dev/null | wc -l)' services running'"
done

echo
echo "═══ 0b. no tunnels: this run is raw TCP over the public internet"
echo "  master will bind 0.0.0.0:$PORT on hk; nodes dial $HK_PUB:$PORT"

echo "═══ 1. master on hk (no iptables, no netns, no sandboxes)"
for h in bm bj hk; do
  left=$(sshq $h 'sudo ls -l /proc/*/exe 2>/dev/null | grep -ci sandstorm' 2>/dev/null)
  [ "${left:-0}" = 0 ] || { echo "  ABORT: $h already has $left sandstorm process(es) -- refusing to start on top of them"; exit 1; }
done
echo "  pre-flight: no sandstorm process on any of the three"
sshq hk "pkill -x sandstormd 2>/dev/null; rm -rf $W; mkdir -p $W; \
  nohup $BIN serve --master --bind 0.0.0.0:$PORT \
    --state-db $W/state.db --pki-dir $W/pki --work-dir $W/m \
    > $W/master.log 2>&1 < /dev/null & sleep 1; echo started"
for i in $(seq 1 60); do
  sshq hk "test -f $W/pki/api-token" 2>/dev/null && break; sleep 1
done
sleep 3
TOK=$(sshq hk "cat $W/pki/api-token" 2>/dev/null)
[ -n "$TOK" ] && ok "master up on hk, API token minted" || { bad "master did not start"; }
PIN=$(sshq hk "sed 's/\x1b\[[0-9;]*m//g' $W/master.log | grep -o 'ca=[0-9a-f]\{64\}' | head -1 | cut -d= -f2")
[ ${#PIN} -eq 64 ] && ok "CA fingerprint logged (${PIN:0:12}…)" || bad "no CA pin"

# a node token per node, minted on hk through its own loopback
mint(){ sshq hk "curl -s --cacert $W/pki/ca.pem -H 'authorization: Bearer $TOK' \
  -H 'content-type: application/json' -d '{\"pool\":\"default\"}' \
  https://127.0.0.1:$PORT/v1/node-tokens | python3 -c 'import json,sys;print(json.load(sys.stdin)[\"token\"])'"; }

echo
echo "═══ 2. nodes dial out from behind NAT: bm (AMD, kernel 7.0) and bj (Intel, 5.15)"
for h in bm bj; do
  NT=$(mint)
  case $h in bm) net=10.96.0.0;; bj) net=10.97.0.0;; esac
  # One root shell for the whole launch: `sudo cmd > file` has the OUTER,
  # unprivileged shell create the file, which failed with Permission denied
  # on a directory sudo had just made root-owned.
  sshq $h "sudo pkill -x sandstormd 2>/dev/null; sudo rm -rf $W; \
    sudo sh -c 'mkdir -p $W && nohup $BIN serve --node \
      --master-url https://$HK_PUB:$PORT --node-id xm-$h \
      --node-token \"$NT\" --state-db $W/n.db --pki-dir $W/pki --work-dir $W/w \
      --runsc-root /var/run/sandstorm-xm --net-base $net \
      > $W/node.log 2>&1 < /dev/null &' ; sleep 2; echo '  '$h' started'"
  sleep 1
  sshq $h "sudo test -s $W/node.log && echo '      log is being written' || echo '      WARNING: no log yet'"
done
for i in $(seq 1 90); do
  n=$(sshq hk "curl -s --cacert $W/pki/ca.pem -H 'authorization: Bearer $TOK' \
      https://127.0.0.1:$PORT/v1/nodes | python3 -c 'import json,sys;print(sum(1 for x in json.load(sys.stdin) if x[\"connected\"]))'" 2>/dev/null)
  [ "${n:-0}" -ge 2 ] && break; sleep 2
done

echo "      peers connected to the master's port:"
sshq hk "ss -tnH 2>/dev/null | awk '\$4 ~ /:$PORT\$/ {print \"        \" \$5}' | sort -u"
NODES=$(sshq hk "curl -s --cacert $W/pki/ca.pem -H 'authorization: Bearer $TOK' https://127.0.0.1:$PORT/v1/nodes")
echo "$NODES" | python3 -c '
import json,sys
d=json.load(sys.stdin)
for n in d: print(f"      {n[\"id\"]:8} pool={n[\"pool\"]:22} fp={n[\"fingerprint\"]} connected={n[\"connected\"]}")
' 2>/dev/null || echo "      $NODES"

conn=$(echo "$NODES" | python3 -c 'import json,sys;print(sum(1 for x in json.load(sys.stdin) if x["connected"]))' 2>/dev/null)
[ "${conn:-0}" = 2 ] && ok "two machines in two VPCs joined over mTLS across the internet" \
  || bad "connected=$conn"

fps=$(echo "$NODES" | python3 -c 'import json,sys;print(len({x["fingerprint"] for x in json.load(sys.stdin)}))' 2>/dev/null)
[ "${fps:-0}" = 2 ] && ok "different kernel and CPU vendor -> different fingerprints" \
  || bad "fingerprints: $fps distinct, expected 2"

pools=$(echo "$NODES" | python3 -c 'import json,sys;print(len({x["pool"] for x in json.load(sys.stdin)}))' 2>/dev/null)
[ "${pools:-0}" = 2 ] && ok "both asked for 'default'; the mismatched one was moved to its own pool" \
  || bad "pools: $pools distinct, expected 2 -- a mixed pool is the bug this prevents"

derived=$(echo "$NODES" | python3 -c 'import json,sys;print([x["pool"] for x in json.load(sys.stdin) if x["pool"]!="default"])' 2>/dev/null)
echo "      derived pool: $derived"
sshq hk "sed 's/\x1b\[[0-9;]*m//g' $W/master.log | grep -c 'fingerprint does not match'" \
  | grep -qv '^0$' && ok "the master logged why it moved it" || bad "no explanation logged"

echo
echo "═══ 3. placement must not cross the pool boundary"
sshq hk "curl -s --cacert $W/pki/ca.pem -H 'authorization: Bearer $TOK' -H 'content-type: application/json' \
  -d '{\"name\":\"py\",\"rootfs\":\"rootfs\",\"spec\":{\"args\":[\"/usr/bin/python3\",\"/app/s.py\"],\"memory_max_mib\":64}}' \
  https://127.0.0.1:$PORT/v1/templates -o /dev/null"
got=""; made=""
for i in 1 2 3; do
  v=$(sshq hk "curl -s --cacert $W/pki/ca.pem -H 'authorization: Bearer $TOK' -H 'content-type: application/json' \
      -d '{\"template\":\"py\",\"pool\":\"default\"}' https://127.0.0.1:$PORT/v1/sandboxes")
  nid=$(echo "$v" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("node_id",""))' 2>/dev/null)
  sid=$(echo "$v" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("id",""))' 2>/dev/null)
  got="$got $nid"; made="$made $sid"
done
# Placement is recorded by the master before any node acts, so this needs no
# sandbox to boot. Removed straight away: letting them retry would leave
# overlay mounts, namespaces and cgroups on a production box for no extra
# information, and bm already proves sandboxes run.
for sid in $made; do
  [ -n "$sid" ] && sshq hk "curl -s --cacert $W/pki/ca.pem -H 'authorization: Bearer $TOK' \
    -X DELETE https://127.0.0.1:$PORT/v1/sandboxes/$sid -o /dev/null"
done
echo "      created and deleted again, so no node builds them"
echo "      placed on:$got"
uniq_n=$(echo $got | tr ' ' '\n' | grep -v '^$' | sort -u | wc -l | tr -d ' ')
[ "$uniq_n" = 1 ] && ok "every sandbox for pool 'default' went to the one node in it" \
  || bad "placement crossed pools: $got"

echo
# The rule specs below must match what the node installed, byte for byte:
# `-m conntrack --ctstate RELATED,ESTABLISHED`. Writing it as `-m state
# --state ESTABLISHED,RELATED` -- a different match module in a different
# order -- made `iptables -C` never match, and one rule was left on a
# production machine. Which is why step 5 diffs the whole table rather than
# trusting this block to have worked.
echo "═══ 4. clean up and check nothing was left or broken"
sshq hk "pkill -x sandstormd 2>/dev/null; sleep 1; rm -rf $W; echo '  hk: master stopped, temp dir removed'"
for h in bm bj; do
  sshq $h "sudo pkill -x sandstormd 2>/dev/null; sleep 2
    for d in /sys/fs/cgroup/sandstorm/*/; do [ -d \"\$d\" ] && { echo 0 | sudo tee \$d/cgroup.freeze >/dev/null 2>&1; echo 1 | sudo tee \$d/cgroup.kill >/dev/null 2>&1; }; done
    sleep 1
    for ns in \$(ip netns list 2>/dev/null | awk '{print \$1}' | grep '^sandstorm-'); do sudo ip netns del \$ns 2>/dev/null; done
    for m in \$(mount | grep -o '$W[^ ]*merged' ); do sudo umount \$m 2>/dev/null; done
    sudo iptables -D OUTPUT -j SANDSTORM-COLD 2>/dev/null
    sudo iptables -F SANDSTORM-COLD 2>/dev/null; sudo iptables -X SANDSTORM-COLD 2>/dev/null
    for sub in 10.96.0.0/16 10.97.0.0/16; do
      while sudo iptables -C INPUT -s \$sub -j DROP 2>/dev/null; do sudo iptables -D INPUT -s \$sub -j DROP; done
      while sudo iptables -C FORWARD -s \$sub -j DROP 2>/dev/null; do sudo iptables -D FORWARD -s \$sub -j DROP; done
      while sudo iptables -C FORWARD -d \$sub -j DROP 2>/dev/null; do sudo iptables -D FORWARD -d \$sub -j DROP; done
      while sudo iptables -C INPUT -s \$sub -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null; do sudo iptables -D INPUT -s \$sub -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT; done
    done
    sudo rm -rf $W /var/run/sandstorm-xm
    echo '  '$h': processes, namespaces, cgroups, rules and dirs removed'"
done

echo
echo "═══ 5. the part that matters: did anything break"
for h in bj hk; do
  echo "  --- $h"
  sshq $h "sudo iptables-save > /tmp/sandstorm-xm-ipt-after.txt 2>/dev/null
    d=\$(diff <(grep -v '^#' /tmp/sandstorm-xm-ipt-before.txt | grep -vE '\[[0-9]+:[0-9]+\]') \
              <(grep -v '^#' /tmp/sandstorm-xm-ipt-after.txt  | grep -vE '\[[0-9]+:[0-9]+\]') | wc -l)
    echo '    iptables diff: '\$d' lines'
    [ \"\$d\" -gt 0 ] && diff <(grep -v '^#' /tmp/sandstorm-xm-ipt-before.txt | grep -vE '\[[0-9]+:[0-9]+\]') <(grep -v '^#' /tmp/sandstorm-xm-ipt-after.txt | grep -vE '\[[0-9]+:[0-9]+\]') | head -8 | sed 's/^/      /'
    echo '    sandstorm netns left: '\$(ip netns list 2>/dev/null | grep -c sandstorm)
    echo '    sandstorm procs left: '\$(sudo ls -l /proc/*/exe 2>/dev/null | grep -ci sandstorm)
    echo '    docker: '\$(systemctl is-active docker)'  containerd: '\$(systemctl is-active containerd)
    now=\$(systemctl list-units --type=service --state=running --no-legend --no-pager 2>/dev/null | awk '{print \$1}' | sort)
    before=\$(cat $W-services-before.txt 2>/dev/null)
    lost=\$(comm -23 <(echo \"\$before\") <(echo \"\$now\") | tr '\n' ' ')
    echo '    services that stopped: '\${lost:-none}
    rm -f /tmp/sandstorm-xm-ipt-before.txt /tmp/sandstorm-xm-ipt-after.txt $W-services-before.txt 2>/dev/null"
done

echo
echo "=== verdict: $pass passed, $fail failed ==="
