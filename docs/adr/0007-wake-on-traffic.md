# 7. The node's gateway wakes a sandbox when a request reaches it

Date: 2026-10-28

## Status

Accepted

## Context

A sandbox that has been frozen or put to disk has to come back when someone
calls it. Before this decision its exposed port answered 503 and told the
caller to `POST /v1/sandboxes/{id}/touch` to the master and try again. Every
client then had to know about tiers, and the density figures came with an
asterisk: a demoted sandbox was available only to callers who knew to wake
it.

Three requirements constrain where that can happen:

- each sandbox has a 128-bit access token, required on any of its ports;
- a wake is latency the caller sees, so it must not travel through the
  master;
- the master owns desired state, so a node that wakes a sandbox on its own
  must not then be undone by a demotion issued from a stale view.

## Decision

- **Each exposed port is served by a gateway on the sandbox's node**
  (`node/gateway.rs`). It checks the `x-sandstorm-sandbox-token` header in
  constant time, does not forward the credential, and proxies to the sandbox
  inside its namespace. A plain DNAT has nowhere to read a token, and the
  master is not on the data path, so the gateway can be neither.
- **A connection refused means the sandbox is cold.** A sandbox that is not
  hot rejects connections with a TCP reset, so the gateway holds no tier
  state: on connection refused it wakes the sandbox on this node
  (`node/wake.rs`) and forwards again. The caller sees a slower response,
  not an error. A response that had to wait for a wake says so in a header.
- **Nothing goes through the master.** The node reports each request back
  (`traffic` on its next report); the master then moves the sandbox's idle
  clock and makes hot its desired tier. Until that report lands, the node
  defers a demotion of a sandbox it just woke.
- A wake is subject to the same admission checks as a promotion the master
  asks for ([ADR 8](0008-nodes-protect-themselves.md)), and makes room when
  the node is short ([ADR 9](0009-wake-makes-room.md)).

## Consequences

- Clients of an exposed port do not need to know tiers exist.
- The exposed port is HTTP in v0.6, since a raw TCP stream has nowhere to
  carry the token. Exposing a non-HTTP port needs another credential
  mechanism and is not built.
- The port and token are stored in the sandbox's bundle (mode 0600), so a
  node restart exposes the sandbox again with its check intact.
- A wake that cannot be done is answered 503 with `retry-after`.
