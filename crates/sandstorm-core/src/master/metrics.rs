//! What the master is doing, in Prometheus text format.
//!
//! Hand-rolled rather than pulled from a metrics crate. The exposition
//! format is a few lines to emit, the series below are a fixed set, and a
//! registry plus its macro layer would be more code than this file.
//!
//! Three kinds of number live here, and they answer different questions:
//!
//!   counters    what the control plane was asked to do, and how it went
//!   histograms  how long it took, per route
//!   gauges      what the fleet looks like right now, read from the store
//!
//! The gauges are `COUNT(*)` over indexed columns, taken once per scrape.
//! That is deliberate: the scheduler used to count a node's sandboxes by
//! selecting every row and calling `.len()`, which cost 14.5 ms per create
//! at 5 nodes x 2000 sandboxes. A metrics endpoint that reintroduced that
//! shape would be worse, because a scraper calls it on a timer whether
//! anyone is looking or not.
//!
//! ## Reading the latency histograms
//!
//! They are labelled by **matched route**, not by request path, so a
//! thousand sandbox ids collapse onto `/v1/sandboxes/:id` instead of
//! becoming a thousand series.
//!
//! `/v1/node/poll` is a long poll: the master parks it for up to
//! `--hold-secs` when it has nothing to say, so its duration is the hold,
//! not a cost. It is its own series for that reason -- do not add it to
//! the others.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::state::Store;
use crate::tier::Tier;

/// Bucket edges in seconds, as `le` upper bounds.
///
/// Spread over five decades because the same histogram has to show both a
/// list served from sqlite in under a millisecond and an exec that waits
/// on a node across the public internet.
const BUCKETS: [f64; 14] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

#[derive(Debug)]
pub struct Histogram {
    /// Count per bucket, not cumulative. Accumulated at render time, which
    /// keeps `observe` to a single `fetch_add`.
    buckets: [AtomicU64; BUCKETS.len()],
    sum_nanos: AtomicU64,
    count: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_nanos: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    pub fn observe(&self, d: Duration) {
        let s = d.as_secs_f64();
        // First bucket whose bound is >= s: `le` is inclusive, so a value
        // exactly on a bound belongs to that bound's bucket.
        let i = BUCKETS.partition_point(|b| *b < s);
        if i < BUCKETS.len() {
            self.buckets[i].fetch_add(1, Relaxed);
        }
        self.sum_nanos.fetch_add(d.as_nanos() as u64, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let mut acc = 0u64;
        for (i, edge) in BUCKETS.iter().enumerate() {
            acc += self.buckets[i].load(Relaxed);
            let _ = writeln!(
                out,
                "{name}_bucket{{{}le=\"{edge}\"}} {acc}",
                comma_after(labels)
            );
        }
        let total = self.count.load(Relaxed);
        let _ = writeln!(
            out,
            "{name}_bucket{{{}le=\"+Inf\"}} {total}",
            comma_after(labels)
        );
        let sum = self.sum_nanos.load(Relaxed) as f64 / 1e9;
        // `name{}` is legal but reads as a mistake; an unlabelled series
        // gets no braces.
        let braced = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        let _ = writeln!(out, "{name}_sum{braced} {sum}");
        let _ = writeln!(out, "{name}_count{braced} {total}");
    }
}

/// A histogram's counts at one moment.
///
/// The console wants latency over the last few seconds, not since the
/// process started, so it keeps the previous snapshot and takes the
/// difference.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistSnap {
    buckets: [u64; BUCKETS.len()],
    pub count: u64,
}

impl Histogram {
    pub fn snap(&self) -> HistSnap {
        HistSnap {
            buckets: std::array::from_fn(|i| self.buckets[i].load(Relaxed)),
            count: self.count.load(Relaxed),
        }
    }
}

impl HistSnap {
    pub fn add(&mut self, o: &HistSnap) {
        for (a, b) in self.buckets.iter_mut().zip(o.buckets) {
            *a += b;
        }
        self.count += o.count;
    }

    /// What was observed after `earlier` was taken.
    pub fn since(&self, earlier: &HistSnap) -> HistSnap {
        HistSnap {
            buckets: std::array::from_fn(|i| self.buckets[i].saturating_sub(earlier.buckets[i])),
            count: self.count.saturating_sub(earlier.count),
        }
    }

    /// The `q` quantile in seconds, interpolated within its bucket.
    ///
    /// An estimate, as any histogram quantile is: good to the bucket's
    /// width. A value past the last bucket reports the last edge, which
    /// reads as "at least this". `None` when nothing was observed.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let want = q.clamp(0.0, 1.0) * self.count as f64;
        let mut acc = 0u64;
        for (i, n) in self.buckets.iter().enumerate() {
            if *n > 0 && (acc + n) as f64 >= want {
                let lo = if i == 0 { 0.0 } else { BUCKETS[i - 1] };
                let frac = ((want - acc as f64) / *n as f64).clamp(0.0, 1.0);
                return Some(lo + (BUCKETS[i] - lo) * frac);
            }
            acc += n;
        }
        Some(BUCKETS[BUCKETS.len() - 1])
    }
}

/// `a="b"` -> `a="b",` so a bare `le=` can follow. Empty stays empty.
fn comma_after(labels: &str) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{labels},")
    }
}

/// Escape a label value per the exposition format.
fn esc(v: &str) -> String {
    v.replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[derive(Default, Debug)]
struct RouteStats {
    latency: Histogram,
    /// Status classes rather than exact codes: the create path reports its
    /// own outcomes as counters below, where the distinction that matters
    /// (placed vs refused) is a semantic one rather than 201 vs 503.
    ok: AtomicU64,
    client_err: AtomicU64,
    server_err: AtomicU64,
}

/// Everything the master counts about itself.
///
/// Always on. A lock acquisition pays three `Instant::now()` calls and the
/// request path pays a few relaxed atomics, which is noise beside a sqlite
/// query -- and the store mutex is the first thing a load test will want
/// to rule in or out, so measuring it is the point.
#[derive(Debug)]
pub struct Metrics {
    /// What each node last said it has on. Kept here rather than in the
    /// store: it is re-sent on every report and means nothing once stale.
    pub node_features: parking_lot::Mutex<HashMap<String, Vec<String>>>,
    /// The templates each node last said it can start on its backend
    /// (`PollRequest::templates`). Absent for a node that has not said,
    /// which is placed on as before.
    pub node_templates: parking_lot::Mutex<HashMap<String, Vec<String>>>,
    /// The last host description each node reported, so the master writes
    /// one only when it changes.
    pub node_hosts: parking_lot::Mutex<HashMap<String, crate::proto::HostInfo>>,
    pub creates_placed: AtomicU64,
    pub creates_refused: AtomicU64,
    pub deletes: AtomicU64,
    /// Sandboxes a node woke because a request reached its gateway.
    pub woken: AtomicU64,
    /// The same wakes, by the tier each was woken from, indexed by
    /// `Tier::ORDER`'s order.
    woken_from: [AtomicU64; 4],
    /// Sandboxes a node put to disk on its own to make room for a wake.
    pub evicted: AtomicU64,
    /// Demotions the sweep decided, by destination tier, indexed by
    /// `Tier::ORDER`'s order.
    demotions: [AtomicU64; 4],
    /// How long the demotion sweep takes. It holds the store lock for the
    /// whole pass, so this is also time nothing else can touch the store.
    pub demote_sweep: Histogram,
    /// Waiting for the store mutex, and then holding it. Contention shows
    /// as the first one growing while the second does not.
    pub store_wait: Histogram,
    pub store_held: Histogram,
    http: RwLock<HashMap<(String, String), Arc<RouteStats>>>,
    started: Instant,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            node_features: Default::default(),
            node_templates: Default::default(),
            node_hosts: Default::default(),
            creates_placed: AtomicU64::new(0),
            creates_refused: AtomicU64::new(0),
            deletes: AtomicU64::new(0),
            woken: AtomicU64::new(0),
            woken_from: std::array::from_fn(|_| AtomicU64::new(0)),
            evicted: AtomicU64::new(0),
            demotions: std::array::from_fn(|_| AtomicU64::new(0)),
            demote_sweep: Histogram::default(),
            store_wait: Histogram::default(),
            store_held: Histogram::default(),
            http: RwLock::new(HashMap::new()),
            started: Instant::now(),
        }
    }
}

impl Metrics {
    pub fn observe_http(&self, method: &str, route: &str, status: u16, took: Duration) {
        let key = (method.to_string(), route.to_string());
        // Read first: the key set is bounded by the router's routes and
        // settles within the first few requests, so the write lock is taken
        // a handful of times over a process's life.
        let stats = {
            let map = self.http.read();
            map.get(&key).cloned()
        };
        let stats = match stats {
            Some(s) => s,
            None => self.http.write().entry(key).or_default().clone(),
        };
        stats.latency.observe(took);
        match status {
            500..=599 => &stats.server_err,
            400..=499 => &stats.client_err,
            _ => &stats.ok,
        }
        .fetch_add(1, Relaxed);
    }

    pub fn demoted(&self, to: Tier) {
        if let Some(i) = Tier::ORDER.iter().position(|t| *t == to) {
            self.demotions[i].fetch_add(1, Relaxed);
        }
    }

    /// A sandbox woken by a request, from the tier it was in.
    pub fn woke(&self, from: Tier) {
        self.woken.fetch_add(1, Relaxed);
        if let Some(i) = Tier::ORDER.iter().position(|t| *t == from) {
            self.woken_from[i].fetch_add(1, Relaxed);
        }
    }

    /// Wakes so far, by the tier woken from, in `Tier::ORDER`'s order.
    pub fn woken_from_total(&self) -> [u64; 4] {
        std::array::from_fn(|i| self.woken_from[i].load(Relaxed))
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Demotions so far, by destination tier, in `Tier::ORDER`'s order.
    pub fn demotions_total(&self) -> [u64; 4] {
        std::array::from_fn(|i| self.demotions[i].load(Relaxed))
    }

    /// Every API route's latency folded into one histogram, with the count
    /// of server errors.
    ///
    /// The node routes are left out: `/v1/node/poll` is parked for up to
    /// the hold by design, so its duration says nothing about how fast the
    /// API is answering, and it would swamp every other route.
    pub fn api_snap(&self) -> (HistSnap, u64) {
        let mut all = HistSnap::default();
        let mut errors = 0;
        for ((_, route), stats) in self.http.read().iter() {
            if route.starts_with("/v1/node/") {
                continue;
            }
            all.add(&stats.latency.snap());
            errors += stats.server_err.load(Relaxed);
        }
        (all, errors)
    }

    /// One route's latency, if it has been called.
    pub fn route_snap(&self, method: &str, route: &str) -> Option<HistSnap> {
        self.http
            .read()
            .get(&(method.to_string(), route.to_string()))
            .map(|s| s.latency.snap())
    }

    /// The whole exposition, gauges included.
    ///
    /// Takes the store rather than a snapshot so the gauges are read once,
    /// under one lock, and cannot disagree with each other.
    pub fn render(&self, store: &Store) -> anyhow::Result<String> {
        let mut o = String::with_capacity(4096);

        help(
            &mut o,
            "sandstorm_master_uptime_seconds",
            "gauge",
            "Seconds since this master process started",
        );
        let _ = writeln!(
            o,
            "sandstorm_master_uptime_seconds {}",
            self.started.elapsed().as_secs()
        );

        // --- what was asked of the control plane -------------------------
        help(
            &mut o,
            "sandstorm_sandbox_creates_total",
            "counter",
            "Sandbox creates, by whether a node took it",
        );
        let _ = writeln!(
            o,
            "sandstorm_sandbox_creates_total{{outcome=\"placed\"}} {}",
            self.creates_placed.load(Relaxed)
        );
        let _ = writeln!(
            o,
            "sandstorm_sandbox_creates_total{{outcome=\"refused\"}} {}",
            self.creates_refused.load(Relaxed)
        );

        help(
            &mut o,
            "sandstorm_sandbox_deletes_total",
            "counter",
            "Sandboxes deleted through the API",
        );
        let _ = writeln!(
            o,
            "sandstorm_sandbox_deletes_total {}",
            self.deletes.load(Relaxed)
        );

        help(
            &mut o,
            "sandstorm_demotions_total",
            "counter",
            "Idle demotions decided by the sweep, by destination tier",
        );
        for (i, t) in Tier::ORDER.iter().enumerate() {
            let _ = writeln!(
                o,
                "sandstorm_demotions_total{{to=\"{}\"}} {}",
                t.as_str(),
                self.demotions[i].load(Relaxed)
            );
        }

        // --- how long things took ----------------------------------------
        help(&mut o, "sandstorm_http_request_duration_seconds", "histogram",
             "Request duration by matched route. /v1/node/poll is a long poll: its duration is the hold, not a cost");
        {
            let map = self.http.read();
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            for k in keys {
                let s = &map[&k];
                let labels = format!("method=\"{}\",route=\"{}\"", esc(&k.0), esc(&k.1));
                s.latency
                    .render(&mut o, "sandstorm_http_request_duration_seconds", &labels);
            }
        }

        help(&mut o, "sandstorm_http_responses_total", "counter",
             "Responses by matched route and status class. A capacity refusal is a 503 and so lands in server_error; sandstorm_sandbox_creates_total{outcome} is what separates \"the fleet is full\" from \"the master is broken\"");
        {
            let map = self.http.read();
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            for k in keys {
                let s = &map[&k];
                let base = format!("method=\"{}\",route=\"{}\"", esc(&k.0), esc(&k.1));
                for (class, n) in [
                    ("ok", s.ok.load(Relaxed)),
                    ("client_error", s.client_err.load(Relaxed)),
                    ("server_error", s.server_err.load(Relaxed)),
                ] {
                    let _ = writeln!(
                        o,
                        "sandstorm_http_responses_total{{{base},class=\"{class}\"}} {n}"
                    );
                }
            }
        }

        help(&mut o, "sandstorm_store_lock_wait_seconds", "histogram",
             "Time spent waiting for the store mutex. Every API call takes it, so this is where control-plane contention shows first");
        self.store_wait
            .render(&mut o, "sandstorm_store_lock_wait_seconds", "");

        help(&mut o, "sandstorm_store_lock_held_seconds", "histogram",
             "Time the store mutex was held once acquired. One behind the wait count during a scrape: this scrape is holding it");
        self.store_held
            .render(&mut o, "sandstorm_store_lock_held_seconds", "");

        help(
            &mut o,
            "sandstorm_demote_sweep_seconds",
            "histogram",
            "Duration of one idle-demotion sweep, which holds the store lock throughout",
        );
        self.demote_sweep
            .render(&mut o, "sandstorm_demote_sweep_seconds", "");

        // --- what the fleet looks like now --------------------------------
        self.render_gauges(&mut o, store)?;
        Ok(o)
    }

    fn render_gauges(&self, o: &mut String, store: &Store) -> anyhow::Result<()> {
        let nodes = store.list_nodes()?;

        help(
            o,
            "sandstorm_nodes",
            "gauge",
            "Registered nodes, by whether they are currently polling",
        );
        let up = nodes.iter().filter(|n| n.connected).count();
        let _ = writeln!(o, "sandstorm_nodes{{state=\"connected\"}} {up}");
        let _ = writeln!(
            o,
            "sandstorm_nodes{{state=\"disconnected\"}} {}",
            nodes.len() - up
        );

        help(
            o,
            "sandstorm_node_free_memory_mib",
            "gauge",
            "Free memory a node reported on its last poll",
        );
        help(
            o,
            "sandstorm_node_pressure_some_avg10",
            "gauge",
            "Memory-pressure 10s average a node reported on its last poll",
        );
        help(
            o,
            "sandstorm_node_sandboxes",
            "gauge",
            "Sandboxes assigned to a node",
        );
        help(
            o,
            "sandstorm_node_max_sandboxes",
            "gauge",
            "Cap the node declared at registration; 0 means it declared none",
        );
        help(
            o,
            "sandstorm_node_last_seen_age_seconds",
            "gauge",
            "Seconds since this node last polled",
        );
        let now = crate::state::now_secs();
        for n in &nodes {
            let l = format!("node=\"{}\",pool=\"{}\"", esc(&n.id), esc(&n.pool));
            let _ = writeln!(o, "sandstorm_node_free_memory_mib{{{l}}} {}", n.free_mib);
            let _ = writeln!(
                o,
                "sandstorm_node_pressure_some_avg10{{{l}}} {}",
                n.psi_some_avg10
            );
            let _ = writeln!(
                o,
                "sandstorm_node_sandboxes{{{l}}} {}",
                store.count_sandboxes_on_node(&n.id)?
            );
            let _ = writeln!(o, "sandstorm_node_max_sandboxes{{{l}}} {}", n.max_sandboxes);
            let _ = writeln!(
                o,
                "sandstorm_node_last_seen_age_seconds{{{l}}} {}",
                (now - n.last_seen_at).max(0)
            );
        }

        help(
            o,
            "sandstorm_node_feature",
            "gauge",
            "A density lever a node has on, as it reported (1 per lever)",
        );
        {
            let features = self.node_features.lock();
            for n in &nodes {
                for f in features.get(&n.id).into_iter().flatten() {
                    let _ = writeln!(
                        o,
                        "sandstorm_node_feature{{node=\"{}\",pool=\"{}\",feature=\"{}\"}} 1",
                        esc(&n.id),
                        esc(&n.pool),
                        esc(f)
                    );
                }
            }
        }

        help(
            o,
            "sandstorm_sandboxes",
            "gauge",
            "Sandboxes in desired state, by tier",
        );
        let by_tier = store.count_sandboxes_by_tier()?;
        for t in Tier::ORDER {
            let _ = writeln!(
                o,
                "sandstorm_sandboxes{{tier=\"{}\"}} {}",
                t.as_str(),
                by_tier.get(t.as_str()).copied().unwrap_or(0)
            );
        }

        help(
            o,
            "sandstorm_sandboxes_failed",
            "gauge",
            "Sandboxes the node could not start, still carrying their last error",
        );
        let _ = writeln!(
            o,
            "sandstorm_sandboxes_failed {}",
            store.count_failed_sandboxes()?
        );

        help(
            o,
            "sandstorm_snapshots",
            "gauge",
            "Snapshot tags held across the fleet",
        );
        help(
            o,
            "sandstorm_snapshot_bytes",
            "gauge",
            "Total size of those snapshots as the nodes reported them",
        );
        let (n, bytes) = store.snapshot_totals()?;
        let _ = writeln!(o, "sandstorm_snapshots {n}");
        let _ = writeln!(o, "sandstorm_snapshot_bytes {bytes}");
        Ok(())
    }
}

/// Render against a live master, taking the store lock once for the gauges.
pub fn render_with(st: &super::api::MasterState) -> anyhow::Result<String> {
    let store = st.store();
    st.metrics.render(&store)
}

fn help(o: &mut String, name: &str, kind: &str, text: &str) {
    let _ = writeln!(o, "# HELP {name} {text}");
    let _ = writeln!(o, "# TYPE {name} {kind}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wake_counts_once_in_all_and_once_under_its_tier() {
        let m = Metrics::default();
        m.woke(Tier::Frozen);
        m.woke(Tier::Frozen);
        m.woke(Tier::Detached);
        assert_eq!(m.woken.load(Relaxed), 3);
        let by = m.woken_from_total();
        let at = |t: Tier| by[Tier::ORDER.iter().position(|x| *x == t).unwrap()];
        assert_eq!(at(Tier::Frozen), 2);
        assert_eq!(at(Tier::Snapshot), 0);
        assert_eq!(at(Tier::Detached), 1);
    }

    #[test]
    fn a_histogram_puts_a_value_on_the_bucket_it_is_equal_to() {
        let h = Histogram::default();
        // 5 ms is a bucket bound. `le` is inclusive, so it belongs there
        // and not in the next one up.
        h.observe(Duration::from_millis(5));
        let mut o = String::new();
        h.render(&mut o, "t", "");
        assert!(o.contains("t_bucket{le=\"0.005\"} 1"), "{o}");
        assert!(o.contains("t_bucket{le=\"0.0025\"} 0"), "{o}");
        assert!(o.contains("t_count 1"), "{o}");
    }

    #[test]
    fn a_quantile_comes_from_what_happened_since_the_last_snapshot() {
        let h = Histogram::default();
        for _ in 0..100 {
            h.observe(Duration::from_millis(2));
        }
        let before = h.snap();
        for _ in 0..99 {
            h.observe(Duration::from_micros(300));
        }
        h.observe(Duration::from_millis(400));
        let d = h.snap().since(&before);
        assert_eq!(d.count, 100);
        // The hundred 2 ms observations before the snapshot do not count.
        assert!(d.quantile(0.5).unwrap() <= 0.0005);
        let p99 = d.quantile(0.99).unwrap();
        assert!(p99 <= 0.0005, "{p99}");
        assert!(d.quantile(1.0).unwrap() > 0.25);
        assert!(HistSnap::default().quantile(0.5).is_none());
    }

    #[test]
    fn buckets_are_cumulative_and_inf_matches_the_count() {
        let h = Histogram::default();
        for ms in [1u64, 7, 300, 60_000] {
            h.observe(Duration::from_millis(ms));
        }
        let mut o = String::new();
        h.render(&mut o, "t", "");
        // 60 s is past the last edge, so it lands only in +Inf -- which is
        // what makes the count the right denominator for a quantile.
        assert!(o.contains("t_bucket{le=\"10\"} 3"), "{o}");
        assert!(o.contains("t_bucket{le=\"+Inf\"} 4"), "{o}");
        assert!(o.contains("t_count 4"), "{o}");
    }

    #[test]
    fn route_labels_are_the_matched_path_not_the_url() {
        let m = Metrics::default();
        m.observe_http("GET", "/v1/sandboxes/:id", 200, Duration::from_millis(2));
        m.observe_http("GET", "/v1/sandboxes/:id", 404, Duration::from_millis(1));
        m.observe_http("GET", "/v1/sandboxes/:id", 500, Duration::from_millis(1));
        let store = Store::open_in_memory().unwrap();
        let out = m.render(&store).unwrap();
        assert!(
            out.contains(r#"route="/v1/sandboxes/:id",class="ok"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"route="/v1/sandboxes/:id",class="client_error"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"route="/v1/sandboxes/:id",class="server_error"} 1"#),
            "{out}"
        );
    }

    /// Every series carries its HELP and TYPE, and no line is left with an
    /// empty value -- the two ways a hand-rolled exposition goes wrong.
    #[test]
    fn the_exposition_is_well_formed_on_an_empty_fleet() {
        let m = Metrics::default();
        let store = Store::open_in_memory().unwrap();
        let out = m.render(&store).unwrap();
        let mut declared = std::collections::HashSet::new();
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                declared.insert(rest.split(' ').next().unwrap().to_string());
                continue;
            }
            if line.starts_with("# HELP ") || line.is_empty() {
                continue;
            }
            let (name, value) = line.rsplit_once(' ').expect("every sample has a value");
            assert!(!value.is_empty(), "empty value: {line}");
            assert!(
                value.parse::<f64>().is_ok(),
                "value is not a number: {line}"
            );
            let base = name.split('{').next().unwrap();
            let base = base
                .strip_suffix("_bucket")
                .or_else(|| base.strip_suffix("_sum"))
                .or_else(|| base.strip_suffix("_count"))
                .unwrap_or(base);
            assert!(declared.contains(base), "{base} has no TYPE line: {line}");
        }
        // The fleet gauges exist at zero rather than being absent, so a
        // dashboard does not read "no data" and "nothing running" the same.
        assert!(out.contains("sandstorm_sandboxes{tier=\"hot\"} 0"), "{out}");
        assert!(
            out.contains("sandstorm_nodes{state=\"connected\"} 0"),
            "{out}"
        );
    }
}
