use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct MetricsWindow {
    /// Bytes durably acknowledged by the receiver during this window.
    pub ack_bytes: u64,
    /// Bytes written to the wire during this window. This may be greater than
    /// `ack_bytes` while a large range is still in flight.
    pub wire_bytes: u64,
    /// Bytes sent again because of retries during this window.
    pub retransmitted_bytes: u64,
    /// Whether the producer reports transport writes explicitly. When false,
    /// wire speed intentionally falls back to ACK speed for legacy callers.
    pub wire_reporting_enabled: bool,
    /// Backwards-compatible name for durable (ACK) throughput.
    pub throughput_bytes_per_second: f64,
    pub durable_bytes_per_second: f64,
    pub wire_bytes_per_second: f64,
    /// New (non-retransmitted) bytes written this window. Split trials use
    /// this instead of gross wire speed so retries cannot look like a gain.
    pub useful_wire_bytes_per_second: f64,
    pub ewma_bytes_per_second: f64,
    pub wire_ewma_bytes_per_second: f64,
    pub useful_wire_ewma_bytes_per_second: f64,
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
    wire_bytes: u64,
    wire_reporting_enabled: bool,
    retransmitted_bytes: u64,
    errors: u32,
    timeouts_or_resets: u32,
    rtt_total: Duration,
    rtt_samples: u32,
    ewma: f64,
    wire_ewma: f64,
    useful_wire_ewma: f64,
}

impl MetricsCollector {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last_roll: now,
            ack_bytes: 0,
            wire_bytes: 0,
            wire_reporting_enabled: false,
            retransmitted_bytes: 0,
            errors: 0,
            timeouts_or_resets: 0,
            rtt_total: Duration::ZERO,
            rtt_samples: 0,
            ewma: 0.0,
            wire_ewma: 0.0,
            useful_wire_ewma: 0.0,
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

    /// Records bytes as they are written to the transport. Callers that only
    /// have range-completion ACKs may omit this; `roll` then falls back to the
    /// durable byte count so existing integrations retain useful diagnostics.
    pub fn record_wire(&mut self, bytes: u64) {
        self.wire_reporting_enabled = true;
        self.wire_bytes = self.wire_bytes.saturating_add(bytes);
    }

    pub fn record_retransmitted(&mut self, bytes: u64) {
        self.retransmitted_bytes = self.retransmitted_bytes.saturating_add(bytes);
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
        let effective_wire_bytes = if self.wire_reporting_enabled {
            self.wire_bytes
        } else {
            self.ack_bytes
        };
        let wire_throughput = effective_wire_bytes as f64 / elapsed;
        let useful_wire_bytes = effective_wire_bytes.saturating_sub(self.retransmitted_bytes);
        let useful_wire_throughput = useful_wire_bytes as f64 / elapsed;
        self.ewma = if self.ewma == 0.0 {
            throughput
        } else {
            self.ewma * 0.7 + throughput * 0.3
        };
        self.wire_ewma = if self.wire_ewma == 0.0 {
            wire_throughput
        } else {
            self.wire_ewma * 0.7 + wire_throughput * 0.3
        };
        self.useful_wire_ewma = if self.useful_wire_ewma == 0.0 {
            useful_wire_throughput
        } else {
            self.useful_wire_ewma * 0.7 + useful_wire_throughput * 0.3
        };
        let average_rtt = if self.rtt_samples == 0 {
            Duration::ZERO
        } else {
            self.rtt_total / self.rtt_samples
        };
        let window = MetricsWindow {
            ack_bytes: self.ack_bytes,
            wire_bytes: effective_wire_bytes,
            retransmitted_bytes: self.retransmitted_bytes,
            wire_reporting_enabled: self.wire_reporting_enabled,
            throughput_bytes_per_second: throughput,
            durable_bytes_per_second: throughput,
            wire_bytes_per_second: wire_throughput,
            useful_wire_bytes_per_second: useful_wire_throughput,
            ewma_bytes_per_second: self.ewma,
            wire_ewma_bytes_per_second: self.wire_ewma,
            useful_wire_ewma_bytes_per_second: self.useful_wire_ewma,
            errors: self.errors,
            timeouts_or_resets: self.timeouts_or_resets,
            average_rtt,
            active,
            pending,
        };
        self.last_roll = Instant::now();
        self.ack_bytes = 0;
        self.wire_bytes = 0;
        self.retransmitted_bytes = 0;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_speed_is_available_before_a_large_range_is_acknowledged() {
        let mut metrics = MetricsCollector::new();
        metrics.last_roll = Instant::now() - Duration::from_secs(1);
        metrics.record_wire(1_000);

        let window = metrics.roll(1, 0);

        assert_eq!(window.ack_bytes, 0);
        assert_eq!(window.wire_bytes, 1_000);
        assert_eq!(window.durable_bytes_per_second, 0.0);
        assert!(window.wire_bytes_per_second >= 900.0);
    }

    #[test]
    fn legacy_callers_fall_back_to_ack_bytes_for_wire_diagnostics() {
        let mut metrics = MetricsCollector::new();
        metrics.last_roll = Instant::now() - Duration::from_secs(1);
        metrics.record_ack(2_000, Duration::from_millis(4));
        metrics.record_retransmitted(250);

        let window = metrics.roll(2, 3);

        assert_eq!(window.wire_bytes, 2_000);
        assert_eq!(window.retransmitted_bytes, 250);
        assert_eq!(window.average_rtt, Duration::from_millis(4));
        assert!(window.throughput_bytes_per_second >= 1_800.0);
    }

    #[test]
    fn retransmissions_do_not_count_as_split_trial_gain() {
        let mut metrics = MetricsCollector::new();
        metrics.last_roll = Instant::now() - Duration::from_secs(1);
        metrics.record_wire(12_000);
        metrics.record_retransmitted(4_000);

        let window = metrics.roll(2, 0);

        assert!(window.wire_bytes_per_second >= 11_000.0);
        assert!(window.useful_wire_bytes_per_second >= 7_000.0);
        assert!(window.useful_wire_bytes_per_second < 9_000.0);
    }

    #[test]
    fn explicit_zero_wire_report_does_not_recount_a_late_ack() {
        let mut metrics = MetricsCollector::new();
        metrics.last_roll = Instant::now() - Duration::from_secs(1);
        metrics.record_wire(0);
        metrics.record_ack(2_000, Duration::from_millis(4));

        let window = metrics.roll(1, 0);

        assert_eq!(window.ack_bytes, 2_000);
        assert_eq!(window.wire_bytes, 0);
        assert_eq!(window.wire_bytes_per_second, 0.0);
        assert!(window.durable_bytes_per_second >= 1_800.0);
    }
}
