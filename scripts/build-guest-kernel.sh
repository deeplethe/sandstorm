#!/bin/bash
# Build a sandstorm-compatible vmlinux from a recent Linux LTS source.
#
# This is the build recipe behind `/var/lib/sandstorm/kernels/vmlinux`
# shipped with sandstorm v0.5.1 and later. Reproducible: same inputs
# (kernel branch + FC reference config) → byte-equivalent vmlinux
# modulo build environment.
#
# Why this exists
# ---------------
#
# sandstorm v0.5.0 and earlier shipped a Linux 4.14.174 vmlinux built in
# 2021. That kernel predated:
#
#   - CONFIG_HW_RANDOM_VIRTIO=y      (virtio-rng driver binding)
#   - CONFIG_RANDOM_TRUST_CPU=y      (RDRAND CRNG init)
#   - random.trust_cpu= cmdline      (Linux 4.19+)
#   - CONFIG_VMGENID=y               (Linux 5.20+, VM-restore CRNG re-seed)
#
# Symptom: `getrandom(2)` blocks forever after FC restore — anything
# that touches OpenSSL (`pip install`, `urllib` HTTPS, `requests`)
# hangs. See #218 + #225.
#
# Linux 6.1 LTS has all three options enabled in FC's reference
# microvm config and Linux 5.20+ includes VMGENID, so a straight rebuild
# from upstream closes the bug.
#
# Usage
# -----
#
#   # Default: Linux 6.1 LTS, install to /var/lib/sandstorm/kernels/vmlinux
#   sudo ./scripts/build-guest-kernel.sh
#
#   # Override target version / work dir / install path:
#   KERNEL_BRANCH=v6.6 \
#   WORK_DIR=/tmp/my-kbuild \
#   OUT_PATH=/tmp/vmlinux \
#     ./scripts/build-guest-kernel.sh
#
# Requires
# --------
#
#   gcc make bc flex bison libelf-dev libssl-dev cpio git curl
#
# Disk + time
# -----------
#
#   ~5 GB free under WORK_DIR (Linux source ~3 GB, build artifacts ~1.5 GB)
#   ~10 min on a 20-core x86_64 host. Mostly bound by file I/O during the
#   initial git clone, not the actual compile.

set -euo pipefail

KERNEL_BRANCH="${KERNEL_BRANCH:-v6.1}"
WORK_DIR="${WORK_DIR:-/tmp/sandstorm-kernel-build}"
FC_VERSION="${FC_VERSION:-v1.12.0}"
OUT_PATH="${OUT_PATH:-/var/lib/sandstorm/kernels/vmlinux}"
ARCH="${ARCH:-x86_64}"

VERSION_NUMBER="${KERNEL_BRANCH#v}"
SRC_DIR="${WORK_DIR}/linux-${VERSION_NUMBER}"
CONFIG_URL="https://raw.githubusercontent.com/firecracker-microvm/firecracker/${FC_VERSION}/resources/guest_configs/microvm-kernel-ci-${ARCH}-${VERSION_NUMBER}.config"

echo "==> sandstorm guest-kernel build"
echo "    kernel:      ${KERNEL_BRANCH}"
echo "    arch:        ${ARCH}"
echo "    fc config:   ${FC_VERSION}"
echo "    work dir:    ${WORK_DIR}"
echo "    install to:  ${OUT_PATH}"

# Sanity-check the toolchain so the failure mode below is "missing
# dep" not "make exit 2 on line 731".
for bin in gcc make bc flex bison cpio git curl; do
    command -v "$bin" >/dev/null 2>&1 || {
        echo "ERROR: missing build dependency: $bin" >&2
        echo "Ubuntu/Debian: sudo apt install gcc make bc flex bison libelf-dev libssl-dev cpio" >&2
        exit 2
    }
done
[ -f /usr/include/libelf.h ] || {
    echo "ERROR: libelf-dev not installed (no /usr/include/libelf.h)" >&2
    echo "Ubuntu/Debian: sudo apt install libelf-dev" >&2
    exit 2
}
[ -f /usr/include/openssl/ssl.h ] || {
    echo "ERROR: libssl-dev not installed (no /usr/include/openssl/ssl.h)" >&2
    echo "Ubuntu/Debian: sudo apt install libssl-dev" >&2
    exit 2
}

mkdir -p "$WORK_DIR"

if [ ! -d "$SRC_DIR" ]; then
    echo "==> cloning ${KERNEL_BRANCH} (shallow, ~5 min)"
    git clone --depth 1 --branch "$KERNEL_BRANCH" \
        https://github.com/gregkh/linux.git "$SRC_DIR"
fi

cd "$SRC_DIR"
echo "==> fetching FC reference config"
curl -sSL --fail -o .config "$CONFIG_URL" || {
    echo "ERROR: failed to download FC reference config from $CONFIG_URL" >&2
    echo "Pick a different KERNEL_BRANCH or FC_VERSION (must have a matching config)." >&2
    exit 2
}

# Sanity-check the entropy-relevant configs are in place. FC's reference
# already sets them since v1.10ish, but we re-assert in case a future
# config drops them.
for opt in CONFIG_HW_RANDOM_VIRTIO CONFIG_RANDOM_TRUST_CPU CONFIG_VMGENID; do
    if ! grep -q "^$opt=y" .config; then
        echo "    enabling $opt (not set in upstream FC config)"
        scripts/config --file .config -e "${opt#CONFIG_}"
    fi
done

echo "==> reconciling config"
make olddefconfig >/dev/null

echo "==> building vmlinux on $(nproc) cores"
time make vmlinux -j"$(nproc)" 2>&1 | tail -3

[ -f vmlinux ] || { echo "ERROR: build did not produce vmlinux" >&2; exit 1; }

echo "==> built:"
ls -la vmlinux
file vmlinux | head -1

if [ "$OUT_PATH" != "" ]; then
    INSTALL_DIR="$(dirname "$OUT_PATH")"
    if [ ! -d "$INSTALL_DIR" ]; then
        sudo mkdir -p "$INSTALL_DIR"
    fi
    sudo cp vmlinux "$OUT_PATH"
    sudo chmod 644 "$OUT_PATH"
    echo "==> installed to $OUT_PATH"
    echo ""
    echo "Verify in a new guest:"
    echo "    sandstorm from-image python:3.12-slim --tag test-pyt"
    echo "    sandstorm fork --tag test-pyt -n 1"
    echo "    # then in the child: python3 -c 'import ssl; ssl.create_default_context()'"
fi
