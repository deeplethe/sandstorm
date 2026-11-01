# A warm image under an upgraded host, 2026-10-31

Warm creates on gVisor took 160 ms each in a burst, against 37 ms for a fork, and restores did not account for it. They were slow because every one of them ran twice.

## What happened

| When (HKT) | |
|---|---|
| 10-30 12:50 | `app64-warm`'s warm image is taken |
| 10-31 07:07-07:14 | unattended-upgrades replaces `python3.12`, `libc` and `ld.so` on the host |
| Every create after that | The sandbox restores, then segfaults about 200 ms later |

Traced with `runsc --debug` on the old image: PID 1 takes signal 11 at address `0x18`, and the sandbox exits with status 11. A sandbox on gVisor runs on the host's own `/usr`, bound in read-only, and the image holds a process laid out for the files it had mapped.

The node then does what it does for any dead sandbox. `observe` no longer lists it and the master asks for it again. The second restore fails, because runsc still holds a stopped container of that name, so `create` removes it and boots cold. A caller sees a 502 or a wait, and the node logs `warm start failed; booting instead`, which reads as a one-off.

Forks did not show it: a fork's parent had already gone through the same fallback, so its image was a healthy one. Firecracker does not show it either: a guest runs on its own copy of the template's disk.

A 20-create burst, each sandbox's times in ms: all 20 running behind their gateways at 1,199, first answers at 2,278 to 3,734. With a new image the last answers at 1,155.

## Results

gVisor, 16 threads, `app64-warm`, created at once, until all answer. Before is the old image, from [the parallel run](../2026-10-31-parallel/README.md); after is with #416 and this fix together, on a new image, mean of two runs (`warm.jsonl`).

| | 10 | 20 | 50 | 100 |
|---:|---:|---:|---:|---:|
| Before, ms | 2,056 | 3,658 | 7,916 | 15,909 |
| **After, ms** | **520** | **876** | **1,825** | **3,820** |
| Fork, same run, ms | 478 | 837 | 1,827 | 3,482 |

No `warm start failed`, no dropped image and no `backing off` in the log across either run.

## Both halves, on the machine

- **Key:** with the old image in place under its old key, the fixed build took a new key, seeded, and 3 of 3 stayed up. `/usr/bin` and `/usr/lib/x86_64-linux-gnu` both had an mtime of 07:14, the upgrade.
- **Dropping the image:** with the old image's contents under the new key, 6 sandboxes died once, the first repeat dropped it, and a new image was ready 1.4 s later. The next 6 started warm and stayed up.

The key reads about 100 directory times per create, 1.4 ms from Python on this machine.

## What this does not cover

- **Snapshots and tags** hold the same kind of memory image and are restored over the same bound files. This was not tested. A sandbox on disk across an upgrade may die the same way, and there the memory cannot be seeded again.
- **The key looks one level down.** A file replaced deeper, such as numpy under `dist-packages`, is left to the second half: one burst dies once, then a new image.
- **A program that dies within 30 s of a warm start for its own reasons** costs the image a reseed, at most once per 10 minutes if the image is new.
