//! M3 benchmark accumulator: per-iteration reset-latency (split into page-copy
//! vs register-restore), dirty-set-size samples, the coverage-over-time curve,
//! and time-to-first-crash. Sample vectors are capped so a multi-hour run cannot
//! grow unbounded; once capped, `capped` is set and later samples are dropped
//! (steady state is reached quickly, so the retained prefix is representative —
//! noted in the result doc).

/// Hard cap on retained per-iteration samples (≈8 MiB per u32 vector).
pub const SAMPLE_CAP: usize = 2_000_000;

#[derive(Default)]
pub struct Metrics {
    reset_total_us: Vec<u32>,
    restore_us: Vec<u32>,
    regs_us: Vec<u32>,
    dirty_pages: Vec<u32>,
    cov_curve: Vec<(f64, u64)>, // (elapsed_secs, distinct_edges)
    first_crash_secs: Option<f64>,
    capped: bool,
}

/// Nearest-rank percentile of an already-sorted slice. `p` in [0.0, 1.0].
/// Empty slice -> 0.
pub fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let p = p.clamp(0.0, 1.0);
    // nearest-rank: rank = ceil(p * N), 1-based; index = rank-1, clamped.
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

impl Metrics {
    pub fn new() -> Metrics {
        Metrics::default()
    }

    fn push_capped(v: &mut Vec<u32>, x: u32, capped: &mut bool) {
        if v.len() >= SAMPLE_CAP {
            *capped = true;
            return;
        }
        v.push(x);
    }

    pub fn record_reset(&mut self, total_us: u32, restore_us: u32, regs_us: u32) {
        let mut capped = self.capped;
        Metrics::push_capped(&mut self.reset_total_us, total_us, &mut capped);
        Metrics::push_capped(&mut self.restore_us, restore_us, &mut capped);
        Metrics::push_capped(&mut self.regs_us, regs_us, &mut capped);
        self.capped = capped;
    }

    pub fn record_dirty(&mut self, pages: u32) {
        let mut capped = self.capped;
        Metrics::push_capped(&mut self.dirty_pages, pages, &mut capped);
        self.capped = capped;
    }

    pub fn sample_coverage(&mut self, elapsed_secs: f64, edges: u64) {
        self.cov_curve.push((elapsed_secs, edges));
    }

    /// Record time-to-first-crash; only the first call sticks.
    pub fn record_first_crash(&mut self, elapsed_secs: f64) {
        if self.first_crash_secs.is_none() {
            self.first_crash_secs = Some(elapsed_secs);
        }
    }

    pub fn capped(&self) -> bool {
        self.capped
    }
    pub fn first_crash_secs(&self) -> Option<f64> {
        self.first_crash_secs
    }

    /// Render the machine-parseable report block consumed by
    /// `scripts/fuzz_m3_bench.py`. `iterations`/`elapsed_secs` come from the
    /// controller (the loop counter and the run clock).
    pub fn report(&self, iterations: u64, elapsed_secs: f64) -> String {
        let mut rt = self.reset_total_us.clone();
        let mut rs = self.restore_us.clone();
        let mut rg = self.regs_us.clone();
        let mut dp = self.dirty_pages.clone();
        rt.sort_unstable();
        rs.sort_unstable();
        rg.sort_unstable();
        dp.sort_unstable();
        let eps = if elapsed_secs > 0.0 { iterations as f64 / elapsed_secs } else { 0.0 };
        let cov_final = self.cov_curve.last().map(|&(_, e)| e).unwrap_or(0);
        let crash = match self.first_crash_secs {
            Some(s) => format!("{s:.3}"),
            None => "none".to_string(),
        };
        let mut out = String::new();
        out.push_str(&format!("metric iters={iterations} elapsed_s={elapsed_secs:.3} execs_per_sec={eps:.0}\n"));
        out.push_str(&format!(
            "metric reset_us_p50={} reset_us_p99={} reset_us_max={}\n",
            percentile(&rt, 0.50), percentile(&rt, 0.99), rt.last().copied().unwrap_or(0)
        ));
        out.push_str(&format!(
            "metric restore_us_p50={} restore_us_p99={}\n",
            percentile(&rs, 0.50), percentile(&rs, 0.99)
        ));
        out.push_str(&format!(
            "metric regs_us_p50={} regs_us_p99={}\n",
            percentile(&rg, 0.50), percentile(&rg, 0.99)
        ));
        out.push_str(&format!(
            "metric dirty_pages_p50={} dirty_pages_p99={} dirty_pages_max={}\n",
            percentile(&dp, 0.50), percentile(&dp, 0.99), dp.last().copied().unwrap_or(0)
        ));
        out.push_str(&format!("metric coverage_final={cov_final} time_to_crash_s={crash} capped={}\n", self.capped));
        for &(t, e) in &self.cov_curve {
            out.push_str(&format!("covsample t={t:.3} edges={e}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<u32> = (1..=100).collect(); // 1..100 sorted
        assert_eq!(percentile(&v, 0.50), 50);
        assert_eq!(percentile(&v, 0.99), 99);
        assert_eq!(percentile(&v, 1.0), 100);
        assert_eq!(percentile(&v, 0.0), 1);
    }

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn first_crash_only_first_sticks() {
        let mut m = Metrics::new();
        m.record_first_crash(1.5);
        m.record_first_crash(9.9);
        assert_eq!(m.first_crash_secs(), Some(1.5));
    }

    #[test]
    fn report_has_all_metric_keys() {
        let mut m = Metrics::new();
        m.record_reset(100, 70, 30);
        m.record_reset(200, 150, 50);
        m.record_dirty(4);
        m.record_dirty(8);
        m.sample_coverage(0.0, 2);
        m.sample_coverage(2.0, 10);
        let r = m.report(5000, 5.0);
        for key in [
            "execs_per_sec=", "reset_us_p50=", "restore_us_p50=", "regs_us_p50=",
            "dirty_pages_p50=", "coverage_final=", "time_to_crash_s=none",
        ] {
            assert!(r.contains(key), "missing {key} in:\n{r}");
        }
        assert!(r.contains("covsample t=2.000 edges=10"));
    }

    #[test]
    fn cap_sets_flag_and_stops_growth() {
        let mut m = Metrics::new();
        for _ in 0..(SAMPLE_CAP + 10) {
            m.record_dirty(1);
        }
        assert!(m.capped());
    }
}
