#!/bin/sh
# The installer's backend choice and its preflight, run against stubs.
#
# Everything the installer reaches outside this machine is stubbed: curl
# serves the CA and a stand-in sandstormd from a directory, openssl hashes to
# the token's pin, systemctl does nothing, and id says root. KVM is a
# character device the test picks (/dev/null for "there is one"), and
# `cp --reflink=always` succeeds or fails as the case asks, so each refusal
# can be reached on any Linux machine. What is checked is what an operator
# would see: which backend it took, what it refused and why, and that a
# refusal leaves nothing installed.
#
#   sh packaging/tests/install_backend.sh
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/../install.sh"
REAL_CP="$(command -v cp)"
PIN="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
fails=0

setup() {
  T="$(mktemp -d)"
  mkdir -p "$T/stub" "$T/srv" "$T/root"
  # The node binary the master serves: answers --version, and records the
  # preflight it was asked for, failing it when the case says so.
  cat > "$T/srv/sandstormd" <<EOF
#!/bin/sh
case "\$1" in
  --version) echo "sandstormd 0.0.0-test" ;;
  preflight) shift; echo "\$*" > "$T/preflight.args"
    [ -f "$T/preflight.fail" ] && { echo "Error: no guest kernel" >&2; exit 1; }
    echo "ok" ;;
esac
EOF
  echo "-----BEGIN CERTIFICATE-----" > "$T/srv/ca.pem"
  cat > "$T/stub/curl" <<EOF
#!/bin/sh
out=""; url=""
while [ \$# -gt 0 ]; do
  case "\$1" in -o) out="\$2"; shift ;; -*) ;; *) url="\$1" ;; esac
  shift
done
f="$T/srv/\${url##*/}"
[ -f "\$f" ] || exit 22
cp "\$f" "\$out"
EOF
  cat > "$T/stub/openssl" <<EOF
#!/bin/sh
if [ "\$1" = dgst ]; then cat >/dev/null; echo "SHA2-256(stdin)= $PIN"; else cat "\$3"; fi
EOF
  printf '#!/bin/sh\nexit 0\n' > "$T/stub/systemctl"
  printf '#!/bin/sh\necho 0\n' > "$T/stub/id"
  cat > "$T/stub/cp" <<EOF
#!/bin/sh
# A reflink this filesystem may not do: refused, or done as a plain copy.
if [ "\$1" = --reflink=always ]; then
  [ -f "$T/reflink.ok" ] || exit 1
  shift
fi
exec "$REAL_CP" "\$@"
EOF
  printf '#!/bin/sh\necho runsc version release-test\n' > "$T/runsc"
  printf '#!/bin/sh\necho Firecracker v1.10.1\n' > "$T/firecracker"
  echo kernel > "$T/vmlinux"
  chmod +x "$T"/stub/* "$T/srv/sandstormd" "$T/runsc" "$T/firecracker"
}

# Run the installer with these extra variables; output in $T/out.
run() {
  # env -i so nothing from the caller's environment (a real runsc on the
  # PATH, a SANDSTORM_* left set) decides the case.
  set +e
  env -i PATH="$T/stub:/usr/bin:/bin" HOME="$T" \
    SANDSTORM_MASTER_URL=https://master.test \
    SANDSTORM_NODE_TOKEN="$PIN.secret" \
    SANDSTORM_BIN="$T/root/sandstormd" \
    SANDSTORM_STATE_DIR="$T/state" \
    SANDSTORM_UNIT="$T/root/sandstorm-node.service" \
    SANDSTORM_NODE_ID=n1 \
    SANDSTORM_KVM_DEVICE="$T/no-kvm" \
    "$@" sh "$SCRIPT" > "$T/out" 2>&1
  code=$?
  set -e
}

check() { # what, condition...
  what="$1"; shift
  if "$@"; then echo "ok    $what"; else echo "FAIL  $what"; sed 's/^/      | /' "$T/out"; fails=$((fails + 1)); fi
}
said() { grep -q -- "$1" "$T/out"; }
refused() { [ "$code" != 0 ] && [ ! -e "$T/root/sandstormd" ] && [ ! -e "$T/root/sandstorm-node.service" ]; }
unit_has() { grep -q -- "$1" "$T/root/sandstorm-node.service"; }
backend_is() { [ "$(cat "$T/state/backend")" = "$1" ]; }

setup
run SANDSTORM_BACKEND=gvisor
check "gvisor without runsc is refused, naming runsc" refused
check "  and says what is missing" said "gvisor needs runsc"

setup
run SANDSTORM_BACKEND=gvisor SANDSTORM_RUNSC="$T/runsc"
check "gvisor with runsc installs" [ "$code" = 0 ]
check "  the node's own preflight ran for runsc" grep -q -- "--backend runsc --runsc $T/runsc" "$T/preflight.args"
check "  the backend file says runsc" backend_is runsc
check "  the unit reads the backend file" unit_has "--backend-file $T/state/backend"
check "  and does not fix --backend itself" sh -c "! grep -q -- '--backend ' '$T/root/sandstorm-node.service'"

setup
run SANDSTORM_RUNSC="$T/runsc"
check "no SANDSTORM_BACKEND is gvisor, as before" backend_is runsc

setup
run SANDSTORM_BACKEND=auto SANDSTORM_RUNSC="$T/runsc"
check "auto without KVM picks gvisor" backend_is runsc
check "  and says why" said "auto -> gvisor, because this machine has no /dev/kvm"

setup
# A path that is not there, so a real kernel on this machine is not found.
run SANDSTORM_BACKEND=auto SANDSTORM_KVM_DEVICE=/dev/null SANDSTORM_FC_KERNEL="$T/no-vmlinux"
check "auto with KVM but no kernel is refused" refused
check "  saying it picked firecracker and why" said "auto -> firecracker, because this machine has /dev/kvm"
check "  naming the kernel and the way out" said "needs a guest kernel"
check "  and offering gVisor instead" said "SANDSTORM_BACKEND=gvisor"

setup
run SANDSTORM_BACKEND=firecracker
check "firecracker without KVM is refused" refused
check "  naming /dev/kvm" said "needs /dev/kvm"

setup
run SANDSTORM_BACKEND=firecracker SANDSTORM_KVM_DEVICE=/dev/null SANDSTORM_FC_KERNEL="$T/vmlinux"
check "firecracker on a work dir that cannot reflink is refused" refused
check "  naming XFS and btrfs" said "XFS with reflink=1 or on btrfs"

setup
touch "$T/reflink.ok" "$T/preflight.fail"
run SANDSTORM_BACKEND=firecracker SANDSTORM_KVM_DEVICE=/dev/null SANDSTORM_FC_KERNEL="$T/vmlinux" SANDSTORM_FIRECRACKER="$T/firecracker"
check "firecracker the node's preflight refuses is not installed" refused
check "  and says so" said "cannot run firecracker"

setup
touch "$T/reflink.ok"
run SANDSTORM_BACKEND=firecracker SANDSTORM_KVM_DEVICE=/dev/null SANDSTORM_FC_KERNEL="$T/vmlinux" SANDSTORM_FIRECRACKER="$T/firecracker"
check "firecracker with everything installs" [ "$code" = 0 ]
check "  the preflight ran for firecracker" grep -q -- "--backend firecracker --firecracker $T/firecracker --fc-kernel $T/vmlinux --work-dir $T/state" "$T/preflight.args"
check "  the backend file says firecracker" backend_is firecracker
check "  the unit names the binary and kernel" unit_has "--fc-kernel $T/vmlinux"
check "  the unit's comments are not run as commands" sh -c "! grep -q 'not found' '$T/out'"
check "  and reach the unit whole" grep -qF 'for `--listen` on a port' "$T/root/sandstorm-node.service"
check "  no reflink probe is left behind" sh -c "! ls -a '$T/state' | grep -q reflink-probe"

setup
run SANDSTORM_BACKEND=kata
check "an unknown backend is refused" refused
check "  listing the choices" said "auto, gvisor or firecracker"

[ "$fails" = 0 ] || { echo "$fails failed"; exit 1; }
echo "all passed"
