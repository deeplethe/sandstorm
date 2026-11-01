# A snapshot under an upgraded host, 2026-10-31

[A warm image](../2026-10-31-warm-image/README.md) went wrong when the host's libraries changed under it. Warm images can be seeded again. A snapshot holds a user's memory, so this checks what happens to one.

## How it was run

On the bare-metal machine, gVisor, one node in its own directory. Nothing on the host was upgraded: a template bound a directory of its own, `/opt/sst-snap`, and ran `python3.12` from it. "Upgraded" means renaming python 3.12.3-1ubuntu0.17 over 3.12.3-1 in that directory, which is what `dpkg` does.

The program keeps a random id drawn at start and a request count, in memory. The same id and a count that carries on mean the memory survived.

## Before

| Case | Result |
|---|---|
| Snapshot, wake, nothing changed | Memory kept |
| Hot sandbox, file upgraded under it | Unaffected, memory kept |
| **Snapshot, upgrade, wake** | **Memory lost, nothing reported** |
| **Tag, upgrade, fork** | Child restarts for ever, about every 25 s, reported as `hot` |

The third row, in order:

1. `runsc restore` succeeds, and the node deletes the image at once.
2. The program dies.
3. `observe` finds no process and no image, so the master creates the sandbox again from its template.
4. The caller's request times out once. The next one gets a 200 with a new id and a count of 1, and the API says `hot` with no error.

## After

The image stays until the sandbox has lasted 60 s.

| Step | Result |
|---|---|
| Snapshot, upgrade, wake | HTTP 503 with the reason, tier `snapshot`, image kept |
| Old file put back | Back 35 s later, same id, count carries on: **memory kept** |
| 105 s after that | Up, image gone |
| Tag, upgrade, fork | Child stops after 5 failures with the same reason, and the master quarantines it |
| Snapshot, wake, nothing changed | Memory kept, image gone within 100 s |

A sandbox that dies is tried again at most once a minute. If the files do not come back it stays a snapshot and goes on failing with the reason.

Firecracker, three rounds of snapshot and wake inside the window, with the diff chain growing to `mem.3`: all answered, and the image went after the sandbox had lasted.

## What this does not settle

- **The program's way of dying depends on what changed.** Here a snapshot died at once and a fork hung for about 25 s first. A program that keeps running on a changed file and misbehaves without dying is not detected.
- **A real upgrade rarely gets undone.** Keeping the image protects the memory, but getting it back needs the old files. Pinning a sandbox's bound files at snapshot time would remove the problem, and is not built.
- **Dying within 60 s of a wake for any reason now counts.** A sandbox that crashes on its own in that minute goes back to its snapshot rather than starting fresh, which loses what it did since waking.
- **A node restart is covered,** because the marker is on disk, but that was not run.
