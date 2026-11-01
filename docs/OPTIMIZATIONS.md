# Backend optimizations

gVisor and Firecracker are slow in different places, so each has its own work
here. Every effect below is a before and after measured when the change was
made. Later changes moved the numbers, so rows do not add up; the current figures
are in the [README](../README.md#latency).

Web is a small web server and the app is a 64 MiB app. Figures are p50 unless the
row says otherwise. The VM is a 2-core, 3.6 GB cloud machine, which runs Firecracker
under PVM, and the KVM machine is 2 x Xeon E5-2637 v2.

## gVisor

Measured on the VM, with its ~110 MB/s disk and a 376 MiB Python heap for the
reclaim and compression rows.

| Optimization | Effect |
|---|---|
| Frozen sandboxes' cold pages reclaimed into zswap | 376 → 113 MiB held; touching it all after a thaw 60 → 310 ms |
| Reclaimed pages kept in zswap, not written to disk | Touching them all back 3.9 s → 310 ms |
| Snapshots compressed only on disks under 500 MB/s | Image 363 → 84 MiB; cold restore 3.3 → 1.4 s |
| Warm template images never compressed | Restore from page cache 696 → 138 ms |
| One image serves every fork of a tag | Five children of a 14 MiB image in 226 ms, nothing copied |
| A branched parent restored from the image just written | Parent pauses about 80 ms: 33 to checkpoint, 45 to restore |

Reclaim also runs on Firecracker's cgroups, but these numbers are gVisor's.

Tried and dropped: `runsc restore --background`, which returned early but left the
app waiting for the whole image, and `--direct`, which took 3.2 s by skipping the
page cache.

## Firecracker

Measured on the VM. Rows marked KVM are from the KVM machine.

| Optimization | Effect |
|---|---|
| Restored VMs' writes tracked by soft-dirty bits | Warm app 103 → 67 ms; branch 52 → 25 ms |
| A branched parent resumes in place | Branch 278 → 38 ms, flat across branches |
| Clock set by the agent, Firecracker started with `setns` | Warm web 57 → 52 ms, app 88 → 83 ms |
| Warm start clear of renames and deletes | KVM: web 129 → 57 ms, app 199 → 98 ms |
| Same, for forks | KVM: 10 at once 831 → 487 ms, 20 at once 1,510 → 924 ms |
| Creates side by side, namespace pool sized by memory | KVM: fork of 20 1,016 → 343 ms, of 100 8,021 → 1,664 ms; gVisor 2,141 → 916 ms, 13,837 → 3,667 ms |
| Sandbox reachable as soon as it is created, pool of 32 | Fork of 10 1,091 → 591 ms, of 20 2,744 → 1,245 ms |
| i8042 boot options, so the kernel stops probing for PS/2 | Boot to init 0.77 → 0.29 s; KVM 0.32 → 0.22 s |
| `quiet` kernel console | KVM: cold web 861 → 652 ms, app 1,167 → 1,066 ms |
| Template's program started before the agent's imports | KVM: cold web 652 → 636 ms, app 1,066 → 964 ms |
| Serial console off under PVM | Cold web 903 → 740 ms, app 1,103 → 897 ms |

Tried and dropped: two vCPUs per guest, which took cold web from 636 to 597 ms and
left the app unchanged with a worse p90.

## Both

Measured on Firecracker on the VM, except where the row says.

| Optimization | Effect |
|---|---|
| Warm templates (opt-in) restore an image of the started app | KVM: web 545 → 67 ms, app 921 → 100 ms |
| Namespaces built ahead, claimed with ioctls | Claim 33 → 11 ms |
| A destroyed sandbox's namespace reused, not rebuilt | Warm app back to back 124-136 → 70-73 ms |
| Gateway retries a starting sandbox every 50 ms at most | gVisor, KVM: first answer was 300-370 ms late |
