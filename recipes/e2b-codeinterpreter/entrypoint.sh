#!/bin/sh
# Start envd, then the template's own program if it has one.
#
# envd listens on 49983, which the E2B SDKs hard-code. -isnotfc because this
# is gVisor, not Firecracker: there is no MMDS to poll and no clock to set.
# -no-cgroups because envd's per-process cgroups assume a writable
# /sys/fs/cgroup, and the sandbox's own cgroup is the node's to manage.
set -eu
/usr/bin/envd -port 49983 -isnotfc -no-cgroups ${ENVD_EXTRA_ARGS:-} &
ENVD=$!
if [ "$#" -gt 0 ]; then
    exec "$@"
fi
wait "$ENVD"
