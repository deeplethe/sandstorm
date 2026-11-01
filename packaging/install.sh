#!/bin/sh
# Join this machine to a sandstorm master as a node.
#
# Served by the master at GET /install.sh with the two placeholders below
# filled in, so the copy you run already knows where it came from. The console
# (and `POST /v1/node-bootstrap`) hands out the one line that fetches it.
#
# What it does: decide the backend and check this machine can run it, fetch
# the master's own binary so the versions cannot drift, install it, write a
# node unit, start it. The node dials out, so this machine needs no inbound
# port.
#
# The backend (SANDSTORM_BACKEND):
#
#   gvisor       runsc, which runs on any Linux machine. The default.
#   firecracker  microVMs. Needs /dev/kvm, a guest kernel (SANDSTORM_FC_KERNEL,
#                an uncompressed vmlinux; scripts/install-guest-kernel.sh
#                fetches one), and a work dir (SANDSTORM_STATE_DIR) on XFS
#                with reflink=1 or on btrfs. The firecracker binary is taken
#                from SANDSTORM_FIRECRACKER or the PATH, or else the pinned
#                release is downloaded and its sha256 checked.
#   auto         firecracker where this machine has /dev/kvm, gvisor where it
#                has not. It says which it picked, and why.
#
# Every requirement is checked before anything is installed, and then once
# more by the node's own binary (`sandstormd preflight`), which makes the
# very checks the node makes when it starts. A machine that cannot run the
# backend is refused, saying what is missing; nothing is left half-installed.
#
# The choice is written to $SANDSTORM_STATE_DIR/backend rather than into the
# unit, so the console can later move an empty node to the other backend:
# the node rewrites that file and restarts (crates/sandstorm-core/src/node/
# switch.rs).
#
# ---------------------------------------------------------------------------
# On trust, because this runs as root and the honest answer is not "it is
# fine".
#
# The master's certificate is signed by its own CA, so plain curl refuses it
# and the one-liner cannot simply verify. What the registration token carries
# is the CA's SHA-256 (`<fingerprint>.<secret>`), and this script uses it:
# it fetches the CA, checks the fingerprint, and does every later request
# with --cacert. So the binary and the join are verified.
#
# The gap is this script itself. The first fetch cannot be verified by a
# fingerprint the script has not read yet, so a machine-in-the-middle could
# substitute THIS FILE and skip the checks below. Two ways to close it:
#
#   SANDSTORM_CA=/path/to/ca.pem   copy the master's pki/ca.pem over first and
#                              point curl at it from the start, or
#   verify by hand             fetch with --cacert and inspect before piping
#                              to a shell.
#
# Piping an unverified script to root is a real exposure on an untrusted
# network. It is the same shape as every `curl | sh` installer, except that
# here the token at least makes everything after the first hop verifiable.
# ---------------------------------------------------------------------------
set -eu

MASTER="${SANDSTORM_MASTER_URL:-@MASTER@}"
TOKEN="${SANDSTORM_NODE_TOKEN:-}"
BIN="${SANDSTORM_BIN:-/usr/local/bin/sandstormd}"
STATE="${SANDSTORM_STATE_DIR:-/var/lib/sandstorm}"
UNIT="${SANDSTORM_UNIT:-/etc/systemd/system/sandstorm-node.service}"
NODE_ID="${SANDSTORM_NODE_ID:-$(hostname)}"
# Set to have the master connect to this node, e.g. 0.0.0.0:80. Unset, the
# node dials the master instead -- the right choice behind NAT, where the
# master cannot reach in.
LISTEN="${SANDSTORM_LISTEN:-}"
BACKEND="${SANDSTORM_BACKEND:-gvisor}"
RUNSC="${SANDSTORM_RUNSC:-}"
FC_BIN="${SANDSTORM_FIRECRACKER:-}"
FC_KERNEL="${SANDSTORM_FC_KERNEL:-}"
# Only the tests set this: it is where to look for KVM, so they can stand a
# character device in for it.
KVM="${SANDSTORM_KVM_DEVICE:-/dev/kvm}"
# Downloaded only when neither of the above finds one.
FC_VERSION="v1.10.1"
FC_X86_64_SHA256="36112969952b0e34fadcfca769d48a55dc22cbba99af17e02bd0e24fc35adc77"
FC_AARCH64_SHA256="9e3641071de140979afaac0c52fdc107baeba398bdb5709c12f77ee469207fcd"

die() { echo "install: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

[ "$(id -u)" = 0 ] || die "run this as root (the node manages netns and cgroups)"
if [ -z "$LISTEN" ]; then
  [ -n "$TOKEN" ] || die "SANDSTORM_NODE_TOKEN is not set; mint one with \`sandstormd node-token\` or from the console -- or set SANDSTORM_LISTEN=0.0.0.0:80 to have the master connect to this node instead"
fi
have curl || die "curl is required"

# ---- the backend, decided and checked before anything is fetched ----------
case "$BACKEND" in
  runsc) BACKEND=gvisor ;;
  gvisor|firecracker) ;;
  auto)
    if [ -c "$KVM" ]; then
      BACKEND=firecracker
      echo "install: backend auto -> firecracker, because this machine has /dev/kvm (set SANDSTORM_BACKEND=gvisor to use gVisor instead)"
    else
      BACKEND=gvisor
      echo "install: backend auto -> gvisor, because this machine has no /dev/kvm"
    fi
    ;;
  *) die "SANDSTORM_BACKEND must be auto, gvisor or firecracker, not '$BACKEND'" ;;
esac

# What to say when Firecracker cannot run here, after the reason.
FC_ELSE="Fix that, or install this node on gVisor with SANDSTORM_BACKEND=gvisor"

if [ "$BACKEND" = firecracker ]; then
  [ -c "$KVM" ] || die "firecracker needs /dev/kvm and this machine has none (a VM without nested virtualisation, or kvm not loaded). $FC_ELSE"
  if [ -z "$FC_KERNEL" ]; then
    for k in "$STATE/vmlinux" /var/lib/sandstorm/vmlinux /var/lib/sandstorm/kernels/vmlinux; do
      [ -f "$k" ] && { FC_KERNEL="$k"; break; }
    done
  fi
  [ -n "$FC_KERNEL" ] && [ -f "$FC_KERNEL" ] \
    || die "firecracker needs a guest kernel (an uncompressed vmlinux) and there is none at ${FC_KERNEL:-$STATE/vmlinux or /var/lib/sandstorm/kernels/vmlinux}. Put one there with scripts/install-guest-kernel.sh, or point SANDSTORM_FC_KERNEL at one. $FC_ELSE"
  # Every sandbox's disk is a reflink of its template's: without one it
  # would be a full copy each time, which the node refuses to do.
  install -d -m 0750 "$STATE"
  probe="$STATE/.reflink-probe.$$"
  echo probe > "$probe"
  if ! cp --reflink=always "$probe" "$probe.copy" 2>/dev/null; then
    rm -f "$probe" "$probe.copy"
    fs="$(stat -f -c %T "$STATE" 2>/dev/null || echo unknown)"
    die "firecracker needs its work dir on XFS with reflink=1 or on btrfs, and $STATE is on $fs, which cannot reflink. Set SANDSTORM_STATE_DIR to a directory on such a filesystem. $FC_ELSE"
  fi
  rm -f "$probe" "$probe.copy"
  [ -n "$FC_BIN" ] || FC_BIN="$(command -v firecracker 2>/dev/null || true)"
  if [ -n "$FC_BIN" ]; then
    [ -x "$FC_BIN" ] || die "SANDSTORM_FIRECRACKER=$FC_BIN is not an executable"
  fi
  echo "install: firecracker: /dev/kvm, kernel $FC_KERNEL, $STATE reflinks"
else
  [ -n "$RUNSC" ] || RUNSC="$(command -v runsc 2>/dev/null || true)"
  [ -n "$RUNSC" ] && [ -x "$RUNSC" ] \
    || die "gvisor needs runsc and it is not installed (see https://gvisor.dev/docs/user_guide/install/), or point SANDSTORM_RUNSC at it"
fi

# The CA fingerprint the token pins, if it carries one.
PIN=""
case "$TOKEN" in
  *.*) PIN="${TOKEN%%.*}" ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

CA="${SANDSTORM_CA:-}"
if [ -n "$CA" ]; then
  echo "install: verifying the master with $CA"
elif [ -n "$PIN" ] && [ "${#PIN}" = 64 ]; then
  # Fetch the CA unverified, then refuse it unless it hashes to what the
  # token says. An attacker who swaps the CA fails this; one who swapped
  # this script does not, which is the gap documented above.
  curl -sSfL -k -o "$TMP/ca.pem" "$MASTER/ca.pem" \
    || die "could not fetch the CA from $MASTER"
  if have openssl; then
    got="$(openssl x509 -in "$TMP/ca.pem" -outform der 2>/dev/null | openssl dgst -sha256 -hex 2>/dev/null | sed 's/.*= *//')"
  else
    die "openssl is required to check the CA fingerprint (or pass SANDSTORM_CA)"
  fi
  [ "$got" = "$PIN" ] || die "the master's CA is $got but the token pins $PIN -- refusing"
  echo "install: CA fingerprint matches the token ($(echo "$PIN" | cut -c1-12)…)"
  CA="$TMP/ca.pem"
else
  die "this token carries no CA fingerprint; mint a new one, or pass SANDSTORM_CA=/path/to/ca.pem"
fi

echo "install: fetching the master's own binary, so the versions cannot drift"
curl -sSfL --cacert "$CA" -o "$TMP/sandstormd" "$MASTER/sandstormd" \
  || die "could not fetch the binary from $MASTER"
chmod +x "$TMP/sandstormd"
"$TMP/sandstormd" --version >/dev/null 2>&1 \
  || die "the downloaded binary does not run on this machine"

# Firecracker's own release when this machine has none: a static binary,
# checked against the pinned sha256 before it is used.
if [ "$BACKEND" = firecracker ] && [ -z "$FC_BIN" ]; then
  arch="$(uname -m)"
  case "$arch" in
    x86_64) sum="$FC_X86_64_SHA256" ;;
    aarch64) sum="$FC_AARCH64_SHA256" ;;
    *) die "no firecracker release for $arch; install one and set SANDSTORM_FIRECRACKER" ;;
  esac
  have sha256sum || die "sha256sum is required to check the firecracker download (or set SANDSTORM_FIRECRACKER)"
  echo "install: fetching firecracker $FC_VERSION ($arch)"
  tgz="$TMP/firecracker.tgz"
  curl -sSfL -o "$tgz" "https://github.com/firecracker-microvm/firecracker/releases/download/$FC_VERSION/firecracker-$FC_VERSION-$arch.tgz" \
    || die "could not download firecracker $FC_VERSION; install it and set SANDSTORM_FIRECRACKER=/path/to/firecracker"
  got="$(sha256sum "$tgz" | cut -d' ' -f1)"
  [ "$got" = "$sum" ] || die "the firecracker download hashes to $got, not the pinned $sum -- refusing"
  tar -xzf "$tgz" -C "$TMP"
  install -D -m 0755 "$TMP/release-$FC_VERSION-$arch/firecracker-$FC_VERSION-$arch" /usr/local/bin/firecracker
  FC_BIN=/usr/local/bin/firecracker
fi

# The node's own checks, from the binary about to be installed: the ones it
# makes when it starts on this backend. If it would refuse to start, refuse
# to install it.
if [ "$BACKEND" = firecracker ]; then
  CHECK="--backend firecracker --firecracker $FC_BIN --fc-kernel $FC_KERNEL"
else
  CHECK="--backend runsc --runsc $RUNSC"
fi
# shellcheck disable=SC2086 # word-split on purpose: CHECK is flags
"$TMP/sandstormd" preflight $CHECK --work-dir "$STATE" \
  || die "this machine cannot run $BACKEND (above). Nothing was installed"

install -D -m 0755 "$TMP/sandstormd" "$BIN"
install -d -m 0750 "$STATE"
echo "install: $BIN $("$BIN" --version)"

for t in iptables ip; do
  have "$t" || echo "install: WARNING $t is not installed; the node will refuse to start sandboxes" >&2
done

# The backend, where the node reads it from and the console can change it.
# Both engines' paths go in the unit, so a later switch finds them.
if [ "$BACKEND" = firecracker ]; then echo firecracker; else echo runsc; fi > "$STATE/backend"
[ -n "$RUNSC" ] || RUNSC="$(command -v runsc 2>/dev/null || echo runsc)"
[ -n "$FC_BIN" ] || FC_BIN="$(command -v firecracker 2>/dev/null || echo firecracker)"
[ -n "$FC_KERNEL" ] || FC_KERNEL="$STATE/vmlinux"
echo "install: backend $BACKEND, recorded in $STATE/backend"

if [ -n "$LISTEN" ]; then
  CONNECT="--listen $LISTEN"
  CONNECT_NOTE="It waits on $LISTEN for the master to connect."
else
  CONNECT="--master-url $MASTER"
  CONNECT_NOTE="It dials the master, so this machine needs no inbound port."
fi

cat > "$UNIT" <<UNITEOF
[Unit]
Description=sandstorm node
Documentation=https://github.com/deeplethe/sandstorm
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
# A node only. $CONNECT_NOTE
ExecStart=$BIN serve --node \\
  $CONNECT \\
  --node-id $NODE_ID \\
  --state-db $STATE/node.db \\
  --pki-dir $STATE/pki \\
  --work-dir $STATE \\
  --backend-file $STATE/backend \\
  --runsc $RUNSC \\
  --firecracker $FC_BIN \\
  --fc-kernel $FC_KERNEL
# on-failure covers a backend switch too: the node exits 75 once it has
# rewritten $STATE/backend, to be started again on the new engine.
Restart=on-failure
RestartSec=2s
TimeoutStopSec=15s
# A descriptor per sandbox, asleep or awake; see sandstormd.service.
LimitNOFILE=524288

# Same capability set and hardening as packaging/systemd/sandstormd.service;
# see the comments there for why each one is needed.
# The last two are not optional, however much they look like it, and
# neither failure names a capability.
#
#   CAP_DAC_OVERRIDE  the per-sandbox overlay mount returns EPERM, which
#                     util-linux reports as "cannot mount overlay
#                     read-only" -- a filesystem-sounding message for a
#                     permissions problem.
#   CAP_NET_BIND_SERVICE  for \`--listen\` on a port below 1024. The first
#                     fleet could only open 22 and 80 inbound, so the
#                     node listens on 80, and without this the bind fails
#                     with EACCES even though the process runs as root.
#   CAP_SETFCAP       runsc's gofer re-execs /proc/self/exe and fails
#                     with "operation not permitted". Found by binary
#                     search over the whole capability set on bm: the
#                     bounding set is what matters here, not the ambient
#                     one, because the gofer's exec is what needs it.
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN CAP_KILL CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_SETFCAP CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN CAP_KILL CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_SETFCAP CAP_NET_BIND_SERVICE
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
# /run as well: \`ip netns add\` creates its namespace file under
# /run/netns, and the runsc state root defaults to /run/sandstorm-runsc.
# ProtectSystem=strict makes everything outside /dev, /proc and /sys
# read-only, so without this a node registers, polls, reports healthy --
# and fails every sandbox with
#     creating netns: ["ip","netns","add",...] failed
# which is what it did on bm until this line was added.
ReadWritePaths=$STATE /sys/fs/cgroup /run
ProtectControlGroups=false
ProtectKernelTunables=true
ProtectKernelModules=true
LockPersonality=true
# AF_PACKET too: gVisor's network stack opens a raw packet socket on
# each sandbox interface, and without it \`runsc start\` fails with
#     unable to create raw socket: address family not supported by protocol
# which sounds like a kernel that is missing something and is this line.
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK AF_PACKET
# ipc and uts as well. runsc's gofer clones with CLONE_NEWIPC and
# CLONE_NEWUTS together, and this filter rejects the whole clone if
# either is missing -- adding only one of the two does not help. The
# failure is \`gofer: fork/exec /proc/self/exe: operation not permitted\`,
# which names neither namespaces nor systemd. Omitting them was a slip
# in writing the list, not a policy: a node needs every namespace type
# except time.
RestrictNamespaces=net mnt user pid cgroup ipc uts
RestrictRealtime=true
SystemCallArchitectures=native
# No SystemCallFilter, and that is deliberate.
#
# gVisor IS a syscall implementation: the sentry and its gofer use a set
# well outside @system-service, and it moves between runsc releases. A
# name allowlist here is a list that works until it doesn't, and the way
# it fails is a process killed with SIGSYS and an empty stderr -- the
# node reported \`creating netns: [...] failed:\` with nothing after the
# colon, because mount(2) was being killed rather than refused.
#
# Measured on bm by building this unit up one line at a time: every
# other setting below passes with a sandbox actually running, and
# \`SystemCallFilter=@system-service @mount\` fails, with or without
# @resources added back. Something gVisor needs is simply not in those
# sets, and guessing at the rest would ship a unit that breaks on the
# next runsc.
#
# What is left is not nothing: CapabilityBoundingSet, RestrictNamespaces,
# RestrictAddressFamilies, ProtectSystem=strict and the Protect* options
# below install fifteen seccomp filters between them. The allowlist is
# the one piece a sandbox runtime cannot live under.

[Install]
WantedBy=multi-user.target
UNITEOF

# The token is single-use and short-lived, so it goes in a drop-in at 0600
# rather than into the unit, which is world-readable. A listening node has
# no token: the master brings the pairing code to it.
if [ -n "$TOKEN" ]; then
  mkdir -p "$UNIT.d"
  printf '[Service]\nEnvironment=SANDSTORM_NODE_TOKEN=%s\n' "$TOKEN" > "$UNIT.d/token.conf"
  chmod 0600 "$UNIT.d/token.conf"
fi

systemctl daemon-reload
systemctl enable --now "$(basename "$UNIT")"
if [ -n "$LISTEN" ]; then
  CODE="$STATE/pki/node/pairing-code"
  for _ in $(seq 1 50); do [ -s "$CODE" ] && break; sleep 0.2; done
  [ -s "$CODE" ] || die "the node did not write $CODE; see journalctl -u $(basename "$UNIT")"
  echo "install: listening on $LISTEN. Give the master this address and the code in"
  echo "install:   $CODE   (root only; it does not change across restarts)"
else
  echo "install: started $(basename "$UNIT"); it should appear in the console within a few seconds"
fi
echo "install: follow it with  journalctl -u $(basename "$UNIT") -f"
