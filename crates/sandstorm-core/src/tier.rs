//! The four lifecycle tiers and the rules for moving between them.
//!
//! Measured on bm (2 vCPU / 3.6 GB, gVisor runsc release-20261019.0,
//! python+numpy workload) on 2026-10-23:
//!
//! | tier     | host RAM | host disk            | back to hot |
//! |----------|----------|----------------------|-------------|
//! | Hot      | 63 MiB   | writable layer       | --          |
//! | Frozen   | same     | writable layer       | 8-11 ms     |
//! | Snapshot | 0        | 50 MiB + layer       | 84 ms       |
//! | Detached | 0        | 0 (volume only)      | 1.7-2.8 s   |
//!
//! Two properties that are easy to get wrong, both load-bearing:
//!   - Frozen does NOT free memory. It shares hot's memory budget;
//!     memory density comes from Snapshot.
//!   - Snapshot does NOT free local disk. The writable layer stays on
//!     the node; disk density comes from Detached.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Where a sandbox is, and therefore what it costs.
/// Ordered coldest-last, so `Frozen < Snapshot` reads as "warmer than".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Running. Serving requests.
    Hot,
    /// `cgroup.freeze=1`. Zero CPU, memory untouched, same node.
    Frozen,
    /// `runsc checkpoint` on local disk. Process gone, writable layer stays.
    Snapshot,
    /// Nothing on any node. Only the volume holds state.
    Detached,
}

impl Tier {
    /// Tiers in demotion order, coldest last.
    pub const ORDER: [Tier; 4] = [Tier::Hot, Tier::Frozen, Tier::Snapshot, Tier::Detached];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Hot => "hot",
            Tier::Frozen => "frozen",
            Tier::Snapshot => "snapshot",
            Tier::Detached => "detached",
        }
    }

    /// The next colder tier, or None at the bottom.
    pub fn colder(self) -> Option<Tier> {
        match self {
            Tier::Hot => Some(Tier::Frozen),
            Tier::Frozen => Some(Tier::Snapshot),
            Tier::Snapshot => Some(Tier::Detached),
            Tier::Detached => None,
        }
    }

    /// Demotion is one step at a time; promotion always jumps to Hot.
    /// Returning the path lets the caller run the legs in order.
    pub fn path_to_hot(self) -> &'static [Tier] {
        match self {
            Tier::Hot => &[],
            Tier::Frozen => &[Tier::Hot],
            Tier::Snapshot => &[Tier::Hot],
            Tier::Detached => &[Tier::Hot],
        }
    }

    /// Does this tier still hold the sandbox's memory on the node?
    pub fn holds_memory(self) -> bool {
        matches!(self, Tier::Hot | Tier::Frozen)
    }

    /// Does this tier still hold the per-child writable layer on the node?
    pub fn holds_local_disk(self) -> bool {
        !matches!(self, Tier::Detached)
    }

    /// Can this sandbox serve a request without any tier change?
    pub fn is_serving(self) -> bool {
        matches!(self, Tier::Hot)
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Tier {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "hot" => Tier::Hot,
            "frozen" => Tier::Frozen,
            "snapshot" => Tier::Snapshot,
            "detached" => Tier::Detached,
            other => anyhow::bail!("unknown tier: {other}"),
        })
    }
}

/// How long a sandbox may sit idle before dropping a tier.
///
/// Idle means "no request completed AND none in flight" -- a long
/// streaming call keeps the sandbox hot. Getting this wrong is what
/// makes CubeSandbox pause a sandbox mid-`commands.run`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct IdlePolicy {
    pub to_frozen: Duration,
    pub to_snapshot: Duration,
    pub to_detached: Duration,
}

impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            to_frozen: Duration::from_secs(30),
            to_snapshot: Duration::from_secs(5 * 60),
            to_detached: Duration::from_secs(60 * 60),
        }
    }
}

impl IdlePolicy {
    /// The tier this sandbox should be in after sitting idle this long,
    /// ignoring node pressure. Pressure can demote further; nothing
    /// here ever promotes.
    pub fn tier_for_idle(&self, idle: Duration) -> Tier {
        if idle >= self.to_detached {
            Tier::Detached
        } else if idle >= self.to_snapshot {
            Tier::Snapshot
        } else if idle >= self.to_frozen {
            Tier::Frozen
        } else {
            Tier::Hot
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demotion_is_one_step_at_a_time() {
        assert_eq!(Tier::Hot.colder(), Some(Tier::Frozen));
        assert_eq!(Tier::Frozen.colder(), Some(Tier::Snapshot));
        assert_eq!(Tier::Snapshot.colder(), Some(Tier::Detached));
        assert_eq!(Tier::Detached.colder(), None);
    }

    #[test]
    fn promotion_always_jumps_straight_to_hot() {
        for t in [Tier::Frozen, Tier::Snapshot, Tier::Detached] {
            assert_eq!(t.path_to_hot(), &[Tier::Hot]);
        }
        assert!(Tier::Hot.path_to_hot().is_empty());
    }

    /// The two properties the density math depends on.
    #[test]
    fn frozen_keeps_memory_snapshot_keeps_disk() {
        assert!(
            Tier::Frozen.holds_memory(),
            "frozen shares hot's memory budget"
        );
        assert!(
            !Tier::Snapshot.holds_memory(),
            "memory density comes from snapshot"
        );
        assert!(
            Tier::Snapshot.holds_local_disk(),
            "writable layer stays on the node"
        );
        assert!(
            !Tier::Detached.holds_local_disk(),
            "disk density comes from detached"
        );
    }

    #[test]
    fn idle_policy_picks_the_right_tier() {
        let p = IdlePolicy::default();
        assert_eq!(p.tier_for_idle(Duration::from_secs(5)), Tier::Hot);
        assert_eq!(p.tier_for_idle(Duration::from_secs(45)), Tier::Frozen);
        assert_eq!(
            p.tier_for_idle(Duration::from_secs(10 * 60)),
            Tier::Snapshot
        );
        assert_eq!(
            p.tier_for_idle(Duration::from_secs(2 * 3600)),
            Tier::Detached
        );
    }

    #[test]
    fn tiers_order_coldest_last() {
        assert!(Tier::Hot < Tier::Frozen);
        assert!(Tier::Frozen < Tier::Snapshot);
        assert!(Tier::Snapshot < Tier::Detached);
    }

    #[test]
    fn tier_round_trips_through_strings() {
        for t in Tier::ORDER {
            assert_eq!(t.as_str().parse::<Tier>().unwrap(), t);
        }
        assert!("warm".parse::<Tier>().is_err());
    }
}
