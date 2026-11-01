//! Squeezing the cold pages of hot sandboxes, gently.
//!
//! Frozen reclaim (`runtime::reclaim_frozen`) takes everything a sandbox
//! holds once it has gone idle. This is the other half: a sandbox that is
//! serving still has pages it has not touched in minutes -- imports done
//! once at startup, a cache filled and never read again -- and those can go
//! to compressed swap while it keeps running.
//!
//! It is the shape of Meta's Senpai / TMO: ask the kernel to reclaim a
//! small fraction of the cgroup, and let the cgroup's own memory-pressure
//! stall time say whether that was too much. A sandbox that stalled on
//! memory in the last interval is left alone and its step reset; one that
//! did not gets a slightly larger step next time. Pages that come back are
//! decompressed from RAM, which is what keeps this cheap enough to run on
//! sandboxes that are answering requests.
//!
//! Only where swap is compressed RAM (zswap or zram), for the same reason
//! as frozen reclaim, and with zswap writeback off so nothing goes to disk.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use super::runtime::Runtime;
use crate::tier::Tier;

/// How often each hot sandbox is looked at.
pub const EVERY: Duration = Duration::from_secs(6);
/// Memory stall time a sandbox may accrue in one interval and still be
/// squeezed further: 0.1% of the interval, Senpai's default target.
pub const STALL_BUDGET_US: u64 = 6_000;
/// The step, as a fraction of what the sandbox holds. Starts small, doubles
/// each quiet interval, and drops back to the start on any stall.
pub const STEP_MIN: f64 = 0.002;
pub const STEP_MAX: f64 = 0.02;
/// Hot reclaim stops for a pass while the host's memory pressure is above
/// this share of the admission ceiling (`--admit-max-psi`).
///
/// Admission reads the host's `some` average, and pages this squeezes out
/// and the sandbox then faults back count towards it. Each sandbox is held
/// to 0.1% of its own time, but `some` is "any task stalled", so across
/// hundreds of sandboxes those add up. Backing off at half the ceiling
/// keeps the other half for real demand: a pass of reclaim must never be
/// the reason a node refuses a sandbox.
pub const PSI_HEADROOM: f64 = 0.5;

/// Not worth a syscall below this.
const ASK_MIN_BYTES: u64 = 1 << 20;

/// What one sandbox looked like at the last interval.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    stall_total_us: u64,
    step: f64,
}

/// The decision for one sandbox in one interval: how many bytes to ask the
/// kernel to reclaim, if any, and what to remember for next time.
pub fn decide(prev: Option<Probe>, stall_total_us: u64, current_bytes: u64) -> (u64, Probe) {
    let Some(prev) = prev else {
        // First sight: nothing to compare the stall counter with yet.
        return (
            0,
            Probe {
                stall_total_us,
                step: STEP_MIN,
            },
        );
    };
    let stalled = stall_total_us.saturating_sub(prev.stall_total_us);
    if stalled > STALL_BUDGET_US {
        return (
            0,
            Probe {
                stall_total_us,
                step: STEP_MIN,
            },
        );
    }
    let ask = (current_bytes as f64 * prev.step) as u64;
    let next = Probe {
        stall_total_us,
        step: (prev.step * 2.0).min(STEP_MAX),
    };
    if ask < ASK_MIN_BYTES {
        (0, next)
    } else {
        (ask, next)
    }
}

/// Cumulative microseconds some task in this cgroup spent stalled on memory.
fn stall_total_us(cg: &Path) -> Option<u64> {
    let s = std::fs::read_to_string(cg.join("memory.pressure")).ok()?;
    s.lines()
        .find(|l| l.starts_with("some"))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("total=")?.parse().ok())
}

fn current_bytes(cg: &Path) -> Option<u64> {
    std::fs::read_to_string(cg.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Run for the life of the node.
pub fn start(rt: Runtime, admit_max_psi: f64) {
    let spawned = std::thread::Builder::new()
        .name("sandstorm-hot-reclaim".into())
        .spawn(move || {
            let mut probes: HashMap<String, Probe> = HashMap::new();
            loop {
                std::thread::sleep(EVERY);
                let psi = super::runtime::host_pressure().1;
                if psi > admit_max_psi * PSI_HEADROOM {
                    for p in probes.values_mut() {
                        p.step = STEP_MIN;
                    }
                    tracing::debug!(psi, "host under pressure; hot reclaim sits this pass out");
                    continue;
                }
                // Every page squeezed out takes a swap slot, compressed or
                // not; see `runtime::host_reading`.
                if super::runtime::host_reading().swap_short(rt.reclaim_floor_mib()) {
                    for p in probes.values_mut() {
                        p.step = STEP_MIN;
                    }
                    tracing::debug!("swap under its floor; hot reclaim sits this pass out");
                    continue;
                }
                let Ok(observed) = rt.observe() else {
                    continue;
                };
                probes.retain(|id, _| observed.iter().any(|o| &o.id == id && o.tier == Tier::Hot));
                let mut asked = 0u64;
                for o in observed.iter().filter(|o| o.tier == Tier::Hot) {
                    let cg = rt.cgroup_dir(&o.id);
                    let (Some(stall), Some(cur)) = (stall_total_us(&cg), current_bytes(&cg)) else {
                        continue;
                    };
                    let prev = probes.get(&o.id).copied();
                    if prev.is_none() {
                        // Compressed RAM only; see the module comment.
                        super::runtime::route_reclaim(&cg, rt.pins_in_zswap());
                    }
                    let (ask, next) = decide(prev, stall, cur);
                    probes.insert(o.id.clone(), next);
                    // EAGAIN when it could not take all of it: fine, it
                    // took what it could.
                    if ask > 0 && std::fs::write(cg.join("memory.reclaim"), ask.to_string()).is_ok()
                    {
                        asked += ask;
                    }
                }
                if asked > 0 {
                    tracing::debug!(asked_mib = asked >> 20, "hot reclaim pass");
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start hot reclaim; carrying on without it");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn first_sight_only_takes_a_reading() {
        let (ask, p) = decide(None, 123, 500 * MIB);
        assert_eq!(ask, 0);
        assert_eq!(p.stall_total_us, 123);
    }

    #[test]
    fn a_quiet_sandbox_is_squeezed_in_growing_steps() {
        let (_, p) = decide(None, 0, 500 * MIB);
        let (a1, p) = decide(Some(p), 100, 500 * MIB);
        let (a2, p) = decide(Some(p), 200, 500 * MIB);
        assert_eq!(a1, (500.0 * MIB as f64 * STEP_MIN) as u64);
        assert_eq!(a2, 2 * a1);
        // And the step stops growing at the ceiling.
        let mut p = p;
        for _ in 0..10 {
            p = decide(Some(p), p.stall_total_us, 500 * MIB).1;
        }
        assert_eq!(p.step, STEP_MAX);
    }

    #[test]
    fn a_stall_stops_it_and_resets_the_step() {
        let (_, p) = decide(None, 0, 500 * MIB);
        let (_, p) = decide(Some(p), 0, 500 * MIB);
        let (_, p) = decide(Some(p), 0, 500 * MIB);
        assert!(p.step > STEP_MIN);
        let (ask, p) = decide(Some(p), STALL_BUDGET_US + 1, 500 * MIB);
        assert_eq!(ask, 0, "it stalled: leave it alone");
        assert_eq!(p.step, STEP_MIN);
    }

    #[test]
    fn small_sandboxes_are_not_worth_a_syscall() {
        let (_, p) = decide(None, 0, 100 * MIB);
        let (ask, _) = decide(Some(p), 0, 100 * MIB);
        assert_eq!(ask, 0, "0.2% of 100 MiB is under a MiB");
    }

    #[test]
    fn the_pressure_file_parses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("memory.pressure"),
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=4242\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=17\n",
        )
        .unwrap();
        assert_eq!(stall_total_us(dir.path()), Some(4242));
    }
}
