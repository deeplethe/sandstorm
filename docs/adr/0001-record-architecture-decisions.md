# 1. Record architecture decisions

Date: 2026-10-28

## Status

Accepted

## Context

Sandstorm's design lived in long documents at the repository root, one per
release: `DESIGN.md`, five `DESIGN-v0.4*.md` files,
`DESIGN-v0.5-diff-snapshot-chains.md`, `DESIGN-v0.6.md`, and three notes
under `docs/design/`. All but the last described the Firecracker runtime,
which is no longer in the tree. `DESIGN-v0.6.md` was an RFC written before
most of v0.6 was built, so it mixed decisions that were made with proposals
that were later dropped or changed (a `cold` tier, PostgreSQL for the
master, gRPC between master and node). A reader could not tell which parts
still held without reading the code.

## Decision

Architecture decisions are recorded as ADRs in `docs/adr/`, one decision per
file, named `NNNN-short-title.md`, with the sections Status, Context,
Decision and Consequences. [`README.md`](README.md) is the index.

An ADR records a decision that was made and is reflected in the code. A
proposal that has not been built does not get one. When a decision changes,
the old ADR is marked `Superseded by NNNN` and a new one is written; an
accepted ADR is not rewritten to say something else.

The root `DESIGN*.md` files and `docs/design/` are removed. The Firecracker
design notes are in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3), and the
last version of the v0.6 RFC is at
[`7b39861`](https://github.com/deeplethe/sandstorm/blob/7b3986150b71feeece318ce1b4475f3c291d3aae/DESIGN-v0.6.md)
(Chinese).

## Consequences

- Each ADR is short and says only what holds for the code on `dev`.
- Measurements and the reasoning behind rejected options stay where they
  were recorded: the CHANGELOG, `bench/fleet/results/`, and the RFC at the
  commit above. ADRs link to them rather than copy them.
- Code comments that cite RFC sections (`RFC §15.5`, `RFC 16.5`) now point
  at a document that is only in history. They still read correctly against
  the permalink above.
