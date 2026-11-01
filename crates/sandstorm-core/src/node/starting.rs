//! Memory a node has promised but not yet seen used.
//!
//! The node admits a create on the memory free now. A sandbox's program
//! fills its heap seconds after the sandbox starts, so until it has, free
//! memory says nothing of what is already spoken for. Measured on the
//! fleet's 2 GB nodes with 1 GiB heaps: 900 creates in 2 s, each admitted
//! against a figure the ones before it had not yet touched, put more than
//! three on a node. The nodes went to under 200 MiB and stopped answering
//! ssh, which the admission floor exists to prevent.
//!
//! Reserving each sandbox's limit outright would be the opposite mistake:
//! `web` is allowed 128 MiB and uses 24, and a node that counted the rest
//! would hold a fraction of what it can. So a sandbox is only counted while
//! it is still growing, for what is left of its limit, and let go as soon
//! as it has stopped.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Time between two looks at a sandbox's memory.
const SAMPLE: Duration = Duration::from_millis(300);
/// Growth between two looks that still counts as having stopped.
const FLAT_MIB: u64 = 2;
/// Looks in a row without growth before a sandbox is let go: one could be
/// a program that lost the CPU to the sandbox starting beside it.
const FLAT_LOOKS: u32 = 2;
/// A program still growing this long after it started is simply running.
const WINDOW: Duration = Duration::from_secs(30);
/// What a sandbox with no memory limit is counted for.
const NO_LIMIT_MIB: u64 = 512;

struct Entry {
    started: Instant,
    limit_mib: u64,
    seen_mib: u64,
    seen_at: Instant,
    flat: u32,
    /// Let through and not running yet, so there is no memory to read.
    promised: bool,
}

#[derive(Default)]
pub struct Starting(Mutex<HashMap<String, Entry>>);

impl Starting {
    /// `id` has just started, under `limit_mib` if it has one.
    pub fn add(&self, id: &str, limit_mib: Option<u64>, now: Instant) {
        self.insert(id, limit_mib, now, false);
    }

    /// `id` has been let through and is about to start. Creates run side by
    /// side, so the next one is judged while this one has nothing to show
    /// for itself yet. `add` takes over from this; `forget` undoes it.
    pub fn promise(&self, id: &str, limit_mib: Option<u64>, now: Instant) {
        self.insert(id, limit_mib, now, true);
    }

    /// `id` will not start after all.
    pub fn forget(&self, id: &str) {
        self.0.lock().remove(id);
    }

    fn insert(&self, id: &str, limit_mib: Option<u64>, now: Instant, promised: bool) {
        self.0.lock().insert(
            id.to_string(),
            Entry {
                started: now,
                limit_mib: limit_mib.unwrap_or(NO_LIMIT_MIB),
                seen_mib: 0,
                seen_at: now,
                flat: 0,
                promised,
            },
        );
    }

    /// How much free memory is spoken for: what the sandboxes still growing
    /// can yet take. `used` reads a sandbox's memory, `None` once it is gone.
    pub fn owed_mib(&self, now: Instant, used: impl Fn(&str) -> Option<u64>) -> u64 {
        let mut owed = 0;
        self.0.lock().retain(|id, e| {
            if now.duration_since(e.started) >= WINDOW {
                return false;
            }
            if e.promised {
                owed += e.limit_mib;
                return true;
            }
            let Some(mib) = used(id) else { return false };
            if now.duration_since(e.seen_at) >= SAMPLE {
                e.flat = if mib <= e.seen_mib + FLAT_MIB {
                    e.flat + 1
                } else {
                    0
                };
                e.seen_mib = mib;
                e.seen_at = now;
                if e.flat >= FLAT_LOOKS {
                    return false;
                }
            }
            owed += e.limit_mib.saturating_sub(mib);
            true
        });
        owed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOK: Duration = SAMPLE;

    #[test]
    fn a_sandbox_just_started_is_counted_for_the_rest_of_its_limit() {
        let (s, t) = (Starting::default(), Instant::now());
        s.add("big", Some(1216), t);
        assert_eq!(s.owed_mib(t, |_| Some(16)), 1200);
    }

    /// Nothing can be read of a sandbox that has not started, and reading
    /// nothing is how a sandbox that has gone is let go.
    #[test]
    fn a_sandbox_let_through_is_counted_before_it_has_any_memory() {
        let (s, t) = (Starting::default(), Instant::now());
        s.promise("next", Some(512), t);
        assert_eq!(s.owed_mib(t + LOOK * 3, |_| None), 512);
        s.add("next", Some(512), t + LOOK * 3);
        assert_eq!(s.owed_mib(t + LOOK * 3, |_| Some(12)), 500);
    }

    #[test]
    fn a_sandbox_that_will_not_start_is_no_longer_counted() {
        let (s, t) = (Starting::default(), Instant::now());
        s.promise("next", Some(512), t);
        s.forget("next");
        assert_eq!(s.owed_mib(t, |_| None), 0);
    }

    #[test]
    fn a_heap_being_filled_stays_counted_until_it_stops() {
        let (s, t) = (Starting::default(), Instant::now());
        s.add("big", Some(1216), t);
        assert_eq!(s.owed_mib(t + LOOK, |_| Some(216)), 1000);
        assert_eq!(s.owed_mib(t + LOOK * 2, |_| Some(716)), 500);
        assert_eq!(s.owed_mib(t + LOOK * 3, |_| Some(1116)), 100);
        // Full: one flat look is not enough, two are.
        assert_eq!(s.owed_mib(t + LOOK * 4, |_| Some(1116)), 100);
        assert_eq!(s.owed_mib(t + LOOK * 5, |_| Some(1116)), 0);
        assert_eq!(s.owed_mib(t + LOOK * 6, |_| Some(1)), 0, "and stays gone");
    }

    #[test]
    fn a_small_program_is_let_go_within_a_moment() {
        let (s, t) = (Starting::default(), Instant::now());
        s.add("web", Some(128), t);
        assert_eq!(s.owed_mib(t + LOOK, |_| Some(24)), 104);
        assert_eq!(s.owed_mib(t + LOOK * 2, |_| Some(24)), 104);
        assert_eq!(s.owed_mib(t + LOOK * 3, |_| Some(24)), 0);
    }

    #[test]
    fn looks_closer_together_than_a_sample_do_not_end_it_early() {
        let (s, t) = (Starting::default(), Instant::now());
        s.add("big", Some(1216), t);
        for ms in 1..50 {
            let at = t + Duration::from_millis(ms);
            assert_eq!(s.owed_mib(at, |_| Some(16)), 1200);
        }
    }

    #[test]
    fn a_stall_in_the_middle_does_not_end_it() {
        let (s, t) = (Starting::default(), Instant::now());
        s.add("big", Some(1216), t);
        assert_eq!(s.owed_mib(t + LOOK, |_| Some(100)), 1116);
        assert_eq!(s.owed_mib(t + LOOK * 2, |_| Some(100)), 1116);
        assert_eq!(s.owed_mib(t + LOOK * 3, |_| Some(600)), 616, "it resumed");
        assert_eq!(s.owed_mib(t + LOOK * 4, |_| Some(600)), 616);
        assert_eq!(s.owed_mib(t + LOOK * 5, |_| Some(600)), 0);
    }

    #[test]
    fn what_has_gone_or_run_long_is_not_counted() {
        let (s, t) = (Starting::default(), Instant::now());
        s.add("gone", Some(1216), t);
        assert_eq!(s.owed_mib(t, |_| None), 0);
        s.add("long", Some(1216), t);
        assert_eq!(s.owed_mib(t + WINDOW, |_| Some(16)), 0);
    }

    #[test]
    fn a_sandbox_with_no_limit_is_still_counted() {
        let (s, t) = (Starting::default(), Instant::now());
        s.add("open", None, t);
        assert_eq!(s.owed_mib(t, |_| Some(12)), NO_LIMIT_MIB - 12);
    }
}
