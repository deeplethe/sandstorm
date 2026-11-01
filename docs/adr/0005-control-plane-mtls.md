# 5. The control plane runs on mutual TLS, in either direction

Date: 2026-10-28

## Status

Accepted

## Context

The master and its nodes exchange registration tokens, the API token and
every command run inside a sandbox, often over the public internet. A node
has to prove it is a registered node, or anyone could poll as one and be
handed its sandboxes. The master has to prove it is the real master, or
anyone who can reach a node could pose as it.

Nodes are reached in two ways. Some sit behind NAT where the master cannot
connect to them. Others are already installed on a network the master can
reach, and asking someone to run a join command on each is the slow part of
adding them.

## Decision

- **mTLS off a Sandstorm-owned CA** (`identity.rs`, `tls.rs`). The master
  holds the CA and its own server certificate; each node gets a client
  certificate when it registers. Both ends verify the other. A node is
  identified by its certificate, not by what its messages claim: the master
  records each issued certificate's SHA-256 and resolves every connection
  through it.
- **Registration is single-use and pinned.** A registration token is
  `<CA fingerprint>.<secret>`. The node pins that CA on first contact, so the
  token is never sent to a master that does not hold it, and the token is
  spent once a certificate is issued. `POST /v1/node/revoke/:id` stops a
  node.
- **Two connection directions, one protocol** (`proto.rs`):
  - *pull*: the node dials the master and holds a request open for work. It
    needs no inbound port, which is the case for a node behind NAT.
  - *listen*: the node waits on a port with a pairing code, and the master
    connects to it (`master/drive.rs`). The pairing code pins the node's
    certificate.

  Either way the master runs the same steps, so a sandbox behaves the same
  whichever way its node is connected.
- **Commands are not signed one by one.** mTLS already authenticates both
  ends of the channel; per-message signatures would be a second identity
  system to keep correct for no extra guarantee.
- `--insecure-transport` turns TLS and the client-certificate check off, for
  a single box on loopback only, and logs what it gives up.

## Consequences

- A pull node needs no inbound port; a compromised master cannot reach a
  pull node that never called in.
- A listen node does accept connections, so it depends on its pairing code
  and on the network it is placed in.
- The transport is HTTPS (axum and rustls), not the gRPC stream the RFC
  proposed.
- The bearer token for the API and the per-sandbox access tokens are separate
  credentials from the node certificates; a leaked sandbox token gives no
  reach into the control plane.
