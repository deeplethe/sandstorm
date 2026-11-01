# 10. One binary: master, node and panel

Date: 2026-10-22

## Status

Accepted

## Context

v0.5 shipped a CLI that drove Firecracker directly and a separate controller
daemon, and the two control paths did not work together. v0.6 has a master
and nodes ([ADR 4](0004-master-and-node.md)), and most deployments are one
machine. Adding a machine has to work on hosts with no internet and no
release infrastructure, which describes the machines Sandstorm is developed
on.

## Decision

`sandstormd` (`crates/sandstorm-core`) is the only binary.

- `sandstormd serve --master` runs the control plane, `--node` runs a node,
  and both flags together run a single-machine deployment in one process.
  In that case the node still talks to the master over the same protocol,
  on loopback.
- The web panel (`GET /`) is one HTML file built into the binary. It has no
  build step and nothing to deploy separately, so it is always the version of
  the master it talks to.
- A joining node fetches the master's own binary, along with the CA, from the
  master. The two cannot drift, and joining needs no internet.

## Consequences

- A single-machine install is one binary and one SQLite file.
- The one-process deployment still needs its own tests. It once took a
  shortcut to its node's registration token that skipped the CA fingerprint,
  and could not start over TLS while two-process setups worked.
- A node installed by the join command runs the same build as its master.
- The binary is `sandstormd`, not `sandstorm`: the name was taken by the v0.5
  CLI. That crate has left the tree, so renaming is a possible follow-up.
