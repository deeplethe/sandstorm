#!/usr/bin/env bash
# Stand a sandstorm fleet up across several machines, look at it, take it down.
#
#   scripts/fleet.sh up --master HOST --nodes H1,H2,H3 [--listen 80]
#   scripts/fleet.sh add --address 10.0.0.5[:80] --code <pairing code>
#   scripts/fleet.sh seed
#   scripts/fleet.sh verify
#   scripts/fleet.sh status
#   scripts/fleet.sh down
#
# HOST is whatever `ssh` accepts -- an alias from ~/.ssh/config is the
# usual thing. The master needs a port the nodes can reach; nodes dial
# out, so they need no inbound port of their own.
#
# Nodes join by running the master's own `install.sh`, the same one the
# console hands out, rather than by some private path this script invents.
# That is deliberate: it is the only way the one-liner gets exercised
# against a machine that is not also the master.
#
# ---------------------------------------------------------------------
# What it writes on each machine, and nothing else:
#
#   master   /usr/local/bin/sandstormd, /var/lib/sandstorm, and a
#            sandstorm-master.service unit. No iptables, no namespaces --
#            install_rules() is node-side, so a master-only process
#            touches neither.
#
#   node     the same binary and state dir, a sandstorm-node.service unit
#            with its token in a 0600 drop-in, plus the things a node
#            needs to run sandboxes: a SANDSTORM-COLD chain jumped to from
#            OUTPUT, INPUT/FORWARD rules scoped to the sandbox subnet,
#            one netns per sandbox (sandstorm-<id>) and its veth (sst*).
#
# `down` removes exactly those. It deletes iptables rules by replaying
# what `iptables -S` itself prints, never by reconstructing the match
# syntax -- a hand-written `-m state --state ESTABLISHED,RELATED` does
# not delete a rule the node installed as `-m conntrack --ctstate
# RELATED,ESTABLISHED`, and that left a rule behind on a production box
# once already.
#
# Every host is snapshotted before anything is installed, and `down`
# diffs against that snapshot and says what changed.
# ---------------------------------------------------------------------
set -uo pipefail

STATE_DIR="${SANDSTORM_FLEET_STATE:-$HOME/.sandstorm-fleet}"
STATE="$STATE_DIR/fleet.env"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=20)

# How the node list is printed, in both `up` and `status`.
#
# Passed with -c and fed on stdin. It used to be `… | python3 - <<'PY'`,
# where the heredoc takes stdin and python reads its PROGRAM from there
# -- so json.load(sys.stdin) saw EOF and the whole thing threw while the
# surrounding script carried on reporting success.
read -r -d '' PRETTY_NODES <<'PYSRC'
import json, sys
from collections import defaultdict
try:
    nodes = json.load(sys.stdin)
except Exception as e:
    print(f"  (the master did not answer with a node list: {e})")
    raise SystemExit(1)
if not nodes:
    print("  (no node has registered)")
    raise SystemExit(0)
pools = defaultdict(list)
for n in nodes:
    pools[n["pool"]].append(n)
for pool in sorted(pools):
    print(f"  pool {pool}")
    for n in pools[pool]:
        why = n.get("not_admitting")
        print(f"    {n['id']:<16} connected={n['connected']} "
              f"free={n['free_mib']} MiB psi={n['psi_some_avg10']:.1f} "
              f"fp={n['fingerprint'][:12]}..."
              + (f"  NOT ADMITTING: {why}" if why else ""))
if len(pools) > 1:
    print()
    print(f"  NOTE  the fleet formed {len(pools)} pools, so a load test against any one")
    print("        of them uses only part of it. Run loadtest.py once per pool, or")
    print("        make the machines match (same kernel, same CPU model).")
PYSRC

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '  \033[32mok\033[0m    %s\n' "$*"; }
warn() { printf '  \033[33mwarn\033[0m  %s\n' "$*"; }
die()  { printf '  \033[31mfail\033[0m  %s\n' "$*" >&2; exit 1; }
sshq() { ssh "${SSH_OPTS[@]}" "$@"; }

usage() {
  sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

up options
  --master HOST          machine to run the control plane on (required)
  --nodes H1,H2,...      machines to join as nodes (required)
  --port N               master's listening port (default 8080)
  --bind-addr ADDR       interface the master listens on (default the
                         address the nodes use, so a fleet on a private
                         network does not also answer on the public one;
                         0.0.0.0 for everything)
  --master-addr ADDR     address the NODES use to reach the master.
                         Defaults to --master, which is right when the
                         ssh alias resolves the same from the nodes; set
                         it explicitly when it does not (NAT, a private
                         interface, a public IP).
  --pool NAME            pool to join nodes into (default "default")
  --binary PATH          a prebuilt static sandstormd to ship
  --build-on HOST        build it there instead (needs cargo + the musl target)
  --net-base A.B.C.D     sandbox subnet base (default 10.86.0.0)
  --net-prefix N         and its prefix (default 16)
  --listen PORT          nodes listen on PORT and the master connects to
                         them, instead of each node dialing the master.
                         Use it when the master can reach the nodes; leave
                         it off for nodes behind NAT

add options (a node that is already installed and listening)
  --address A            host or host:port the master can reach (port 80
                         if left off)
  --code C               the node's pairing code, from
                         /var/lib/sandstorm/pki/node/pairing-code on the node
  --node-id N            defaults to what the node reports
  --pool P               default "default"
  --yes                  do not ask before touching each machine

down options
  --keep-state           leave /var/lib/sandstorm in place
EOF
  exit 1
}

# --- helpers ---------------------------------------------------------

# A host's pre-install state, kept so `down` can prove it went back.
snapshot_host() {
  local h="$1" out="$2"
  sshq "$h" '
    echo "kernel=$(uname -r)"
    echo "arch=$(uname -m)"
    echo "cpu=$(grep -m1 "model name" /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed "s/^ //")"
    echo "mem_mb=$(( $(grep MemTotal /proc/meminfo | awk "{print \$2}") / 1024 ))"
    echo "iptables_lines=$(sudo iptables-save 2>/dev/null | wc -l)"
    echo "sandstorm_rules=$(sudo iptables -S 2>/dev/null | grep -cE "SANDSTORM-COLD|10\.86\." )"
    echo "sandstorm_netns=$(ip netns list 2>/dev/null | grep -c "^sandstorm-")"
    echo "netns=$(ip netns list 2>/dev/null | wc -l)"
    echo "sandstorm_procs=$(sudo ls -l /proc/*/exe 2>/dev/null | grep -c sandstormd)"
    for t in runsc runc iptables ip systemctl curl; do
      command -v "$t" >/dev/null 2>&1 && echo "has_$t=1" || echo "has_$t=0"
    done
    systemctl list-units --type=service --state=running --no-legend --no-pager 2>/dev/null \
      | awk "{print \"svc=\" \$1}" | sort
  ' > "$out" 2>/dev/null
}

getval() { sed -n "s/^$2=//p" "$1" | head -1; }

# curl the master, from the master, over loopback. The workstation may
# have no route to the control plane -- a fleet behind NAT is the normal
# case -- and nothing here needs one.
mapi() {
  local method="$1" path="$2" body="${3:-}"
  # Built as one string for the REMOTE shell to parse, with the quotes
  # the remote needs. An array expanded with ${a[*]} would arrive as
  # bare words and "Bearer <token>" would become two arguments.
  local cmd
  cmd="sudo curl -sS --cacert /var/lib/sandstorm/pki/ca.pem"
  cmd="$cmd -H \"authorization: Bearer \$(sudo cat /var/lib/sandstorm/pki/api-token)\""
  cmd="$cmd -H 'content-type: application/json' -X $method"
  [ -n "$body" ] && cmd="$cmd --data-raw '$body'"
  # The bind address, not loopback: the master listens on one interface
  # by default now, so 127.0.0.1 answers nothing. Its certificate carries
  # the bind address as a SAN, so this verifies.
  cmd="$cmd 'https://${MASTER_BIND:-127.0.0.1}:$MASTER_PORT$path'"
  sshq "$MASTER_HOST" "$cmd"
}

preflight() {
  local h="$1" snap="$2" role="$3" bad=0
  local kern arch procs
  kern=$(getval "$snap" kernel); arch=$(getval "$snap" arch)
  procs=$(getval "$snap" sandstorm_procs)
  [ -n "$kern" ] || die "$h: cannot ssh in (or /proc is not readable)"
  printf '  %-14s %s %s  %s MiB  %s\n' "$h" "$arch" "$kern" \
    "$(getval "$snap" mem_mb)" "$(getval "$snap" cpu)"
  [ "${procs:-0}" = 0 ] || { warn "$h already runs $procs sandstorm process(es)"; bad=1; }
  for t in curl systemctl; do
    [ "$(getval "$snap" "has_$t")" = 1 ] || { warn "$h has no $t"; bad=1; }
  done
  if [ "$role" = node ]; then
    for t in runsc iptables ip; do
      [ "$(getval "$snap" "has_$t")" = 1 ] \
        || warn "$h has no $t -- it will join, but cannot start a sandbox"
    done
  fi
  return $bad
}

confirm() {
  [ "${ASSUME_YES:-0}" = 1 ] && return 0
  printf '\n  %s\n  continue? [y/N] ' "$1"
  read -r a < /dev/tty
  [ "$a" = y ] || [ "$a" = Y ]
}

# Each node dials the master: mint a token, run the one-liner over there.
join_dialing() {
  say "3. joining ${#NODE_LIST[@]} node(s) with the master's own install.sh"
  for n in "${NODE_LIST[@]}"; do
    local bs cmd
    bs=$(mapi POST /v1/node-bootstrap "{\"pool\":\"$POOL\",\"ttl_secs\":900,\"master_url\":\"$M\"}")
    cmd=$(printf '%s' "$bs" | python3 -c 'import json,sys;print(json.load(sys.stdin)["install_command"])' 2>/dev/null)
    [ -n "$cmd" ] || { warn "$n: could not mint a join token (is the master reachable from here?)"; continue; }
    say "   $n"
    # The one-liner ends `| sudo SANDSTORM_...=x sh`, so the node id has to go
    # in beside the others -- `SANDSTORM_NODE_ID=$n <whole command>` would set
    # it for the curl at the head of the pipe and not for the shell at the
    # tail, which is why the first run named the node after its hostname.
    cmd="${cmd/| sudo /| sudo SANDSTORM_NODE_ID=$n }"
    # pipefail, and then check the unit landed. The one-liner is
    # `curl ... | sudo ... sh`, so without it the exit status is the
    # shell's at the tail of the pipe -- which succeeds on the empty
    # input a failed curl produces. The first run reported six nodes
    # joined when none of them had.
    sshq "$n" "set -o pipefail; $cmd" > "$STATE_DIR/install-$n.log" 2>&1
    if sshq "$n" 'test -f /etc/systemd/system/sandstorm-node.service'; then
      ok "$n joined"
    else
      warn "$n: install.sh did not install a node; see $STATE_DIR/install-$n.log"
      tail -3 "$STATE_DIR/install-$n.log" | sed 's/^/        /'
    fi
  done

}

# Each node listens and the master connects: install the node with
# --listen, read back the pairing code it wrote, and hand the master the
# address and the code. Nothing on the node knows the master's address.
join_listening() {
  say "3. installing ${#NODE_LIST[@]} listening node(s), then adding them to the master"
  for n in "${NODE_LIST[@]}"; do
    say "   $n"
    # Fully verified, with no -k anywhere: the CA goes over ssh first, so
    # the installer and the binary are both checked against it.
    if ! sshq "$n" 'cat > /tmp/sandstorm-ca.pem' < "$STATE_DIR/ca.pem"; then
      warn "$n: could not copy the CA over"; continue
    fi
    sshq "$n" "set -o pipefail; curl -sSfL --cacert /tmp/sandstorm-ca.pem $M/install.sh \
      | sudo SANDSTORM_MASTER_URL=$M SANDSTORM_CA=/tmp/sandstorm-ca.pem SANDSTORM_LISTEN=0.0.0.0:$LISTEN_PORT \
             SANDSTORM_NODE_ID=$n sh; rm -f /tmp/sandstorm-ca.pem" > "$STATE_DIR/install-$n.log" 2>&1
    local code addr added
    code=$(sshq "$n" 'sudo cat /var/lib/sandstorm/pki/node/pairing-code 2>/dev/null')
    if [ -z "$code" ]; then
      warn "$n: no pairing code; see $STATE_DIR/install-$n.log"
      tail -3 "$STATE_DIR/install-$n.log" | sed 's/^/        /'
      continue
    fi
    # The address the master should dial is the node's own address on the
    # route TO the master -- on a private network that is the private one,
    # which ssh's address (often public) is not.
    addr=$(sshq "$n" "ip -o route get $MASTER_ADDR 2>/dev/null | sed -n 's/.* src \\([^ ]*\\).*/\\1/p'")
    addr="${addr:-$(ssh -G "$n" 2>/dev/null | awk '/^hostname /{print $2; exit}')}"
    added=$(add_node "$addr:$LISTEN_PORT" "$code" "$n" "$POOL")
    case "$added" in
      *'"id"'*) ok "$n added at $addr:$LISTEN_PORT -- the master drives it" ;;
      *) warn "$n: the master would not add it: $added" ;;
    esac
  done
}

# Ask the master to take over a listening node. The code goes on stdin to
# the remote curl rather than the command line, where ps would show it.
add_node() {
  local address="$1" code="$2" node_id="${3:-}" pool="${4:-default}"
  local body
  body=$(python3 -c 'import json,sys
d = {"address": sys.argv[1], "pairing_code": sys.argv[2], "pool": sys.argv[4]}
if sys.argv[3]: d["node_id"] = sys.argv[3]
print(json.dumps(d))' "$address" "$code" "$node_id" "$pool")
  printf '%s' "$body" | sshq "$MASTER_HOST" "sudo curl -sS --cacert /var/lib/sandstorm/pki/ca.pem \
    -H \"authorization: Bearer \$(sudo cat /var/lib/sandstorm/pki/api-token)\" \
    -H 'content-type: application/json' --data-binary @- \
    'https://${MASTER_BIND:-127.0.0.1}:$MASTER_PORT/v1/nodes'"
}

cmd_add() {
  local ADDRESS="" CODE="" NODE_ID="" APOOL=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --address) ADDRESS="$2"; shift 2;;
      --code) CODE="$2"; shift 2;;
      --node-id) NODE_ID="$2"; shift 2;;
      --pool) APOOL="$2"; shift 2;;
      *) usage;;
    esac
  done
  [ -n "$ADDRESS" ] && [ -n "$CODE" ] || usage
  [ -f "$STATE" ] || die "no fleet recorded in $STATE; run 'up' first, or point SANDSTORM_FLEET_STATE at one"
  # shellcheck disable=SC1090
  . "$STATE"
  MASTER_HOST="$MASTER"; MASTER_PORT="$PORT"; MASTER_BIND="${BIND_ADDR:-127.0.0.1}"
  local out
  out=$(add_node "$ADDRESS" "$CODE" "$NODE_ID" "${APOOL:-$POOL}")
  case "$out" in
    *'"id"'*) ok "added: $out" ;;
    *) die "the master would not add it: $out" ;;
  esac
}

# --- up --------------------------------------------------------------

cmd_up() {
  local MASTER="" NODES="" PORT=8080 MASTER_ADDR="" BIND_ADDR="" POOL=default
  local BINARY="" BUILD_ON="" NET_BASE=10.86.0.0 NET_PREFIX=16 LISTEN_PORT=""
  ASSUME_YES=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --master) MASTER="$2"; shift 2;;
      --nodes) NODES="$2"; shift 2;;
      --port) PORT="$2"; shift 2;;
      --master-addr) MASTER_ADDR="$2"; shift 2;;
      --bind-addr) BIND_ADDR="$2"; shift 2;;
      --pool) POOL="$2"; shift 2;;
      --binary) BINARY="$2"; shift 2;;
      --build-on) BUILD_ON="$2"; shift 2;;
      --net-base) NET_BASE="$2"; shift 2;;
      --net-prefix) NET_PREFIX="$2"; shift 2;;
      --listen) LISTEN_PORT="$2"; shift 2;;
      --yes) ASSUME_YES=1; shift;;
      *) usage;;
    esac
  done
  [ -n "$MASTER" ] && [ -n "$NODES" ] || usage
  IFS=, read -r -a NODE_LIST <<< "$NODES"
  # Default to the address ssh would dial, not the alias. An alias lives
  # in ~/.ssh/config on THIS machine and means nothing on a node: the
  # first run of this handed the nodes `https://f1:8090` and every one of
  # them failed with `Could not resolve host: f1`.
  if [ -z "$MASTER_ADDR" ]; then
    MASTER_ADDR=$(ssh -G "$MASTER" 2>/dev/null | awk '/^hostname /{print $2; exit}')
    MASTER_ADDR="${MASTER_ADDR:-$MASTER}"
    [ "$MASTER_ADDR" = "$MASTER" ] || say "master alias $MASTER resolves to $MASTER_ADDR for the nodes"
  fi
  mkdir -p "$STATE_DIR"
  [ -f "$STATE" ] && die "a fleet is already recorded in $STATE; run 'down' first"

  say "0. what is on each machine before anything is installed"
  local snaps="$STATE_DIR/before"; mkdir -p "$snaps"; rm -f "$snaps"/*
  local problems=0
  snapshot_host "$MASTER" "$snaps/$MASTER.txt"
  preflight "$MASTER" "$snaps/$MASTER.txt" master || problems=1
  for n in "${NODE_LIST[@]}"; do
    snapshot_host "$n" "$snaps/$n.txt"
    preflight "$n" "$snaps/$n.txt" node || problems=1
  done

  # The thing that decides whether this is one pool or several. Said now,
  # before anything is installed, because it changes what a load test
  # even means: a node whose host fingerprint differs from the pool's
  # joins a derived pool, since a runsc checkpoint does not restore
  # across differing hosts.
  say "0b. will these machines form one pool?"
  local kerns=() arches=()
  for n in "${NODE_LIST[@]}"; do
    kerns+=("$(getval "$snaps/$n.txt" kernel)")
    arches+=("$(getval "$snaps/$n.txt" arch)")
  done
  local uniq_k uniq_a
  uniq_k=$(printf '%s\n' "${kerns[@]}" | sort -u | wc -l | tr -d ' ')
  uniq_a=$(printf '%s\n' "${arches[@]}" | sort -u | wc -l | tr -d ' ')
  if [ "$uniq_k" = 1 ] && [ "$uniq_a" = 1 ]; then
    ok "every node reports the same kernel and arch -- likely one pool"
  else
    warn "$uniq_k kernel(s) and $uniq_a arch(es) across the nodes."
    warn "nodes that differ will land in derived pools (${POOL}#<fp8>) and a"
    warn "run against '$POOL' will load only part of the fleet. The exact"
    warn "fingerprint is computed by the node, so the grouping is confirmed"
    warn "after they join -- see the summary at the end."
  fi
  [ "$problems" = 0 ] || confirm "warnings above" || die "stopped"

  # --- the binary ----------------------------------------------------
  say "1. the binary"
  local LOCAL_BIN="$STATE_DIR/sandstormd"
  if [ -n "$BINARY" ]; then
    cp "$BINARY" "$LOCAL_BIN" || die "cannot read $BINARY"
    ok "using $BINARY"
  else
    BUILD_ON="${BUILD_ON:-${NODE_LIST[0]}}"
    say "   building on $BUILD_ON (static musl, so it runs anywhere in the fleet)"
    local here; here=$(cd "$(dirname "$0")/.." && pwd)
    sshq "$BUILD_ON" 'mkdir -p ~/sandstorm-build' || die "cannot reach $BUILD_ON"
    tar -C "$here" -czf - --exclude target --exclude .git . \
      | sshq "$BUILD_ON" 'tar -C ~/sandstorm-build -xzf -' || die "cannot copy the source"
    sshq "$BUILD_ON" 'export PATH=$HOME/.cargo/bin:$PATH
      cd ~/sandstorm-build
      rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1
      cargo build --release --target x86_64-unknown-linux-musl --bin sandstormd' \
      || die "build failed on $BUILD_ON"
    sshq "$BUILD_ON" 'cat ~/sandstorm-build/target/x86_64-unknown-linux-musl/release/sandstormd' \
      > "$LOCAL_BIN" || die "cannot fetch the built binary"
  fi
  chmod +x "$LOCAL_BIN"
  local SUM; SUM=$(shasum -a 256 "$LOCAL_BIN" | awk '{print $1}')
  ok "sandstormd  $SUM"

  # --- master --------------------------------------------------------
  BIND_ADDR="${BIND_ADDR:-$MASTER_ADDR}"
  say "2. master on $MASTER, listening on $BIND_ADDR:$PORT"
  confirm "this installs a systemd unit and starts a process on $MASTER" || die "stopped"
  sshq "$MASTER" "sudo install -D -m 0755 /dev/stdin /usr/local/bin/sandstormd" \
    < "$LOCAL_BIN" || die "cannot install the binary on $MASTER"
  sshq "$MASTER" "sudo install -d -m 0750 /var/lib/sandstorm
    sudo tee /etc/systemd/system/sandstorm-master.service >/dev/null <<'UNIT'
[Unit]
Description=sandstorm master (fleet.sh)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/sandstormd serve --master --bind $BIND_ADDR:$PORT \\
  --state-db /var/lib/sandstorm/state.db --pki-dir /var/lib/sandstorm/pki \\
  --work-dir /var/lib/sandstorm
Restart=on-failure
RestartSec=2s

[Install]
WantedBy=multi-user.target
UNIT
    sudo systemctl daemon-reload
    sudo systemctl enable --now sandstorm-master" || die "the master unit would not start"

  local TOKEN="" i
  for i in $(seq 1 30); do
    TOKEN=$(sshq "$MASTER" 'sudo cat /var/lib/sandstorm/pki/api-token 2>/dev/null')
    [ -n "$TOKEN" ] && break
    sleep 1
  done
  [ -n "$TOKEN" ] || die "the master never wrote its api-token; check journalctl -u sandstorm-master"
  sshq "$MASTER" 'sudo cat /var/lib/sandstorm/pki/ca.pem' > "$STATE_DIR/ca.pem" \
    || die "cannot fetch the CA"
  local FP; FP=$(openssl x509 -in "$STATE_DIR/ca.pem" -outform der 2>/dev/null \
    | openssl dgst -sha256 -hex | sed 's/.*= *//')
  ok "master up; CA ${FP:0:16}…"

  local M="https://$MASTER_ADDR:$PORT"
  MASTER_HOST="$MASTER"; MASTER_PORT="$PORT"; MASTER_BIND="$BIND_ADDR"
  if curl -sS --connect-timeout 8 --cacert "$STATE_DIR/ca.pem" -o /dev/null "$M/healthz" 2>/dev/null; then
    ok "reachable at $M from this workstation too, so loadtest.py can run here"
  else
    warn "this workstation cannot reach $M -- fine for the deploy, which talks to"
    warn "the master over ssh, but run loadtest.py on $MASTER itself (or open the port)."
  fi

  cat > "$STATE" <<EOF
MASTER=$MASTER
MASTER_ADDR=$MASTER_ADDR
BIND_ADDR=$BIND_ADDR
PORT=$PORT
NODES=$NODES
POOL=$POOL
TOKEN=$TOKEN
CA=$STATE_DIR/ca.pem
NET_BASE=$NET_BASE
NET_PREFIX=$NET_PREFIX
SUM=$SUM
EOF
  chmod 600 "$STATE"

  # --- nodes ---------------------------------------------------------
  if [ -n "$LISTEN_PORT" ]; then
    join_listening
  else
    join_dialing
  fi

  say "4. what the master sees"
  local j
  for i in $(seq 1 40); do
    j=$(mapi GET /v1/nodes)
    local up; up=$(printf '%s' "$j" | python3 -c 'import json,sys;print(sum(1 for n in json.load(sys.stdin) if n["connected"]))' 2>/dev/null)
    [ "${up:-0}" -ge "${#NODE_LIST[@]}" ] && break
    sleep 1
  done
  printf '%s' "$j" | python3 -c "$PRETTY_NODES"
  local joined
  joined=$(printf '%s' "$j" | python3 -c 'import json,sys;print(sum(1 for n in json.load(sys.stdin) if n["connected"]))' 2>/dev/null)
  if [ "${joined:-0}" -lt "${#NODE_LIST[@]}" ]; then
    warn "${joined:-0} of ${#NODE_LIST[@]} node(s) connected. The master is up and"
    warn "recorded in $STATE, so 'status' and 'down' work -- but do not load it yet."
    warn "Each failed join left a log in $STATE_DIR/install-<host>.log."
    say "partly up"
  else
    say "ready"
  fi
  cat <<EOF

  master   $M
  token    $STATE_DIR (kept at mode 600; also /var/lib/sandstorm/pki/api-token on $MASTER)
  CA       $STATE_DIR/ca.pem

  load it:
    python3 bench/fleet/loadtest.py --master $M --ca $STATE_DIR/ca.pem \\
      --token-file <(printf %s "\$(sed -n s/^TOKEN=//p $STATE)") \\
      --pool $POOL --total 500 --concurrency 32 --hold-secs 120 --touch --out run.json

  first:        scripts/fleet.sh seed      # a node cannot start a sandbox
                                           # until a template rootfs is on it
  then:         scripts/fleet.sh verify    # joined is not the same as working
  look at it:   scripts/fleet.sh status
  take it down: scripts/fleet.sh down
EOF
}

# --- seed ------------------------------------------------------------

# A node with no template rootfs cannot start a sandbox: the master
# records the template, but the tree has to be on the node's disk. So a
# freshly joined fleet is not loadable until something puts one there.
#
# This is the smallest one that runs: /usr /lib /lib64 /bin /sbin arrive
# as read-only binds, so only the mount points, a passwd line and the app
# itself are written. The app sleeps -- a load test is measuring the
# sandbox lifecycle, not what runs inside it. Same shape as the one
# scripts/dev/two-nodes.sh uses.
cmd_seed() {
  local NAME=py MEM=128
  while [ $# -gt 0 ]; do
    case "$1" in
      --name) NAME="$2"; shift 2;;
      --memory-mib) MEM="$2"; shift 2;;
      *) usage;;
    esac
  done
  [ -f "$STATE" ] || die "no fleet recorded in $STATE"
  # shellcheck disable=SC1090
  . "$STATE"
  MASTER_HOST="$MASTER"; MASTER_PORT="$PORT"; MASTER_BIND="${BIND_ADDR:-127.0.0.1}"

  say "installing template '$NAME' on each node"
  for h in $(printf '%s\n' ${NODES//,/ } | sort -u); do
    sshq "$h" "sudo bash -s" <<EOF || { warn "\$h: could not write the rootfs"; continue; }
set -eu
TPL=/var/lib/sandstorm/templates/$NAME/rootfs
mkdir -p "\$TPL"/{proc,dev,sys,tmp,etc,var,root,app,usr,lib,lib64,bin,sbin}
printf 'root:x:0:0:root:/root:/bin/sh\n' > "\$TPL/etc/passwd"
cat > "\$TPL/app/s.py" <<'APP'
import time
while True:
    time.sleep(3600)
APP
EOF
    ok "$h"
  done

  say "registering it on the master"
  local body
  body="{\"name\":\"$NAME\",\"rootfs\":\"rootfs\",\"spec\":{\"args\":[\"/usr/bin/python3\",\"/app/s.py\"],\"memory_max_mib\":$MEM}}"
  if mapi POST /v1/templates "$body" >/dev/null 2>&1; then
    ok "template '$NAME' registered"
    say "now:  python3 bench/fleet/loadtest.py --master https://$MASTER_ADDR:$PORT --ca $CA \\"
    echo "        --token-file <(printf %s \"\$(sed -n s/^TOKEN=//p $STATE)\") --template $NAME --total 50"
  else
    die "the master refused the template"
  fi
}

# --- verify ----------------------------------------------------------

# Create one sandbox and check it actually came up.
#
# A create returns 201 the moment the master has written desired state;
# the node does the work afterwards. So "the node joined" and "the node
# can run something" are different claims, and only the first one was
# ever checked before a fleet got handed to a load test. This checks the
# second, on one sandbox, and says what the node said if it failed.
cmd_verify() {
  local NAME=py
  while [ $# -gt 0 ]; do
    case "$1" in --name) NAME="$2"; shift 2;; *) usage;; esac
  done
  [ -f "$STATE" ] || die "no fleet recorded in $STATE"
  # shellcheck disable=SC1090
  . "$STATE"
  MASTER_HOST="$MASTER"; MASTER_PORT="$PORT"; MASTER_BIND="${BIND_ADDR:-127.0.0.1}"

  say "creating one '$NAME' sandbox and waiting for the node to start it"
  local made id
  made=$(mapi POST /v1/sandboxes "{\"template\":\"$NAME\",\"pool\":\"$POOL\"}")
  id=$(printf '%s' "$made" | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["id"])
except Exception: pass' 2>/dev/null)
  [ -n "$id" ] || die "the master would not create one: $(printf '%s' "$made" | head -c 200)"

  local i state
  for i in $(seq 1 30); do
    sleep 2
    state=$(mapi GET "/v1/sandboxes/$id" | python3 -c 'import json,sys
s = json.load(sys.stdin)
if s.get("ip"): print("up|" + s["ip"])
elif s.get("last_error"): print("failed|" + s["last_error"].split(chr(10))[0])
else: print("pending|")' 2>/dev/null)
    case "$state" in up\|*|failed\|*) break;; esac
  done

  mapi DELETE "/v1/sandboxes/$id" >/dev/null 2>&1
  case "$state" in
    up\|*)
      ok "a sandbox ran: ${state#up|}"
      say "the fleet can actually do work"
      ;;
    failed\|*)
      warn "the node accepted it and then could not start it:"
      warn "  ${state#failed|}"
      warn "Logs for the attempt are on the node under /var/lib/sandstorm/logs."
      die "this fleet is NOT ready to load: nothing it accepts will run"
      ;;
    *)
      warn "still pending after 60s -- the node may be slow, or stuck"
      die "could not confirm the fleet runs anything"
      ;;
  esac
}

# --- status ----------------------------------------------------------

cmd_status() {
  [ -f "$STATE" ] || die "no fleet recorded in $STATE"
  # shellcheck disable=SC1090
  . "$STATE"
  local M="https://$MASTER_ADDR:$PORT"
  MASTER_HOST="$MASTER"; MASTER_PORT="$PORT"; MASTER_BIND="${BIND_ADDR:-127.0.0.1}"
  say "master $M"
  mapi GET /v1/nodes | python3 -c "$PRETTY_NODES"
  local boxes; boxes=$(mapi GET /v1/sandboxes \
    | python3 -c 'import json,sys;from collections import Counter;b=json.load(sys.stdin);print(len(b), dict(Counter(x["tier"] for x in b)))' 2>/dev/null)
  echo "  sandboxes $boxes"
  say "per host"
  for h in $(printf '%s\n' "$MASTER" ${NODES//,/ } | sort -u); do
    printf '  %-14s %s\n' "$h" "$(sshq "$h" '
      p=$(sudo ls -l /proc/*/exe 2>/dev/null | grep -c sandstormd)
      r=$(sudo iptables -S 2>/dev/null | grep -c SANDSTORM-COLD)
      n=$(ip netns list 2>/dev/null | grep -c "^sandstorm-")
      echo "sandstorm procs=$p  SANDSTORM-COLD rules=$r  netns=$n"' 2>/dev/null)"
  done
}

# --- down ------------------------------------------------------------

cmd_down() {
  local KEEP_STATE=0
  while [ $# -gt 0 ]; do
    case "$1" in --keep-state) KEEP_STATE=1; shift;; *) usage;; esac
  done
  [ -f "$STATE" ] || die "no fleet recorded in $STATE"
  # shellcheck disable=SC1090
  . "$STATE"
  MASTER_HOST="$MASTER"; MASTER_PORT="$PORT"; MASTER_BIND="${BIND_ADDR:-127.0.0.1}"

  say "1. deleting sandboxes through the API, so the nodes tear them down"
  local ids
  ids=$(mapi GET /v1/sandboxes \
    | python3 -c 'import json,sys
try: print("\n".join(s["id"] for s in json.load(sys.stdin)))
except Exception: pass' 2>/dev/null)
  local n=0
  for id in $ids; do
    mapi DELETE "/v1/sandboxes/$id" >/dev/null 2>&1 && n=$((n+1))
  done
  ok "$n deleted"
  [ "$n" -gt 0 ] && { say "   waiting for the nodes to act on it"; sleep 8; }

  say "2. stopping units"
  for h in ${NODES//,/ }; do
    sshq "$h" 'sudo systemctl disable --now sandstorm-node >/dev/null 2>&1; true'
    ok "$h  sandstorm-node stopped"
  done
  sshq "$MASTER" 'sudo systemctl disable --now sandstorm-master >/dev/null 2>&1; true'
  ok "$MASTER  sandstorm-master stopped"

  say "3. removing what the nodes put on the host"
  local SUBNET="$NET_BASE/$NET_PREFIX"
  for h in ${NODES//,/ }; do
    sshq "$h" "sudo bash -s" <<EOF
set -u
# Namespaces and their veths. Deleting the netns takes the peer with it.
for ns in \$(ip netns list 2>/dev/null | awk '{print \$1}' | grep '^sandstorm-' || true); do
  ip netns del "\$ns" 2>/dev/null
done
for l in \$(ip -o link show 2>/dev/null | awk -F': ' '{print \$2}' | cut -d@ -f1 | grep '^sst' || true); do
  ip link del "\$l" 2>/dev/null
done
# iptables, by replaying what iptables itself prints. Never by writing
# the match syntax out again: the node installs
#   -m conntrack --ctstate RELATED,ESTABLISHED
# and a hand-written -m state --state ESTABLISHED,RELATED does not
# delete it, which is how a rule survived a cleanup once before.
iptables -S 2>/dev/null | grep -E -- '(SANDSTORM-COLD|SANDSTORM-EGRESS|$SUBNET)' | while read -r line; do
  case "\$line" in
    -A*) iptables \$(printf '%s' "\$line" | sed 's/^-A/-D/') 2>/dev/null ;;
  esac
done
iptables -t nat -S 2>/dev/null | grep -E -- '$SUBNET' | while read -r line; do
  case "\$line" in
    -A*) iptables -t nat \$(printf '%s' "\$line" | sed 's/^-A/-D/') 2>/dev/null ;;
  esac
done
for chain in SANDSTORM-COLD SANDSTORM-EGRESS SANDSTORM-PUBLIC; do
  iptables -F \$chain 2>/dev/null
  iptables -X \$chain 2>/dev/null
done
rm -rf /var/run/sandstorm-runsc
EOF
    ok "$h  namespaces, veths and rules removed"
  done

  say "4. removing files and units"
  for h in $(printf '%s\n' "$MASTER" ${NODES//,/ } | sort -u); do
    sshq "$h" "sudo bash -s" <<EOF
set -u
rm -f /etc/systemd/system/sandstorm-node.service /etc/systemd/system/sandstorm-master.service
rm -rf /etc/systemd/system/sandstorm-node.service.d
rm -f /usr/local/bin/sandstormd
$([ "$KEEP_STATE" = 1 ] || echo 'rm -rf /var/lib/sandstorm')
systemctl daemon-reload
EOF
    ok "$h  cleaned"
  done

  say "5. did every machine go back to where it was?"
  local snaps="$STATE_DIR/before" clean=1
  for h in $(printf '%s\n' "$MASTER" ${NODES//,/ } | sort -u); do
    local after="$STATE_DIR/after-$h.txt"
    snapshot_host "$h" "$after"
    local b_ipt a_ipt b_ns a_ns a_proc
    b_ipt=$(getval "$snaps/$h.txt" iptables_lines); a_ipt=$(getval "$after" iptables_lines)
    b_ns=$(getval "$snaps/$h.txt" netns);           a_ns=$(getval "$after" netns)
    a_proc=$(getval "$after" sandstorm_procs)
    # What matters is whether anything of OURS is left. The raw rule count
    # drifts on its own on a host with a live firewall -- bm runs one that
    # adds an SSH blocklist entry whenever someone knocks -- so that count
    # is reported but does not decide.
    local a_fr a_fn
    a_fr=$(getval "$after" sandstorm_rules); a_fn=$(getval "$after" sandstorm_netns)
    local line="$h  sandstorm: rules ${a_fr:-?}, netns ${a_fn:-?}, procs ${a_proc:-?}"
    if [ "${a_fr:-1}" = 0 ] && [ "${a_fn:-1}" = 0 ] && [ "${a_proc:-1}" = 0 ]; then
      ok "$line"
      [ "$b_ipt" = "$a_ipt" ] || echo "        note: the host's iptables went $b_ipt -> $a_ipt lines, none of them ours"
      [ "$b_ns" = "$a_ns" ] || echo "        note: netns count went $b_ns -> $a_ns, none of them ours"
    else
      warn "$line"; warn "  something of ours is still on $h"; clean=0
    fi
    # Services that were running before and are not now. A note, not a
    # failure: plenty of units idle out on their own between the two
    # snapshots -- fwupd did on the first run -- and this cannot tell
    # those apart from one the deploy disturbed. Worth printing, not
    # worth calling the machine dirty over.
    local gone
    gone=$(comm -23 <(grep '^svc=' "$snaps/$h.txt" | sort) <(grep '^svc=' "$after" | sort) \
           | sed 's/^svc=//' | grep -v '^sandstorm-' | tr '\n' ' ')
    [ -z "$gone" ] || echo "        note: not running now, was before (may have idled out): $gone"
  done
  if [ "$clean" = 1 ]; then
    ok "every machine matches its before-snapshot"
    rm -f "$STATE"
    say "fleet record removed; $STATE_DIR still holds the logs and snapshots"
  else
    warn "differences above -- $STATE kept so you can re-run 'down'"
  fi
}

case "${1:-}" in
  up) shift; cmd_up "$@";;
  add) shift; cmd_add "$@";;
  seed) shift; cmd_seed "$@";;
  verify) shift; cmd_verify "$@";;
  status) shift; cmd_status "$@";;
  down) shift; cmd_down "$@";;
  *) usage;;
esac
