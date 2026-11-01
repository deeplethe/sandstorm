#!/usr/bin/env bash
# Build an E2B template for Sandstorm and register it with a master.
#
#   NAME=base bash recipes/e2b-codeinterpreter/build.sh            # E2B's `base`
#   NAME=code-interpreter-v1 BASE=e2bdev/code-interpreter:latest \
#     bash recipes/e2b-codeinterpreter/build.sh                    # the code interpreter
#
# Three steps, each skippable, because the machine with Docker and the node
# that runs sandboxes are often not the same one (Sandstorm's own test nodes
# have no registry access):
#
#   1. build   docker buildx -> $OUT/rootfs.tar          (needs Docker)
#   2. unpack  $OUT/rootfs.tar -> $WORK_DIR/templates/$NAME/rootfs
#              on every node of the pool                 (needs root)
#   3. register POST /v1/templates on the master         (needs the API token)
#
# STEPS selects them, default "build unpack register".
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NAME="${NAME:-base}"
BASE="${BASE:-ubuntu:22.04}"
PLATFORM="${PLATFORM:-linux/amd64}"
OUT="${OUT:-$HERE/out/$NAME}"
WORK_DIR="${WORK_DIR:-/var/lib/sandstorm}"
MASTER="${MASTER:-https://127.0.0.1:8080}"
PKI_DIR="${PKI_DIR:-/var/lib/sandstorm/pki}"
TOKEN="${TOKEN:-$(cat "$PKI_DIR/api-token" 2>/dev/null || true)}"
MEMORY_MIB="${MEMORY_MIB:-512}"
STEPS="${STEPS:-build unpack register}"
# The template's own program, run after envd starts, as a JSON array of
# strings. Empty: envd is the sandbox's only service, as in E2B's `base`.
CMD_JSON="${CMD_JSON:-[]}"

has() { case " $STEPS " in *" $1 "*) return 0;; esac; return 1; }

if has build; then
    mkdir -p "$OUT"
    docker buildx build --platform "$PLATFORM" --build-arg BASE="$BASE" \
        --output "type=tar,dest=$OUT/rootfs.tar" "$HERE"
    echo "rootfs: $OUT/rootfs.tar ($(du -h "$OUT/rootfs.tar" | cut -f1))"
fi

if has unpack; then
    [ "$(id -u)" -eq 0 ] || { echo "unpack needs root" >&2; exit 1; }
    dest="$WORK_DIR/templates/$NAME/rootfs"
    rm -rf "$dest.new" && mkdir -p "$dest.new"
    tar -C "$dest.new" -xf "$OUT/rootfs.tar"
    # The sandbox mounts its own /proc, /dev, /sys and /tmp over these.
    mkdir -p "$dest.new"/{proc,dev,sys,tmp}
    rm -rf "$dest.old"; [ -d "$dest" ] && mv "$dest" "$dest.old"
    mv "$dest.new" "$dest" && rm -rf "$dest.old"
    echo "unpacked into $dest"
fi

if has register; then
    [ -n "$TOKEN" ] || { echo "no API token: set TOKEN or PKI_DIR" >&2; exit 1; }
    # ro_binds is empty on purpose: the default binds the host's /usr, /lib
    # and /bin over the template's own, which is right for a skeleton rootfs
    # and wrong for a whole distribution.
    spec=$(python3 - "$CMD_JSON" "$MEMORY_MIB" <<'PY'
import json, sys
cmd = json.loads(sys.argv[1])
print(json.dumps({
    "args": ["/usr/bin/tini", "-s", "--", "/usr/local/bin/sandstorm-e2b-entrypoint", *cmd],
    "env": ["LANG=C.UTF-8"],
    "cwd": "/home/user",
    "ro_binds": [],
    "memory_max_mib": int(sys.argv[2]),
}))
PY
)
    curl -fsS --cacert "$PKI_DIR/ca.pem" -H "authorization: Bearer $TOKEN" \
        -H 'content-type: application/json' \
        -d "{\"name\":\"$NAME\",\"rootfs\":\"rootfs\",\"spec\":$spec}" \
        "$MASTER/v1/templates"
    echo "registered template $NAME"
fi
