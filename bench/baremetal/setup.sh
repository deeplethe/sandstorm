#!/usr/bin/env bash
# Puts what compare.py measures on one machine with KVM (Ubuntu 24.04). Run as
# root from a checkout, after:
#   - apt: python3-numpy docker.io bubblewrap podman xfsprogs debootstrap
#   - runsc, firecracker and a Firecracker guest vmlinux on the PATH and at
#     /var/lib/sandstorm/vmlinux; msb and libkrunfw for microsandbox
#   - `cargo build --release -p sandstorm-core`
#   - /root/rootfs: `debootstrap --variant=minbase
#       --include=python3,python3-numpy,ca-certificates,iproute2,procps noble`
#     with bench/fleet/apps copied to /root/rootfs/app
#
# Two Sandstorm nodes, one per backend, each a master and node in one
# process: runsc on 127.0.0.1:18080 in /var/lib/sst-gvisor, Firecracker on
# :18081 in /var/lib/sst-fc (XFS, which it requires). Both with templates
# web and app64, plain and warm, at the memory limits the fleet runs used.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
BIN="$REPO/target/release/sandstormd"
ROOTFS=/root/rootfs
APPS="web:128 app64:224"
# Extra flags for both nodes. To keep every sandbox hot, as the other runtimes
# keep their instances, for a memory comparison:
#   SANDSTORM_FLAGS="--idle-frozen-secs 86400 --idle-snapshot-secs 86400
#     --idle-detached-secs 86400 --reclaim-hot off --reclaim-frozen off"
SANDSTORM_FLAGS=${SANDSTORM_FLAGS:-}

# The guest's hostname has to resolve without DNS: Python's HTTPServer looks
# it up before it listens, and no sandbox here reaches a resolver, so it
# waited out the timeout -- 20 s per sandbox on bm. runsc writes /etc/hosts
# itself; a VM's init has to.
HOSTS_FIX='echo sandbox > /proc/sys/kernel/hostname
printf "127.0.0.1 localhost\n127.0.1.1 sandbox\n" > /etc/hosts'

serve() { # name port workdir extra-args...
  local name=$1 port=$2 dir=$3; shift 3
  systemctl stop "$name" 2>/dev/null || true
  systemd-run --unit "$name" --collect -p KillMode=process "$BIN" serve --master --node \
    --bind "127.0.0.1:$port" --state-db "$dir/state.db" --pki-dir "$dir/pki" --work-dir "$dir" \
    --node-id "$name" "$@" >/dev/null
  for _ in $(seq 1 60); do [ -s "$dir/pki/api-token" ] && curl -s -o /dev/null --cacert "$dir/pki/ca.pem" "https://127.0.0.1:$port/" && break; sleep 1; done
}

register() { # port dir name args mem warm
  local body="{\"name\":\"$3\",\"rootfs\":\"rootfs\",\"spec\":{\"args\":$4,\"memory_max_mib\":$5$6}}"
  curl -s -o /dev/null -w "$3 %{http_code}\n" --cacert "$2/pki/ca.pem" \
    -H "authorization: Bearer $(cat "$2/pki/api-token")" -H 'content-type: application/json' \
    -X POST --data-raw "$body" "https://127.0.0.1:$1/v1/templates"
}

args_of() {
  case $1 in
    web) echo '["/usr/bin/python3","/app/web.py"]' ;;
    app*) echo "[\"/usr/bin/python3\",\"/app/srv.py\",\"${1#app}\"]" ;;
  esac
}

# --- Sandstorm on runsc: the fleet's templates, the host's /usr bound in -------
G=/var/lib/sst-gvisor
mkdir -p "$G"
for app in $APPS; do
  name=${app%%:*}
  for t in "$name" "$name-warm"; do
    r=$G/templates/$t/rootfs
    mkdir -p $r/proc $r/dev $r/sys $r/tmp $r/etc/alternatives $r/var $r/root $r/app $r/usr $r/lib $r/lib64 $r/bin $r/sbin
    printf 'root:x:0:0:root:/root:/bin/sh\n' > $r/etc/passwd
    cp "$ROOTFS"/app/*.py $r/app/
    cp -a /etc/alternatives/lib*.so* $r/etc/alternatives/ 2>/dev/null || true
  done
done
# Two nodes on one machine allocate gateway ports from the same default range,
# so each gets its own: on the same port the second one's sandbox never binds.
serve sst-gvisor 18080 "$G" --port-min 21000 --port-max 25999 $SANDSTORM_FLAGS
for app in $APPS; do
  name=${app%%:*} mem=${app##*:}
  register 18080 "$G" "$name" "$(args_of "$name")" "$mem" ""
  register 18080 "$G" "$name-warm" "$(args_of "$name")" "$mem" ',"warm":{}'
done

# --- Sandstorm on Firecracker: the same rootfs as an ext4 image ------------------
F=/var/lib/sst-fc
if ! mountpoint -q "$F"; then
  mkdir -p "$F"
  [ -f /var/lib/sst-fc.xfs ] || { fallocate -l 80G /var/lib/sst-fc.xfs; mkfs.xfs -q -m reflink=1 /var/lib/sst-fc.xfs; }
  mount -o loop /var/lib/sst-fc.xfs "$F"
fi
img=$(mktemp -d)
cp -a "$ROOTFS"/. "$img"/
install -m755 "$REPO/rootfs-init/sandstorm-init.sh" "$img/sandstorm-init.sh"
install -m644 "$REPO/rootfs-init/sandstorm-agent.py" "$img/sandstorm-agent.py"
python3 - "$img/sandstorm-init.sh" "$HOSTS_FIX" <<'PY'
import sys
p, fix = sys.argv[1], sys.argv[2]
s = open(p).read()
if "/etc/hosts" not in s:
    s = s.replace('echo "sandstorm-init: launching agent..."', fix + '\necho "sandstorm-init: launching agent..."')
open(p, "w").write(s)
PY
rm -f /root/tpl.ext4; truncate -s 2G /root/tpl.ext4; mkfs.ext4 -q -d "$img" /root/tpl.ext4; rm -rf "$img"
for app in $APPS; do
  name=${app%%:*}
  for t in "$name" "$name-warm"; do mkdir -p "$F/templates/$t"; cp --sparse=always /root/tpl.ext4 "$F/templates/$t/rootfs.ext4"; done
done
# Warm images hold the init they booted with; a new one means new images.
rm -rf "$F/warm" "$G/warm"
serve sst-fc 18081 "$F" --backend firecracker --fc-kernel /var/lib/sandstorm/vmlinux --port-min 26000 --port-max 31999 $SANDSTORM_FLAGS
for app in $APPS; do
  name=${app%%:*} mem=${app##*:}
  register 18081 "$F" "$name" "$(args_of "$name")" "$mem" ""
  register 18081 "$F" "$name-warm" "$(args_of "$name")" "$mem" ',"warm":{}'
done

# --- microsandbox: an image per program from the same rootfs ---------------------
if command -v msb >/dev/null; then
  systemctl start docker
  for app in $APPS; do
    name=${app%%:*}
    cmd=$(args_of "$name" | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin)))')
    tar -C "$ROOTFS" -c . | docker import \
      --change "CMD [\"/bin/sh\",\"-c\",\"echo 127.0.1.1 \$(hostname) >> /etc/hosts; exec $cmd\"]" - "sst-$name:latest" >/dev/null
    docker save "sst-$name:latest" -o "/root/sst-$name.tar"
    msb load -i "/root/sst-$name.tar" >/dev/null 2>&1 || msb load "/root/sst-$name.tar"
  done
  systemctl stop docker.socket docker.service containerd.service
fi
echo setup done
