set -eu
for NAME in idle app16 app64 app256; do
  TPL=/var/lib/sandstorm/templates/$NAME/rootfs
  mkdir -p "$TPL"/proc "$TPL"/dev "$TPL"/sys "$TPL"/tmp "$TPL"/etc "$TPL"/var "$TPL"/root "$TPL"/app "$TPL"/usr "$TPL"/lib "$TPL"/lib64 "$TPL"/bin "$TPL"/sbin
  printf 'root:x:0:0:root:/root:/bin/sh\n' > "$TPL/etc/passwd"
  printf 'import time\nwhile True:\n    time.sleep(3600)\n' > "$TPL/app/s.py"
  cp /tmp/srv.py "$TPL/app/srv.py"
done
echo ok
