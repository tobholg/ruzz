//! In-process metrics behind the Activity tab's resource and search-load
//! views: per-request query counters and a bounded in-memory ring of
//! resource samples.
//!
//! Deliberately not a time-series database — the ring holds ~24h at a 15s
//! cadence (a few hundred KB), lives only as long as the process, and is
//! served decimated. Long-term monitoring belongs to an external scraper.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Ring cadence. Also the resolution of the QPS and p95 figures.
pub const SAMPLE_INTERVAL_SECS: u64 = 15;
/// ~24h at the sample cadence.
pub const RING_CAPACITY: usize = (24 * 60 * 60 / SAMPLE_INTERVAL_SECS) as usize;
/// Most points one /activity response carries per series — enough for a
/// few-hundred-pixel sparkline, a fraction of the ring's weight.
pub const MAX_POINTS_SERVED: usize = 320;
/// Latencies buffered between ticks. A tick that saw more queries than this
/// computes its percentile over the first N — fine for a load indicator.
const MAX_LATENCIES_PER_TICK: usize = 65_536;

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Sample {
    /// Unix seconds.
    pub t: u64,
    pub rss_bytes: u64,
    pub available_mem: u64,
    pub total_mem: u64,
    pub disk_free: u64,
    pub disk_total: u64,
    /// Process CPU, percent of one core (like top).
    pub cpu_pct: f32,
    /// Queries per second over the tick's interval.
    pub qps: f32,
    /// p95 search latency over the tick's interval; None when idle.
    pub p95_ms: Option<f64>,
}

/// Everything a sample needs that the sampler reads from the system.
pub struct ResourceReading {
    pub rss_bytes: u64,
    pub available_mem: u64,
    pub total_mem: u64,
    pub disk_free: u64,
    pub disk_total: u64,
    pub cpu_pct: f32,
}

pub struct Metrics {
    query_count: AtomicU64,
    latencies_ms: Mutex<Vec<f64>>,
    ring: Mutex<VecDeque<Sample>>,
    last_count: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            query_count: AtomicU64::new(0),
            latencies_ms: Mutex::new(Vec::new()),
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            last_count: AtomicU64::new(0),
        }
    }
}

impl Metrics {
    /// Record one served search request.
    pub fn record_query(&self, latency_ms: f64) {
        self.query_count.fetch_add(1, Ordering::Relaxed);
        let mut latencies = self.latencies_ms.lock().unwrap();
        if latencies.len() < MAX_LATENCIES_PER_TICK {
            latencies.push(latency_ms);
        }
    }

    pub fn total_queries(&self) -> u64 {
        self.query_count.load(Ordering::Relaxed)
    }

    /// Fold the interval since the last tick into one ring sample.
    /// `elapsed_secs` is a parameter (not measured here) so tests can drive
    /// deterministic intervals.
    pub fn sample_tick(&self, now_unix: u64, elapsed_secs: f64, reading: ResourceReading) {
        let drained: Vec<f64> = std::mem::take(&mut *self.latencies_ms.lock().unwrap());
        let count = self.query_count.load(Ordering::Relaxed);
        let interval_queries = count.saturating_sub(self.last_count.swap(count, Ordering::Relaxed));
        let qps = if elapsed_secs > 0.0 {
            (interval_queries as f64 / elapsed_secs) as f32
        } else {
            0.0
        };

        let sample = Sample {
            t: now_unix,
            rss_bytes: reading.rss_bytes,
            available_mem: reading.available_mem,
            total_mem: reading.total_mem,
            disk_free: reading.disk_free,
            disk_total: reading.disk_total,
            cpu_pct: reading.cpu_pct,
            qps,
            p95_ms: percentile(drained, 0.95),
        };

        let mut ring = self.ring.lock().unwrap();
        if ring.len() >= RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(sample);
    }

    /// The ring, oldest first, decimated to at most `max_points` by stride —
    /// always including the newest sample.
    pub fn samples(&self, max_points: usize) -> Vec<Sample> {
        let ring = self.ring.lock().unwrap();
        let len = ring.len();
        if len == 0 || max_points == 0 {
            return Vec::new();
        }
        let stride = len.div_ceil(max_points);
        let mut out: Vec<Sample> = ring.iter().rev().step_by(stride).copied().collect();
        out.reverse();
        out
    }

    pub fn latest(&self) -> Option<Sample> {
        self.ring.lock().unwrap().back().copied()
    }
}

fn percentile(mut values: Vec<f64>, p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((p * values.len() as f64).ceil() as usize).clamp(1, values.len());
    Some(values[rank - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading() -> ResourceReading {
        ResourceReading {
            rss_bytes: 1000,
            available_mem: 2000,
            total_mem: 4000,
            disk_free: 50,
            disk_total: 100,
            cpu_pct: 3.5,
        }
    }

    #[test]
    fn ticks_fold_queries_into_qps_and_p95() {
        let metrics = Metrics::default();
        for ms in [1.0, 2.0, 3.0, 100.0] {
            metrics.record_query(ms);
        }
        metrics.sample_tick(1000, 2.0, reading());

        let sample = metrics.latest().unwrap();
        assert_eq!(sample.qps, 2.0, "4 queries over 2s");
        assert_eq!(sample.p95_ms, Some(100.0));

        // The next idle tick starts from a clean interval.
        metrics.sample_tick(1015, 15.0, reading());
        let sample = metrics.latest().unwrap();
        assert_eq!(sample.qps, 0.0);
        assert_eq!(sample.p95_ms, None);
        assert_eq!(metrics.total_queries(), 4);
    }

    #[test]
    fn ring_is_bounded_and_decimation_keeps_the_newest() {
        let metrics = Metrics::default();
        for i in 0..(RING_CAPACITY as u64 + 100) {
            metrics.sample_tick(i, 1.0, reading());
        }
        let all = metrics.samples(usize::MAX);
        assert_eq!(all.len(), RING_CAPACITY, "oldest samples dropped");
        assert_eq!(all.first().unwrap().t, 100);

        let few = metrics.samples(10);
        assert!(few.len() <= 10);
        assert_eq!(
            few.last().unwrap().t,
            RING_CAPACITY as u64 + 99,
            "newest sample always served"
        );
        assert!(few.windows(2).all(|w| w[0].t < w[1].t), "oldest first");
    }

    #[test]
    fn percentiles_behave_at_the_edges() {
        assert_eq!(percentile(vec![], 0.95), None);
        assert_eq!(percentile(vec![7.0], 0.95), Some(7.0));
        let hundred: Vec<f64> = (1..=100).map(|n| n as f64).collect();
        assert_eq!(percentile(hundred.clone(), 0.95), Some(95.0));
        assert_eq!(percentile(hundred, 0.5), Some(50.0));
    }
}
