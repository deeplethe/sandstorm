#!/usr/bin/env bash
# Put the benchmark's apps on a fleet that scripts/fleet.sh stood up.
#
#   bench/fleet/apps.sh
#
# Writes each app's rootfs on every node and registers it on the master:
#
#   idle    a sleeping Python with nothing to answer (loadtest.py's runs)
#   web     a Python HTTP server and nothing else (apps/web.py)
#   appN    Python with numpy and an N MiB heap that every request reads
#           (apps/srv.py), for N = 16, 64, 256, 512, 1024
#
# /usr /lib /lib64 /bin /sbin arrive as read-only binds, so a rootfs is
# mount points, a passwd line, the app -- and /etc/alternatives, because
# numpy finds its BLAS through those links and fails to import without
# them. The memory limits are the ones the 2026-10-26 runs used.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
STATE="${SANDSTORM_FLEET_STATE:-$HOME/.sandstorm-fleet}/fleet.env"
[ -f "$STATE" ] || { echo "no fleet recorded in $STATE; run scripts/fleet.sh up first" >&2; exit 1; }
# shellcheck disable=SC1090
. "$STATE"
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=20)

APPS="idle:128 web:128 app16:160 app64:224 app256:448 app512:704 app1024:1216"

for h in $(printf '%s\n' ${NODES//,/ } | sort -u); do
  scp -q "$HERE/apps/srv.py" "$HERE/apps/web.py" "$h:/tmp/"
  "${SSH[@]}" "$h" "sudo bash -s" <<SH &
set -eu
for app in ${APPS}; do
  name=\${app%%:*}
  t=/var/lib/sandstorm/templates/\$name/rootfs
  mkdir -p \$t/proc \$t/dev \$t/sys \$t/tmp \$t/etc/alternatives \$t/var \$t/root \$t/app \$t/usr \$t/lib \$t/lib64 \$t/bin \$t/sbin
  printf 'root:x:0:0:root:/root:/bin/sh\n' > \$t/etc/passwd
  printf 'import time\nwhile True:\n    time.sleep(3600)\n' > \$t/app/s.py
  cp /tmp/srv.py /tmp/web.py \$t/app/
  cp -a /etc/alternatives/lib*.so* \$t/etc/alternatives/ 2>/dev/null || true
done
python3 -c 'import numpy' 2>/dev/null || echo "$h: the host has no numpy, so appN cannot start" >&2
echo "  $h"
SH
done
wait

register() {
  local body="{\"name\":\"$1\",\"rootfs\":\"rootfs\",\"spec\":{\"args\":$2,\"memory_max_mib\":$3}}"
  "${SSH[@]}" "$MASTER" "sudo curl -sS -o /dev/null -w '%{http_code}' --cacert /var/lib/sandstorm/pki/ca.pem \
    -H \"authorization: Bearer \$(sudo cat /var/lib/sandstorm/pki/api-token)\" -H 'content-type: application/json' \
    -X POST --data-raw '$body' 'https://${BIND_ADDR:-127.0.0.1}:$PORT/v1/templates'"
}
for app in $APPS; do
  name=${app%%:*} mem=${app##*:}
  case $name in
    idle) args='["/usr/bin/python3","/app/s.py"]' ;;
    web) args='["/usr/bin/python3","/app/web.py"]' ;;
    app*) args="[\"/usr/bin/python3\",\"/app/srv.py\",\"${name#app}\"]" ;;
  esac
  echo "  $name: $(register "$name" "$args" "$mem")"
done
