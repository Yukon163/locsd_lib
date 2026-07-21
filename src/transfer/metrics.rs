use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct MetricsWindow {
    pub ack_bytes: u64,
    pub throughput_bytes_per_second: f64,
    pub ewma_bytes_per_second: f64,
    pub errors: u32,
    pub timeouts_or_resets: u32,
    pub average_rtt: Duration,
    pub active: usize,
    pub pending: usize,
}

#[derive(Debug)]
pub struct MetricsCollector {
    started: Instant,
    last_roll: Instant,
    ack_bytes: u64,
    errors: u32,
    timeouts_or_resets: u32,
    rtt_total: Duration,
    rtt_samples: u32,
    ewma: f64,
}

impl MetricsCollector {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last_roll: now,
            ack_bytes: 0,
            errors: 0,
            timeouts_or_resets: 0,
            rtt_total: Duration::ZERO,
            rtt_samples: 0,
            ewma: 0.0,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn record_ack(&mut self, bytes: u64, rtt: Duration) {
        self.ack_bytes = self.ack_bytes.saturating_add(bytes);
        self.rtt_total += rtt;
        self.rtt_samples += 1;
    }

    pub fn record_error(&mut self, timeout_or_reset: bool) {
        self.errors += 1;
        if timeout_or_reset {
            self.timeouts_or_resets += 1;
        }
    }

    pub fn roll(&mut self, active: usize, pending: usize) -> MetricsWindow {
        let elapsed = self.last_roll.elapsed().as_secs_f64().max(0.001);
        let throughput = self.ack_bytes as f64 / elapsed;
        self.ewma = if self.ewma == 0.0 {
            throughput
        } else {
            self.ewma * 0.7 + throughput * 0.3
        };
        let average_rtt = if self.rtt_samples == 0 {
            Duration::ZERO
        } else {
            self.rtt_total / self.rtt_samples
        };
        let window = MetricsWindow {
            ack_bytes: self.ack_bytes,
            throughput_bytes_per_second: throughput,
            ewma_bytes_per_second: self.ewma,
            errors: self.errors,
            timeouts_or_resets: self.timeouts_or_resets,
            average_rtt,
            active,
            pending,
        };
        self.last_roll = Instant::now();
        self.ack_bytes = 0;
        self.errors = 0;
        self.timeouts_or_resets = 0;
        self.rtt_total = Duration::ZERO;
        self.rtt_samples = 0;
        window
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}
