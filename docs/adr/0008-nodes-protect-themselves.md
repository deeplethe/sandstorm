# 8. Each node decides for itself whether it can take work

Date: 2026-10-28

## Status

Accepted

## Context

Overselling means a node runs close to its memory limit on purpose. Placement
on the master alone did not protect it: six 2-vCPU / 1.9 GiB nodes reporting
about 1,500 MiB free were handed 3,000 sandboxes in 2.1 seconds. Every
placement passed the master's 512 MiB floor because every one saw the same
stale 1,500 MiB. The nodes ran down to 68 MiB, load 27, and stopped
answering ssh.

Free memory alone also misleads. On bm the apparent cost per sandbox fell
while memory pressure climbed, because swap was absorbing it; and a node
whose pages had gone to zswap could show 678 MiB available with no swap
left.

## Decision

A node checks its own current reading before any work that costs memory -- a
create, or a promotion to hot -- and holds it back if it is:

- below its free-memory floor (`--admit-min-free-mib`, default 512);
- below its swap floor, derived from the same setting, when it has swap;
- above its memory-pressure ceiling: the 10-second PSI `some` average
  (`--admit-max-psi`, default 8; `--wake-max-psi` for wakes).

Teardowns and demotions are never held back, since they are how a short node
stops being short. A reading that fails reads as 0 and does not refuse, so an
unreadable `/proc` cannot wedge the fleet.

The master keeps its own admission check (`master::Admission`) for placement,
on the last reported figures, but the node's decision is the one that holds:
it is the only process with a current number, and the one that pays.

Overload is refused quickly rather than queued:

- A create a node holds back is reported as `held_back`, kept as the reason
  the sandbox is not up yet, retried within 5 s, and never counted towards
  quarantine.
- A create the master cannot place anywhere is answered 503.
- A wake that cannot be done in time is answered 503 with `retry-after`
  ([ADR 9](0009-wake-makes-room.md)).

## Consequences

- A node short on memory turns work away instead of running out, and keeps
  answering ssh.
- Callers see 503s under overload and have to retry; the gateway's
  `retry-after` says when.
- PSI is a 10-second average, so it lags: a burst can push a node past the
  ceiling before the average shows it.
- Squeezing hot sandboxes' cold pages into compressed swap backs off at half
  the PSI ceiling, so that reclaim is never the reason a node refuses a
  sandbox.
