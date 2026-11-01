#!/bin/bash
# Install a sandstorm-compatible vmlinux. Two paths:
#
#   1. Download — fetch Firecracker's CI-blessed vmlinux-6.1.141 from
#      AWS S3. ~40 MiB, takes seconds. Recommended for almost everyone.
#
#   2. Build from source — clone Linux 6.1 LTS, apply FC's reference
#      microvm config, build vmlinux. Takes ~10 min on a 20-core box.
#      Use this if you want to audit / tune the kernel config, or your
#      target architecture isn't x86_64.
#
# This is the recipe behind /var/lib/sandstorm/kernels/vmlinux as of
# sandstorm v0.5.1. The shipped pre-v0.5.1 kernel was Linux 4.14.174 from
# 2021 — predated CONFIG_HW_RANDOM_VIRTIO, CONFIG_RANDOM_TRUST_CPU,
# CONFIG_VMGENID, random.trust_cpu= cmdline, and io_uring; in
# particular CRNG never initialized inside a restored guest, so
# `getrandom(2)` blocked forever and `pip install` / urllib HTTPS hung.
# See #218 + #225 for the full root cause walk.
#
# Usage
# -----
#
#   # Download path (default, recommended):
#   sudo ./scripts/install-guest-kernel.sh
#
#   # Build-from-source path:
#   sudo ./scripts/install-guest-kernel.sh --build
#
#   # Custom install path:
#   sudo OUT_PATH=/tmp/vmlinux ./scripts/install-guest-kernel.sh
#
# Verify after install:
#
#   sandstorm from-image python:3.12-slim --tag test-pyt
#   # then inside a forked child:
#   python3 -c 'import ssl; ssl.create_default_context(); print("ok")'

set -euo pipefail

OUT_PATH="${OUT_PATH:-/var/lib/sandstorm/kernels/vmlinux}"
KERNEL_URL="${KERNEL_URL:-https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.13/x86_64/vmlinux-6.1.141}"
# sha256 pin for the FC CI vmlinux. Verified end-to-end on the dev
# box on 2026-06-05 — `getrandom(2)` initializes, `import ssl` works,
# `pip install numpy` completes. Override `EXPECTED_SHA256=""` to skip
# the check (e.g. when pointing KERNEL_URL at a private mirror).
EXPECTED_SHA256="${EXPECTED_SHA256:-b36a4a1b10f33b9cfdcde3d1a787d9c090556a3edb211cd06d1f3f9a6c7e8724}"
MODE="download"

while [ $# -gt 0 ]; do
    case "$1" in
        --build) MODE="build" ;;
        --download) MODE="download" ;;
        --help|-h)
            sed -n '2,/^$/p' "$0"
            exit 0
            ;;
        *) echo "Unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

# Privilege shim: when already root (e.g. `sandstorm quickstart` running
# the embedded copy of this script in a container or via sudo -E),
# `sudo` may not even be installed — call the commands directly.
if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
else
    SUDO="sudo"
fi

INSTALL_DIR="$(dirname "$OUT_PATH")"
$SUDO mkdir -p "$INSTALL_DIR"

case "$MODE" in
    download)
        echo "==> downloading Firecracker CI vmlinux-6.1.141"
        echo "    from: $KERNEL_URL"
        echo "    to:   $OUT_PATH"
        TMP="$(mktemp -t sandstorm-vmlinux.XXXXXX)"
        # shellcheck disable=SC2064
        trap "rm -f '$TMP'" EXIT
        curl -fL --progress-bar -o "$TMP" "$KERNEL_URL"
        if [ -n "$EXPECTED_SHA256" ]; then
            echo "==> verifying sha256"
            ACTUAL=$(sha256sum "$TMP" | awk '{print $1}')
            if [ "$ACTUAL" != "$EXPECTED_SHA256" ]; then
                echo "ERROR: sha256 mismatch (got $ACTUAL, want $EXPECTED_SHA256)" >&2
                exit 1
            fi
        fi
        $SUDO cp "$TMP" "$OUT_PATH"
        $SUDO chmod 644 "$OUT_PATH"
        ;;
    build)
        echo "==> building from source via scripts/build-guest-kernel.sh"
        SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
        "$SCRIPT_DIR/build-guest-kernel.sh"
        ;;
esac

echo
echo "==> installed:"
ls -la "$OUT_PATH"
# `file` / `strings` are nice-to-have descriptors, not part of the
# install — minimal hosts (containers, cloud images) often lack them.
command -v file > /dev/null && file "$OUT_PATH" | head -1
command -v strings > /dev/null \
    && strings "$OUT_PATH" 2>/dev/null | grep -E "^Linux version" | head -1
true

echo
echo "==> next: rebuild any v0.5.0-era snapshots so they pick up the"
echo "    virtio-rng device and #218's CRNG fix:"
echo
echo "    sandstorm ls --snapshots   # see what you have"
echo "    sandstorm rmi <tag>"
echo "    sandstorm from-image python:3.12-slim --tag <tag>"
