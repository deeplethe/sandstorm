# Contributing to sandstorm

## Branch workflow

- Open feature, fix, documentation, and dependency pull requests against
  `dev`. GitHub uses `dev` as the repository's default branch.
- Name your branch `feature/<short-name>`, whatever the change is: a fix or a
  documentation change too.
- `main` is the stable release line. Pull requests to `main` are accepted
  only from this repository's `dev` branch.
- Both branches run the full CI suite. A pull request must pass the required
  `rust` check before it can merge.
- Do not force-push or delete `dev` or `main`.

Maintainers promote a tested `dev` revision to `main` with a pull request,
then fast-forward `dev` to the resulting merge commit so the branches do not
accumulate avoidable divergence.

## Commits

Use focused commits with an imperative subject. Sign off every commit for the
[Developer Certificate of Origin](https://developercertificate.org/) with:

```bash
git commit --signoff
```

By signing off, you certify that you have the right to submit the contribution
under the project's license.

## Working on the console

The master's web console is a TypeScript + React app in `web/console`, built
by Vite into one HTML file that `sandstormd` embeds (`crates/sandstorm-core/build.rs`)
and serves at `/`. The built file is not in the repository; CI, releases and
the Docker image build it before cargo does. A `cargo build` without it still
works and serves a page saying the console was not built.

```bash
npm --prefix web/console ci
npm --prefix web/console run build   # type-check, then build; rebuild sandstormd after
npm --prefix web/console run dev     # hot reload, with /v1 proxied to a master on 127.0.0.1:8898
```

`scripts/dev/console-sim.py` gives that master a pretend fleet to look at.

## Which backend an issue is about

Two labels say which sandbox backend an issue is about:

| Label | Path | Status |
| --- | --- | --- |
| `backend:gvisor` | `sandstorm-core` `node/backend.rs` → `runsc` | **Default.** Needs no `/dev/kvm`. |
| `backend:firecracker` | `sandstorm-core` `node/firecracker.rs` (`--backend firecracker`) | Optional, for hosts with KVM. Not yet run on one; see [ADR 11](docs/adr/0011-firecracker-backend.md). The v0.5 Firecracker runtime is not in this tree; it lives in the [`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3). |

gVisor is the default for availability, not preference: Firecracker needs
`/dev/kvm` and most cloud instances do not expose nested virtualisation, so
it cannot be the default for the machines operators actually have. Where KVM
does exist, Firecracker is the stronger isolation boundary —
`docs/SECURITY.md` states what each one gives you. A `backend:firecracker`
issue is about `--backend firecracker` or about the v0.5 code in the tag.

**If you pick up a `backend:firecracker` issue, you need a KVM host of your
own.** None of the three machines sandstorm is developed and measured on has
`/dev/kvm`, so results on that path cannot be reproduced here. That, rather
than difficulty, is why several of those issues have sat still.

An unlabelled issue is either backend-neutral (docs, recipes, SDKs) or not
yet triaged. A `backend:firecracker` label is not by itself a statement that
something is low priority — #317 carries it and is a live data-corruption
bug in v0.5.x.
