# `scripts/`

| Script | Purpose |
|---|---|
| `fleet.sh` | Stand a fleet up across several machines, seed a template, verify, take it down |
| `install-guest-kernel.sh` / `build-guest-kernel.sh` | Fetch or build the guest vmlinux that `--backend firecracker` boots (`--fc-kernel`) |
| `dev/` | End-to-end checks against a running `sandstormd`, and `console-sim.py` for working on the console |

The v0.5 host setup, rootfs and network scripts are in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3/scripts).
