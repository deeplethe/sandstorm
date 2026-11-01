#!/usr/bin/env bash
# The gVisor half of setup.sh, for a machine without KVM: the fleet's 2-core,
# 2 GB nodes. One master and node in one process on 127.0.0.1:18080 in
# /var/lib/sst-gvisor, with templates web and app64, plain and warm. Run as
# root from a checkout with runsc on the PATH and python3-numpy installed:
#
#   BIN=/usr/local/bin/sandstormd-cmp bench/baremetal/setup-gvisor.sh
#
# then compare.py with --runtimes process,bwrap,runc,podman,runsc,
# sandstorm-gvisor,sandstorm-gvisor-warm.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
BIN=${BIN:-$REPO/target/release/sandstormd}
APPS="web:128 app64:224"
# Every sandbox kept hot, as the other runtimes keep their instances.
SANDSTORM_FLAGS=${SANDSTORM_FLAGS:---idle-frozen-secs 86400 --reclaim-hot off --reclaim-frozen off}

G=/var/lib/sst-gvisor
mkdir -p "$G"
for app in $APPS; do
  name=${app%%:*}
  for t in "$name" "$name-warm"; do
    r=$G/templates/$t/rootfs
    mkdir -p $r/proc $r/dev $r/sys $r/tmp $r/etc/alternatives $r/var $r/root $r/app $r/usr $r/lib $r/lib64 $r/bin $r/sbin
    printf 'root:x:0:0:root:/root:/bin/sh\n' > $r/etc/passwd
    cp "$REPO"/bench/fleet/apps/*.py $r/app/
    cp -a /etc/alternatives/lib*.so* $r/etc/alternatives/ 2>/dev/null || true
  done
done
rm -rf "$G/warm"

systemctl stop sst-gvisor 2>/dev/null || true
systemd-run --unit sst-gvisor --collect -p KillMode=process "$BIN" serve --master --node \
  --bind 127.0.0.1:18080 --state-db "$G/state.db" --pki-dir "$G/pki" --work-dir "$G" \
  --node-id sst-gvisor --port-min 21000 --port-max 25999 $SANDSTORM_FLAGS >/dev/null
for _ in $(seq 1 60); do [ -s "$G/pki/api-token" ] && curl -s -o /dev/null --cacert "$G/pki/ca.pem" https://127.0.0.1:18080/ && break; sleep 1; done

for app in $APPS; do
  name=${app%%:*} mem=${app##*:}
  case $name in
    web) args='["/usr/bin/python3","/app/web.py"]' ;;
    app*) args="[\"/usr/bin/python3\",\"/app/srv.py\",\"${name#app}\"]" ;;
  esac
  for t in "$name" "$name-warm"; do
    warm=; [ "$t" = "$name" ] || warm=',"warm":{}'
    curl -s -o /dev/null -w "$t %{http_code}\n" --cacert "$G/pki/ca.pem" \
      -H "authorization: Bearer $(cat "$G/pki/api-token")" -H 'content-type: application/json' \
      -X POST --data-raw "{\"name\":\"$t\",\"rootfs\":\"rootfs\",\"spec\":{\"args\":$args,\"memory_max_mib\":$mem$warm}}" \
      https://127.0.0.1:18080/v1/templates
  done
done
