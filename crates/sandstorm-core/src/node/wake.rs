//! Waking a sandbox because a request arrived for it.
//!
//! Without this, a frozen or snapshotted sandbox answers its gateway port
//! with a 503 telling the caller to POST `/touch` to the master and try
//! again. That makes every client of an exposed port aware of tiers, and it
//! is the reason the density figures came with an asterisk: a sandbox that
//! has been demoted is only "available" to callers who know to wake it.
//!
//! Here the gateway wakes it instead. A forward that fails with connection
//! refused -- #327 makes a cold sandbox refuse at once rather than hang --
//! wakes the sandbox on this node and forwards again, and the caller sees a
//! slow response rather than an error. Nothing goes through the master,
//! which RFC §15.5 requires of a wake ("唤醒请求绝不经过 master").
//!
//! The master still owns desired state, so two things keep it and the node
//! from fighting over a sandbox that was just woken:
//!
//! - every request is reported back (`PollRequest::traffic`), and the
//!   master moves the idle clock and makes Hot the desired tier;
//! - until it has, the node will not demote a sandbox that served a
//!   request in the last `GRACE`, so a demotion the master issued from a
//!   stale view is deferred rather than undoing the wake.
//!
//! The node is the one that pays for a wake, so it is subject to the same
//! admission check as a promotion the master asks for -- and, when the
//! node is under its memory floor, it makes room by putting the
//! longest-idle sandboxes here to disk (`victims`) rather than refusing.

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::proto::ObservedSandbox;
use crate::tier::Tier;

/// How long a sandbox that has just come up -- created, or restored for the
/// master -- is spared from being put away to make room, so that the
/// request it came up for has time to reach it. Past that, only a request
/// in flight keeps a sandbox from being put away.
///
/// This used to be how long ANY sandbox that had served a request was
/// spared, a stand-in for "in use". On 2026-10-28, at 80 per node and
/// 8 req/s, it spared nearly every hot sandbox at once -- one that had
/// answered its only request a second ago as much as a busy one -- and 26%
/// of requests were refused for want of anything to put away.
pub const JUST_STARTED: Duration = Duration::from_secs(2);

/// How long a wake may take to start before it is refused: waiting for a
/// restore permit, then for memory pressure to ease. What is left of it
/// when the wake gets its permit is how long it may wait out pressure.
///
/// A caller is better served by a quick 503 with `retry-after` than by a
/// long wait. On 2026-10-28, at 80 per node and 8 req/s, wakes queued with
/// no bound: refusals fell to 14%, but the answered requests' p50 went from
/// 13 ms to 4.6 s, a popular sandbox's wake waiting 12 s behind the wakes
/// of sandboxes called once. Before that, waiting out pressure for up to
/// 8 s on its own had saved 8 of 600 wakes from being refused for pressure
/// their own restores caused; most of that is inside this.
pub const WAKE_BUDGET: Duration = Duration::from_secs(2);

/// The time constant of a sandbox's demand: each request adds one, and the
/// total decays by `e` every this long. Ranks how frequently and how
/// recently a sandbox is asked for in one number, so a wake puts away the
/// sandbox least likely to be asked for next: one called once before one
/// called often, and of two called once, the one called longer ago.
pub const DEMAND_WINDOW: Duration = Duration::from_secs(30);

/// How long after a request a sandbox is safe from demotion.
///
/// Long enough to cover the master hearing about the request: a listening
/// node reports every 5 s, and a polling node reports as soon as a deferred
/// command shortens its hold. The master's idle thresholds are 30 s and up,
/// so this never decides on its own when a sandbox goes cold.
pub const GRACE: Duration = Duration::from_secs(15);

/// Predicting the next request, to have a snapshot image in the page cache
/// before it arrives.
///
/// A cold restore is a disk read: on bm 1.4 s for a compressed image that
/// restores in 0.35-0.7 s from cache. Plenty of sandboxes are called on a
/// schedule -- an agent polled every few minutes, a cron-driven job -- and
/// for those the next arrival is easy to guess. Reading the image shortly
/// before costs a read the restore would have done anyway; guessing wrong
/// costs one read and nothing else.
pub mod predict {
    use std::time::Duration;
    /// Arrivals kept per sandbox.
    pub const HISTORY: usize = 8;
    /// Intervals looked at; all of them must agree.
    pub const INTERVALS: usize = 3;
    /// How far apart the longest and shortest interval may be.
    pub const JITTER: f64 = 1.25;
    /// Shorter periods than this never let a sandbox reach Snapshot.
    pub const MIN_PERIOD: Duration = Duration::from_secs(30);
    /// How long before the predicted arrival to start reading.
    pub const LEAD: Duration = Duration::from_secs(20);
}

/// Brings a sandbox to Hot, waiting out memory pressure for at most the
/// `Duration` it is given.
type WakeFn = dyn Fn(&Wakeups, &str, Duration) -> Result<()> + Send + Sync;

pub struct Wakeups {
    /// One per sandbox, held across any tier change -- a wake, or a command
    /// from the master -- so the two never run runsc on one sandbox at once.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// When each sandbox last had a request through the gateway.
    seen: Mutex<HashMap<String, Instant>>,
    /// Requests the master has not been told about yet.
    unreported: Mutex<HashSet<String>>,
    /// When each sandbox last served a request, or came up. Unlike `seen`
    /// it is kept for the sandbox's life: it is how a wake that needs
    /// memory picks which idle sandbox to put away.
    used: Mutex<HashMap<String, Instant>>,
    /// Requests through the gateway not yet answered. A sandbox with any is
    /// never put away to make room.
    in_flight: Mutex<HashMap<String, usize>>,
    /// When each sandbox came up without a request: created, or restored
    /// for the master. See `JUST_STARTED`.
    fresh: Mutex<HashMap<String, Instant>>,
    /// Each sandbox's decaying request count, and when it was last brought
    /// up to date. See `DEMAND_WINDOW`.
    demand: Mutex<HashMap<String, (f64, Instant)>>,
    /// Sandboxes put to disk here to make room for a wake, and when.
    evicted: Mutex<HashMap<String, Instant>>,
    /// Restores read whole images from disk, so a burst of wakes for
    /// different sandboxes is queued rather than run all at once and left
    /// to fight over the same disk. A permit belongs to the wake, not to
    /// whoever asked for it; see `wake`.
    restores: Arc<tokio::sync::Semaphore>,
    wake: Box<WakeFn>,
    /// Signalled after every wake, so a polling node can tell the master at
    /// once rather than on its next poll.
    pub woke: tokio::sync::Notify,
    /// When each burst of traffic began: the first request after at least
    /// `GRACE` of quiet. What `due_prefetch` predicts from.
    arrivals: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// The arrival a prefetch was last done after, so one is done per cycle.
    prefetched: Mutex<HashMap<String, Instant>>,
    /// See `WAKE_BUDGET`.
    budget: Duration,
    /// How many wakes may wait for a restore permit at once. One that would
    /// be further back is refused at once: it would not start in time.
    max_waiting: usize,
    waiting: AtomicUsize,
}

impl Wakeups {
    /// `wake` brings a sandbox to Hot and returns when it is serving. It
    /// blocks, and is called with that sandbox's lock held and how long it
    /// may wait out memory pressure.
    pub fn new(
        wake: impl Fn(&Wakeups, &str, Duration) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        let parallel = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .max(2);
        Self {
            locks: Mutex::default(),
            seen: Mutex::default(),
            unreported: Mutex::default(),
            used: Mutex::default(),
            in_flight: Mutex::default(),
            fresh: Mutex::default(),
            demand: Mutex::default(),
            evicted: Mutex::default(),
            restores: Arc::new(tokio::sync::Semaphore::new(parallel)),
            wake: Box::new(wake),
            woke: tokio::sync::Notify::new(),
            arrivals: Mutex::default(),
            prefetched: Mutex::default(),
            budget: WAKE_BUDGET,
            // Two each: about as many as can start within the budget when
            // a restore takes half a second to a second.
            max_waiting: 2 * parallel,
            waiting: AtomicUsize::new(0),
        }
    }

    /// Other limits than `WAKE_BUDGET` and twice the restore permits.
    pub fn with_limits(mut self, budget: Duration, max_waiting: Option<usize>) -> Self {
        self.budget = budget;
        if let Some(n) = max_waiting {
            self.max_waiting = n;
        }
        self
    }

    /// The lock for one sandbox's tier.
    pub fn lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.locks.lock().entry(id.to_string()).or_default().clone()
    }

    /// A request for this sandbox reached the gateway.
    pub fn saw(&self, id: &str) {
        self.saw_at(id, Instant::now());
    }

    pub(super) fn saw_at(&self, id: &str, now: Instant) {
        let mut demand = self.demand.lock();
        let d = demand.entry(id.to_string()).or_insert((0.0, now));
        *d = (decayed(*d, now) + 1.0, now);
        drop(demand);
        let prev = self.seen.lock().insert(id.to_string(), now);
        self.used.lock().insert(id.to_string(), now);
        self.unreported.lock().insert(id.to_string());
        // `seen` forgets anything older than GRACE, so no entry also means
        // a quiet spell.
        if prev.is_none_or(|p| now.saturating_duration_since(p) >= GRACE) {
            self.arrived(id, now);
        }
    }

    fn arrived(&self, id: &str, at: Instant) {
        let mut all = self.arrivals.lock();
        let h = all.entry(id.to_string()).or_default();
        h.push_back(at);
        while h.len() > predict::HISTORY {
            h.pop_front();
        }
    }

    /// Sandboxes whose next burst of traffic is due within `predict::LEAD`,
    /// each returned once per cycle.
    pub fn due_prefetch(&self, now: Instant) -> Vec<String> {
        use predict::*;
        let all = self.arrivals.lock();
        let mut done = self.prefetched.lock();
        let mut due = Vec::new();
        for (id, h) in all.iter() {
            if h.len() < INTERVALS + 1 {
                continue;
            }
            let recent: Vec<Instant> = h.iter().rev().take(INTERVALS + 1).copied().collect();
            let gaps: Vec<Duration> = recent.windows(2).map(|w| w[0] - w[1]).collect();
            let (lo, hi) = (gaps.iter().min().unwrap(), gaps.iter().max().unwrap());
            if *lo < MIN_PERIOD || hi.as_secs_f64() > lo.as_secs_f64() * JITTER {
                continue;
            }
            let period = gaps.iter().sum::<Duration>() / gaps.len() as u32;
            let last = recent[0];
            let next = last + period;
            // Inside the lead window, and not so late that the arrival was
            // plainly missed.
            if now + LEAD < next || now > next + period / 2 {
                continue;
            }
            if done.get(id) == Some(&last) {
                continue;
            }
            done.insert(id.clone(), last);
            due.push(id.clone());
        }
        due.sort();
        due
    }

    /// How much longer this sandbox is protected from demotion, if at all.
    pub fn in_grace(&self, id: &str, now: Instant) -> Option<Duration> {
        let at = *self.seen.lock().get(id)?;
        GRACE
            .checked_sub(now.saturating_duration_since(at))
            .filter(|d| !d.is_zero())
    }

    /// The sandboxes to report as used, emptied as it is read.
    pub fn take_traffic(&self) -> Vec<String> {
        let now = Instant::now();
        // Pruned here because this runs on every report: nothing outside
        // the grace window is consulted again.
        self.seen
            .lock()
            .retain(|_, at| now.saturating_duration_since(*at) < GRACE);
        let mut ids: Vec<String> = self.unreported.lock().drain().collect();
        ids.sort();
        ids
    }

    /// A sandbox just woke for a request, or just answered one. It counts
    /// as used from now -- not from when the request arrived, which under
    /// load can be half a minute earlier -- both for making room and for
    /// the master's demotions.
    pub fn in_use(&self, id: &str) {
        let now = Instant::now();
        self.seen.lock().insert(id.to_string(), now);
        self.used.lock().insert(id.to_string(), now);
    }

    /// A sandbox came up here with no request yet: created, or restored for
    /// the master. Spared for `JUST_STARTED`.
    pub fn started(&self, id: &str) {
        let now = Instant::now();
        self.used.lock().insert(id.to_string(), now);
        self.fresh.lock().insert(id.to_string(), now);
    }

    /// A request for this sandbox is on its way through the gateway. Until
    /// the returned guard is dropped -- once the answer is in hand, or the
    /// caller has gone -- the sandbox is not put away to make room.
    pub fn hold(self: &Arc<Self>, id: &str) -> InFlight {
        // Serialize enrollment with the tier-change check, then let the
        // request run without holding this lock (a wake needs it too).
        let lock = self.lock(id);
        let _held = lock.lock();
        *self.in_flight.lock().entry(id.to_string()).or_default() += 1;
        InFlight {
            wake: self.clone(),
            id: id.to_string(),
        }
    }

    /// Whether this sandbox may not be put away to make room: a request is
    /// in flight, or it has only just come up.
    pub fn protected(&self, id: &str, now: Instant) -> bool {
        self.in_flight.lock().get(id).is_some_and(|n| *n > 0)
            || self
                .fresh
                .lock()
                .get(id)
                .is_some_and(|at| now.saturating_duration_since(*at) < JUST_STARTED)
    }

    /// Which sandboxes a wake may put to disk to make room, best first.
    ///
    /// Frozen before Hot -- a frozen one has already been idle long enough
    /// for the master to say so -- and within each, the one least in demand
    /// (`DEMAND_WINDOW`), then the one used longest ago; one with no record
    /// (adopted across a restart) comes first. Never `except`, the sandbox
    /// being woken, and never one that is `protected`.
    pub fn victims(&self, observed: &[ObservedSandbox], except: &str, now: Instant) -> Vec<String> {
        let candidates: Vec<&ObservedSandbox> = observed
            .iter()
            .filter(|o| o.id != except && matches!(o.tier, Tier::Hot | Tier::Frozen))
            .filter(|o| !self.protected(&o.id, now))
            .collect();
        let used = self.used.lock();
        let demand = self.demand.lock();
        let mut ranked: Vec<(bool, f64, Option<Instant>, &str)> = candidates
            .into_iter()
            .map(|o| {
                let wanted = demand.get(&o.id).map_or(0.0, |d| decayed(*d, now));
                (
                    o.tier == Tier::Hot,
                    wanted,
                    used.get(&o.id).copied(),
                    o.id.as_str(),
                )
            })
            .collect();
        ranked.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.total_cmp(&b.1))
                .then(a.2.cmp(&b.2))
                .then(a.3.cmp(b.3))
        });
        ranked
            .into_iter()
            .map(|(_, _, _, id)| id.to_string())
            .collect()
    }

    /// This node put a sandbox to disk to make room, which the master has
    /// to hear about.
    pub fn put_away(&self, id: &str) {
        self.evicted.lock().insert(id.to_string(), Instant::now());
    }

    /// The sandboxes to report as put away: every one inside its `GRACE`,
    /// on every report, not just the next one.
    ///
    /// A report's observed tiers and this list are not read at the same
    /// instant. Found on bm: a wake put a sandbox away between the two, so
    /// the one report that listed it still observed it frozen, the master
    /// ignored it as nothing new, and its next diff restored the sandbox
    /// 0.3 s after it went to disk. Repeating the list until a report has
    /// caught up with it is safe because the master only ever moves desired
    /// state colder for it.
    pub fn recently_evicted(&self, now: Instant) -> Vec<String> {
        let mut all = self.evicted.lock();
        all.retain(|_, at| now.saturating_duration_since(*at) < GRACE);
        let mut ids: Vec<String> = all.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// How much longer a master command that would bring this sandbox back
    /// is put off, if at all: until the master has heard it was put away.
    pub fn evicted_for(&self, id: &str, now: Instant) -> Option<Duration> {
        let at = *self.evicted.lock().get(id)?;
        GRACE
            .checked_sub(now.saturating_duration_since(at))
            .filter(|d| !d.is_zero())
    }

    /// A destroyed sandbox needs none of this any more.
    pub fn forget(&self, id: &str) {
        self.locks.lock().remove(id);
        self.seen.lock().remove(id);
        self.used.lock().remove(id);
        self.fresh.lock().remove(id);
        self.demand.lock().remove(id);
        self.evicted.lock().remove(id);
        self.unreported.lock().remove(id);
        self.arrivals.lock().remove(id);
        self.prefetched.lock().remove(id);
    }

    /// Wake a sandbox and wait until it is serving. A sandbox that is
    /// already hot costs one observe: `set_tier` returns at once when the
    /// tier already matches, which is also what makes a burst of requests
    /// for one cold sandbox wake it once rather than once each.
    ///
    /// A caller can give up -- the gateway's handler is dropped when its
    /// client disconnects -- but the blocking work cannot be called back,
    /// so the permit goes with the work and is released when it ends. A
    /// caller that gives up while still queued never starts anything.
    ///
    /// Refused, as held back, if it cannot start within the budget: at once
    /// when `max_waiting` are already waiting, or once the budget has gone
    /// waiting for a permit.
    pub async fn wake(self: &Arc<Self>, id: &str) -> Result<Duration> {
        let deadline = Instant::now() + self.budget;
        // Only a wake that has to queue counts as waiting. Counting every
        // wake on the way in let one that was about to take a free permit
        // push another over `max_waiting`, refusing a wake that would have
        // started at once.
        let permit = match self.restores.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(tokio::sync::TryAcquireError::Closed) => bail!("shutting down"),
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                let ahead = self.waiting.fetch_add(1, Ordering::SeqCst);
                let _waiting = Waiting(&self.waiting);
                if ahead >= self.max_waiting {
                    return Err(super::HeldBack(format!(
                        "node held it back: {ahead} wakes already waiting to restore, at most {}",
                        self.max_waiting
                    ))
                    .into());
                }
                match tokio::time::timeout_at(
                    deadline.into(),
                    self.restores.clone().acquire_owned(),
                )
                .await
                {
                    Ok(p) => p.context("shutting down")?,
                    Err(_) => {
                        return Err(super::HeldBack(format!(
                            "node held it back: no restore could start within {:?}",
                            self.budget
                        ))
                        .into())
                    }
                }
            }
        };
        let me = self.clone();
        let id = id.to_string();
        let me2 = self.clone();
        let took = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let lock = me.lock(&id);
            let _held = lock.lock();
            let t0 = Instant::now();
            (me.wake)(&me, &id, deadline.saturating_duration_since(t0))?;
            // Before the lock is let go, so a wake making room cannot take
            // this one between its coming up and its counting as used.
            me.in_use(&id);
            Ok::<_, anyhow::Error>(t0.elapsed())
        })
        .await
        .context("the wake task panicked")??;
        me2.woke.notify_one();
        Ok(took)
    }
}

/// One wake waiting for a restore permit, counted until it has one or its
/// caller has gone.
struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A request in flight to a sandbox; see `Wakeups::hold`.
pub struct InFlight {
    wake: Arc<Wakeups>,
    id: String,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut all = self.wake.in_flight.lock();
        if let Some(n) = all.get_mut(&self.id) {
            *n -= 1;
            if *n == 0 {
                all.remove(&self.id);
            }
        }
    }
}

/// A demand count brought forward to `now`.
fn decayed((count, at): (f64, Instant), now: Instant) -> f64 {
    let dt = now.saturating_duration_since(at).as_secs_f64();
    count * (-dt / DEMAND_WINDOW.as_secs_f64()).exp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn traffic_is_reported_once() {
        let w = Wakeups::new(|_, _, _| Ok(()));
        w.saw("a");
        w.saw("b");
        w.saw("a");
        assert_eq!(w.take_traffic(), vec!["a".to_string(), "b".to_string()]);
        assert!(w.take_traffic().is_empty(), "already reported");
    }

    #[test]
    fn grace_runs_out() {
        let w = Wakeups::new(|_, _, _| Ok(()));
        assert_eq!(w.in_grace("a", Instant::now()), None, "never seen");
        w.saw("a");
        let left = w.in_grace("a", Instant::now()).expect("just seen");
        assert!(left <= GRACE && left > GRACE - Duration::from_secs(1));
        assert_eq!(w.in_grace("a", Instant::now() + GRACE), None);
    }

    /// A sandbox called every 60 s is prefetched once, about 20 s before
    /// its next call; an irregular one never is.
    #[test]
    fn a_periodic_sandbox_is_prefetched_ahead_of_its_next_call() {
        let w = Wakeups::new(|_, _, _| Ok(()));
        let t0 = Instant::now();
        let s = Duration::from_secs;
        for k in 0..4 {
            w.arrived("cron", t0 + s(60 * k));
        }
        for k in [0, 40, 200, 230] {
            w.arrived("chaos", t0 + s(k));
        }
        let last = t0 + s(180);
        assert!(w.due_prefetch(last + s(30)).is_empty(), "too early");
        assert_eq!(w.due_prefetch(last + s(45)), vec!["cron".to_string()]);
        assert!(w.due_prefetch(last + s(50)).is_empty(), "once per cycle");
        assert!(
            w.due_prefetch(last + s(100)).is_empty(),
            "missed: no late read"
        );
    }

    #[test]
    fn too_short_a_period_is_not_worth_predicting() {
        let w = Wakeups::new(|_, _, _| Ok(()));
        let t0 = Instant::now();
        for k in 0..4 {
            w.arrived("busy", t0 + Duration::from_secs(20 * k));
        }
        assert!(w.due_prefetch(t0 + Duration::from_secs(75)).is_empty());
    }

    fn seen(id: &str, tier: Tier) -> ObservedSandbox {
        ObservedSandbox {
            id: id.into(),
            tier,
            serving: tier.is_serving(),
            ip: None,
            net_index: None,
            egress: None,
        }
    }

    /// Frozen goes before Hot, and within each the one least in demand:
    /// a sandbox called once a second ago goes before one called thirty
    /// times that has been quiet for half a minute, and of two called once,
    /// the one called longer ago goes first. Never the sandbox being woken,
    /// one with a request in flight, one that has only just come up, or one
    /// already on disk.
    #[test]
    fn a_wake_puts_away_the_least_wanted_first() {
        let w = Arc::new(Wakeups::new(|_, _, _| Ok(())));
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let now = t0 + s(300);
        w.saw_at("frozen-once-long-ago", t0 + s(100));
        w.saw_at("frozen-once-recent", t0 + s(250));
        w.saw_at("once-long-ago", t0 + s(150));
        w.saw_at("once-just-now", t0 + s(299));
        for i in 0..30 {
            w.saw_at("popular", t0 + s(240 + i));
        }
        w.saw_at("busy", t0 + s(150));
        let _request = w.hold("busy");
        w.fresh.lock().insert("fresh".into(), now - s(1));
        let observed = [
            seen("once-just-now", Tier::Hot),
            seen("frozen-once-recent", Tier::Frozen),
            seen("busy", Tier::Hot),
            seen("popular", Tier::Hot),
            seen("target", Tier::Frozen),
            seen("once-long-ago", Tier::Hot),
            seen("on-disk", Tier::Snapshot),
            seen("fresh", Tier::Hot),
            seen("frozen-once-long-ago", Tier::Frozen),
            seen("adopted", Tier::Hot),
        ];
        assert_eq!(
            w.victims(&observed, "target", now),
            [
                "frozen-once-long-ago",
                "frozen-once-recent",
                "adopted",
                "once-long-ago",
                "once-just-now",
                "popular"
            ]
        );
    }

    /// Reported on every report for as long as it is protected, then not.
    #[test]
    fn evictions_are_reported_until_the_master_has_caught_up() {
        let w = Wakeups::new(|_, _, _| Ok(()));
        w.put_away("b");
        w.put_away("a");
        let now = Instant::now();
        assert_eq!(w.recently_evicted(now), ["a", "b"]);
        assert_eq!(w.recently_evicted(now), ["a", "b"], "repeated, not drained");
        assert!(w.evicted_for("a", now).is_some());
        assert!(w.recently_evicted(now + GRACE).is_empty());
        assert_eq!(w.evicted_for("a", now + GRACE), None);
    }

    #[test]
    fn forgetting_drops_everything() {
        let w = Wakeups::new(|_, _, _| Ok(()));
        w.saw("a");
        w.started("a");
        w.forget("a");
        assert!(w.demand.lock().is_empty());
        assert!(w.fresh.lock().is_empty());
        assert_eq!(w.in_grace("a", Instant::now()), None);
        assert!(w.take_traffic().is_empty());
    }

    /// The point of the per-sandbox lock: a wake must not run runsc while
    /// a command from the master is changing the same sandbox's tier.
    #[tokio::test]
    async fn a_wake_waits_for_a_tier_change_in_progress() {
        let running = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicUsize::new(0));
        let (r, o) = (running.clone(), overlapped.clone());
        let w = Arc::new(Wakeups::new(move |_, _, _| {
            if r.fetch_add(1, Ordering::SeqCst) > 0 {
                o.fetch_add(1, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(20));
            r.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }));

        // Held by "the master's command" on another thread for 100 ms.
        let lock = w.lock("sb");
        let (taken, is_taken) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = lock.lock();
            taken.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        is_taken.recv().unwrap();
        let t0 = Instant::now();
        w.wake("sb").await.unwrap();
        assert!(
            t0.elapsed() >= Duration::from_millis(80),
            "the wake ran through a held lock"
        );
        holder.join().unwrap();

        // And many wakes for one sandbox run one at a time.
        let all: Vec<_> = (0..8)
            .map(|_| {
                let w = w.clone();
                tokio::spawn(async move { w.wake("sb").await })
            })
            .collect();
        for h in all {
            h.await.unwrap().unwrap();
        }
        assert_eq!(overlapped.load(Ordering::SeqCst), 0);
    }

    /// A caller that hangs up does not take its restore permit with it.
    ///
    /// The gateway's handler is dropped when its client disconnects, but
    /// the wake it started runs on a blocking thread and carries on. On
    /// 2026-10-28 the permit went with the handler: a load generator giving
    /// up after 60 s let wake after wake past the limit, each then waiting
    /// out memory pressure on its own thread, and when the pressure eased
    /// 24 restores started in the same second on a 2-vCPU node.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_hanging_up_does_not_free_its_permit() {
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((parking_lot::Mutex::new(false), parking_lot::Condvar::new()));
        let (r, m, g) = (running.clone(), most.clone(), gate.clone());
        let w = Arc::new(Wakeups::new(move |_, _, _| {
            let now = r.fetch_add(1, Ordering::SeqCst) + 1;
            m.fetch_max(now, Ordering::SeqCst);
            let (open, cv) = &*g;
            let mut open = open.lock();
            while !*open {
                cv.wait(&mut open);
            }
            drop(open);
            r.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }));
        let parallel = w.restores.available_permits();

        // Three times as many sandboxes as permits, every caller giving up
        // long before its wake could finish.
        for i in 0..parallel * 3 {
            let w = w.clone();
            let id = format!("sb-{i}");
            let _ =
                tokio::time::timeout(Duration::from_millis(20), async move { w.wake(&id).await })
                    .await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let at_once = running.load(Ordering::SeqCst);

        // Opened before asserting: the runtime waits for its blocking
        // threads when it shuts down, so a failure with the gate closed
        // would hang rather than fail.
        let (open, cv) = &*gate;
        *open.lock() = true;
        cv.notify_all();
        assert!(
            at_once <= parallel,
            "{at_once} wakes running past {parallel} permits"
        );
        // The wakes that were running finish and hand their permits back.
        // Those whose callers hung up while still queued never started.
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.restores.available_permits() < parallel && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            w.restores.available_permits(),
            parallel,
            "a permit was lost"
        );
        assert!(most.load(Ordering::SeqCst) <= parallel);
    }

    /// A sandbox woken for a request is spared until that request has its
    /// answer, however long the wake took, and no longer. On 2026-10-28, at
    /// 80 per node and 8 req/s, requests waited 24 s (p50) for their wake;
    /// counted from the request, the sandbox came up looking long idle and
    /// the next wake put it to disk 0.24 s later -- 98% of the 502s.
    /// Spared for a fixed 2 s after answering instead, nearly every hot
    /// sandbox was spared at once and 26% of requests were refused.
    #[tokio::test]
    async fn a_sandbox_is_spared_while_its_request_is_in_flight_and_no_longer() {
        let w = Arc::new(Wakeups::new(|_, _, _| Ok(())));
        let asked = Instant::now().checked_sub(Duration::from_secs(30)).unwrap();
        w.saw_at("a", asked);
        w.saw_at("b", asked);
        let observed = [seen("a", Tier::Hot), seen("b", Tier::Hot)];

        let request = w.hold("a");
        w.wake("a").await.unwrap();
        let now = Instant::now();
        assert_eq!(w.victims(&observed, "c", now), ["b"]);
        assert!(
            w.in_grace("a", now).is_some(),
            "nor demoted by the master on a view from before it came up"
        );

        // Answered. Wanted no more than "b", but used since.
        drop(request);
        assert_eq!(w.victims(&observed, "c", Instant::now()), ["b", "a"]);
    }

    /// Holds every restore permit until `release` is called, so a test can
    /// see what a wake does when none is free.
    fn blocking_wakes() -> (
        Arc<Wakeups>,
        Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>,
    ) {
        let gate = Arc::new((parking_lot::Mutex::new(false), parking_lot::Condvar::new()));
        let g = gate.clone();
        let w = Wakeups::new(move |_, _, _| {
            let (open, cv) = &*g;
            let mut open = open.lock();
            while !*open {
                cv.wait(&mut open);
            }
            Ok(())
        });
        (Arc::new(w), gate)
    }

    fn release(gate: &(parking_lot::Mutex<bool>, parking_lot::Condvar)) {
        *gate.0.lock() = true;
        gate.1.notify_all();
    }

    /// A wake that must not take long, timed. Asserting comes after the
    /// gate is released: the runtime waits for its blocking threads when it
    /// shuts down, so a failure with the gate shut would hang, not fail.
    async fn late_wake(w: &Arc<Wakeups>) -> (Result<Duration>, Duration) {
        let t0 = Instant::now();
        let r = match tokio::time::timeout(Duration::from_secs(5), w.wake("late")).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("still waiting after 5 s")),
        };
        (r, t0.elapsed())
    }

    fn held_back(r: &Result<Duration>) -> String {
        let e = r.as_ref().expect_err("refused");
        assert!(
            e.downcast_ref::<super::super::HeldBack>().is_some(),
            "{e:#}"
        );
        format!("{e:#}")
    }

    /// A wake that cannot get a restore permit within its budget is
    /// refused, as held back, rather than left queueing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wake_that_cannot_start_in_time_is_refused() {
        let (w, gate) = blocking_wakes();
        let w = Arc::new(
            Arc::try_unwrap(w)
                .ok()
                .unwrap()
                .with_limits(Duration::from_millis(300), Some(100)),
        );
        let permits = w.restores.available_permits();
        let busy: Vec<_> = (0..permits)
            .map(|i| {
                let w = w.clone();
                tokio::spawn(async move { w.wake(&format!("busy-{i}")).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (r, took) = late_wake(&w).await;
        release(&gate);
        for b in busy {
            b.await.unwrap().unwrap();
        }

        let why = held_back(&r);
        assert!(why.contains("no restore could start within"), "{why}");
        assert!(took >= Duration::from_millis(200), "{took:?}");
        assert!(took < Duration::from_secs(1), "{took:?}");
    }

    /// With as many wakes already waiting as the node allows, another is
    /// refused at once: it would not start in time, so its caller should
    /// not wait to find that out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wake_behind_a_full_queue_is_refused_at_once() {
        let (w, gate) = blocking_wakes();
        let w = Arc::new(
            Arc::try_unwrap(w)
                .ok()
                .unwrap()
                .with_limits(Duration::from_secs(10), Some(1)),
        );
        let permits = w.restores.available_permits();
        let mut busy: Vec<_> = (0..permits + 1)
            .map(|i| {
                let w = w.clone();
                tokio::spawn(async move { w.wake(&format!("busy-{i}")).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (r, took) = late_wake(&w).await;
        release(&gate);
        for b in busy.drain(..) {
            b.await.unwrap().unwrap();
        }

        let why = held_back(&r);
        assert!(why.contains("already waiting"), "{why}");
        assert!(took < Duration::from_millis(100), "{took:?}");
        // And the count is back to nothing waiting.
        assert_eq!(w.waiting.load(Ordering::SeqCst), 0);
    }

    /// A wake waits out memory pressure only for what is left of its
    /// budget once it has its permit, not a fixed time on top of the wait.
    #[tokio::test]
    async fn a_wake_waits_out_pressure_only_for_what_is_left() {
        let given = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let g = given.clone();
        let w = Arc::new(
            Wakeups::new(move |_, _, patience| {
                g.lock().push(patience);
                std::thread::sleep(Duration::from_millis(300));
                Ok(())
            })
            .with_limits(Duration::from_secs(2), None),
        );
        // More than there are permits, so some wait for one first.
        let permits = w.restores.available_permits();
        let all: Vec<_> = (0..permits * 2)
            .map(|i| {
                let w = w.clone();
                tokio::spawn(async move { w.wake(&format!("sb-{i}")).await })
            })
            .collect();
        for a in all {
            a.await.unwrap().unwrap();
        }
        let given = given.lock();
        assert!(
            given.iter().all(|p| *p <= Duration::from_secs(2)),
            "{given:?}"
        );
        assert!(
            given.iter().any(|p| *p < Duration::from_millis(1800)),
            "the ones that queued got less: {given:?}"
        );
    }
}
