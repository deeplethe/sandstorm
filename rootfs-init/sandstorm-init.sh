#!/bin/bash
# /sandstorm-init.sh — PID 1 inside the guest. Mounts pseudo-fs, fixes
# DNS to public resolvers, then launches the Python agent.

mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null

# Terminals and shared memory, which a program expects of any Linux: envd's
# PTY opens /dev/ptmx, and Python's multiprocessing uses /dev/shm.
mkdir -p /dev/pts /dev/shm
mount -t devpts -o ptmxmode=0666,mode=0620 devpts /dev/pts 2>/dev/null
ln -sf pts/ptmx /dev/ptmx
mount -t tmpfs tmpfs /dev/shm 2>/dev/null

# Mount /tmp as tmpfs so per-sandbox scratch writes (agent logs,
# socket paths, hint files used by demos) land in this VM's memory
# instead of the shared on-disk rootfs.ext4. Without this, multiple
# sandboxes booted from the same snapshot file would concurrently
# write to the same inode in the same loop-mounted ext4 and corrupt
# it ("Structure needs cleaning" on the next access).
# tmpfs state survives BRANCH because it lives in guest RAM, which
# is what memory.bin captures.
mount -t tmpfs -o size=256m tmpfs /tmp 2>/dev/null

# Make sure PATH covers both Ubuntu (/usr/bin) and official python (/usr/local/bin)
# images. Subprocess invocations from the agent inherit this.
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

# Persistent volumes — see VolumeSpec / BootConfig::with_volume on the host.
# The kernel cmdline carries an entry of the form:
#   sandstorm.mounts=vdb:/opt/cache,vdc:/var/cache/pip
# where each pair is "<device>:<guest mount path>".
mounts="$(grep -oE 'sandstorm\.mounts=[^ ]+' /proc/cmdline | head -1 | cut -d= -f2-)"
if [ -n "$mounts" ]; then
    IFS=',' read -ra _pairs <<<"$mounts"
    for pair in "${_pairs[@]}"; do
        dev="${pair%%:*}"
        target="${pair#*:}"
        if [ -z "$dev" ] || [ -z "$target" ] || [ "$dev" = "$target" ]; then
            echo "sandstorm-init: ignoring malformed mount entry '$pair'" >&2
            continue
        fi
        mkdir -p "$target"
        if ! mount "/dev/$dev" "$target" 2>/dev/null; then
            echo "sandstorm-init: WARN mount /dev/$dev -> $target failed" >&2
        fi
    done
fi

# Ubuntu Docker images symlink /etc/resolv.conf to a systemd-resolved
# stub that doesn't exist in our minimal init. Point to public resolvers
# so the guest can do DNS over the netns + host bridge NAT path.
rm -f /etc/resolv.conf
{
    # A program's A and AAAA lookups can lose each other in NAT, and glibc
    # waits that out. See RESOLVERS in bundle.rs.
    echo "options single-request-reopen timeout:2"
    echo "nameserver 1.1.1.1"
    echo "nameserver 8.8.8.8"
} > /etc/resolv.conf

# A hostname that resolves without DNS. The kernel names the guest after its
# IP when ip= gives none, and resolving that goes to a resolver -- which a
# sandbox usually cannot reach, so it waits out the timeout. Python's
# HTTPServer resolves it before it listens: 20 s to a first answer on bm.
echo sandbox > /proc/sys/kernel/hostname
printf "127.0.0.1 localhost\n127.0.1.1 sandbox\n" > /etc/hosts

echo "sandstorm-init: launching agent..."
# Find python: Ubuntu has /usr/bin/python3; official python:* images have /usr/local/bin/python3.
for PY in /usr/local/bin/python3 /usr/bin/python3 /usr/local/bin/python /usr/bin/python; do
    if [ -x "$PY" ]; then
        exec "$PY" /sandstorm-agent.py
    fi
done
echo "sandstorm-init: ERROR: no python interpreter found in /usr/bin or /usr/local/bin" >&2
# Park PID 1 so the kernel doesn't panic. The agent won't be available
# but at least snapshot/restore plumbing still works for debugging.
exec sleep infinity
