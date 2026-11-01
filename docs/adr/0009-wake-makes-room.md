# 9. A wake makes room on its node, spares sandboxes in use, and has 2 s to start

Date: 2026-10-28

## Status

Accepted. Made in #360 and #361.

## Context

With [ADR 8](0008-nodes-protect-themselves.md) alone, a request that reached
a sandbox on disk while its node was under its memory floor got 503. With 40
sandboxes per node where about 12 fit awake, that was 13% of requests at
5 req/s and a third at 40, while frozen sandboxes nobody had called for
minutes held the memory.

Making room raised three questions, each settled by measurement at 80
sandboxes per node and 8 req/s on 2026-10-28:

- **What to spare.** Sparing anything used in the last 2 s, counted from when
  the request arrived, put a just-woken sandbox straight back to disk (98% of
  the 502s); counted from the answer, it spared nearly every hot sandbox and
  26% of requests were refused for want of anything to put away.
- **Who holds the restore permit.** When it went with the request, a client
  that gave up handed the permit on while its wake carried on. With a 60 s
  client timeout, 24 restores started in the same second on a 2-vCPU node and
  ran it out of memory and swap for an hour and a half.
- **How long to wait.** With no bound, a popular sandbox's wake waited 12 s
  behind wakes of sandboxes called once, and the answered requests' p50 went
  from 13 ms to 4.6 s.

## Decision

- **A wake, from the gateway or the master, makes room.** While the node is
  under its memory or swap floor, it puts the sandboxes least in demand to
  disk first, frozen before hot, at most four per wake. Past that, or with
  nothing to choose, the wake is refused.
- **Demand decides who goes.** Each request adds one to a sandbox's demand,
  decaying by `e` every 30 s. A sandbox woken for a request counts as used
  from then, for the master's demotions as well.
- **A request in flight protects its sandbox.** So does having come up in the
  last 2 s with no request yet, or a tier change already under way. A wake
  checks again, once it holds the sandbox's lock, that nothing has reached
  it.
- **Memory pressure is waited out, not made room for.** PSI is a 10-second
  average a checkpoint does not move, so a wake waits for it while holding
  its restore permit, and the wakes behind it queue.
- **The restore permit belongs to the wake** until it ends. A wake whose
  caller gives up while still queued never starts.
- **A wake has 2 s to start** (`--wake-budget-ms`): to get a restore slot and
  for pressure to ease. After that it is answered 503 with `retry-after`. A
  wake that would wait behind more than twice as many wakes as there are
  slots (`--wake-max-waiting`) is refused at once.
- **The master follows.** The node lists what it put away (`evicted`) in its
  reports, and the master moves desired state there rather than asking for
  the sandbox back, which would make the node evict something else.

## Consequences

- A node far over its awake capacity keeps answering: demand-ranked sandboxes
  cycle to disk instead of the node refusing.
- A caller waits at most about 2 s before learning that a wake cannot start,
  and is told when to retry.
- Every 502 the gateway answers is logged with its reason.
- The defaults (four evictions, 2 s, `e`-folding every 30 s) were chosen from
  runs on 2-vCPU nodes.
