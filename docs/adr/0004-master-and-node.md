# 4. A master holds desired state in SQLite; nodes run sandboxes

Date: 2026-10-22

## Status

Accepted

## Context

The v0.5 controller kept its state in an in-memory map mirrored to a JSON
file. Every sandbox was its child, and on restart `kill_orphans` killed every
Firecracker it found because it could not tell an orphan from a sandbox it
was managing (issue #301). A restart or an upgrade lost every sandbox on the
machine; across a fleet it would lose all of them.

CubeSandbox, reviewed while designing v0.6, keeps state in five places with
nothing reconciling them. v0.6 set out to have one place for what should be
and one for what is.

## Decision

The work is split between a master and nodes.

- **The master** (`crates/sandstorm-core/src/master`) holds desired state in
  SQLite (`--state-db`): which sandboxes should exist, on which node, in
  which tier. It serves the public API, places sandboxes, issues node
  identities and runs the idle-demotion loop. It changes desired state only;
  it never runs a sandbox.
- **A node** (`crates/sandstorm-core/src/node`) runs sandboxes on `runsc` and
  reports what it actually holds. The master diffs desired state against the
  report, and the difference is the node's work.
- **The database holds what is wanted, never what was observed**
  (`state.rs`). Actual state is read back from the node every time; a
  controller that trusts its own cache kills sandboxes it merely forgot.
- **A restart adopts, it never reconciles by killing.** `runsc create` and
  `runsc start` daemonise the sandbox, so it is not a child of `sandstormd`
  and a control-plane restart does not touch it. Measured on bm: `kill -9`
  on the controller cost 0 of 20 sandboxes, and all 20 were adopted again in
  134 ms.

## Consequences

- Restarting or upgrading the master or a node does not stop sandboxes.
- A node keeps serving its sandboxes, including waking them, while the master
  is unreachable; only creates and placement wait for it.
- The RFC planned PostgreSQL for the master's database. What is built is a
  single SQLite file, which keeps a one-machine install to one binary and one
  file ([ADR 10](0010-one-binary.md)) and means the master itself is not
  replicated.
- Each node decides for itself whether it can take work
  ([ADR 8](0008-nodes-protect-themselves.md)); the master's view of a node's
  memory is only as fresh as its last report.
