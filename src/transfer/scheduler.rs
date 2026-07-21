use super::metrics::MetricsWindow;
use super::range::{Interval, divide_interval};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

pub const INITIAL_CONCURRENCY: usize = 8;
pub const MAX_CONCURRENCY: usize = 12;
pub const NORMAL_MIN_RANGE: u64 = 512 * 1024;
pub const TAIL_75_MIN_RANGE: u64 = 256 * 1024;
pub const TAIL_90_MIN_RANGE: u64 = 128 * 1024;
pub const SCHEDULER_TICK: Duration = Duration::from_millis(200);
pub const DIAGNOSTIC_WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct RangeTask {
    pub range_id: u64,
    pub interval: Interval,
    pub generation: u32,
    pub attempts: u8,
    pub root_range_id: u64,
    pub created_at: Instant,
}

#[derive(Clone, Debug)]
pub struct ActiveRange {
    pub task: RangeTask,
    pub worker_id: usize,
    pub committed_end: u64,
    pub started_at: Instant,
    pub bytes_sent: u64,
    pub last_sample_at: Instant,
    pub last_sample_bytes: u64,
    pub ewma_speed_bytes_per_second: f64,
}

#[derive(Clone, Debug)]
pub struct SplitTrial {
    pub baseline_ewma: f64,
    pub baseline_window_speed: f64,
    pub previous_target: usize,
    pub child_range_ids: Vec<u64>,
    pub root_range_id: u64,
    pub started_at: Instant,
    pub clean_windows: u8,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SchedulerDecision {
    pub spawn_workers: usize,
    pub retire_workers: usize,
    pub pause_for_split: Option<u64>,
    pub rollback_children: Vec<u64>,
}

pub struct AdaptiveScheduler {
    total_bytes: u64,
    durable_bytes: u64,
    receiver_limit: usize,
    target: usize,
    next_range_id: u64,
    pending: VecDeque<RangeTask>,
    active: HashMap<u64, ActiveRange>,
    cooldowns: HashMap<u64, Instant>,
    split_trial: Option<SplitTrial>,
    low_gain_windows: u8,
    degradation_windows: u8,
    last_ewma: f64,
    last_scale_baseline: Option<f64>,
    recent_errors: bool,
    rollback_ranges: HashSet<u64>,
    rollback_intervals: Vec<Interval>,
}

impl AdaptiveScheduler {
    pub fn new(total_bytes: u64, missing: Vec<Interval>, receiver_limit: usize) -> Self {
        let receiver_limit = receiver_limit.clamp(1, MAX_CONCURRENCY);
        let mut scheduler = Self {
            total_bytes,
            durable_bytes: total_bytes
                .saturating_sub(missing.iter().map(|range| range.len()).sum()),
            receiver_limit,
            target: INITIAL_CONCURRENCY.min(receiver_limit),
            next_range_id: 1,
            pending: VecDeque::new(),
            active: HashMap::new(),
            cooldowns: HashMap::new(),
            split_trial: None,
            low_gain_windows: 0,
            degradation_windows: 0,
            last_ewma: 0.0,
            last_scale_baseline: None,
            recent_errors: false,
            rollback_ranges: HashSet::new(),
            rollback_intervals: Vec::new(),
        };
        let total_missing: u64 = missing.iter().map(|range| range.len()).sum();
        let initial_range_size = total_missing
            .div_ceil(scheduler.target.max(1) as u64)
            .max(NORMAL_MIN_RANGE);
        for interval in missing {
            for piece in divide_interval(interval, initial_range_size) {
                let task = scheduler.new_task(piece, 0, 0);
                scheduler.pending.push_back(task);
            }
        }
        scheduler.target = scheduler.target.min(scheduler.pending.len().max(1));
        scheduler
    }

    fn new_task(&mut self, interval: Interval, generation: u32, root_range_id: u64) -> RangeTask {
        let range_id = self.next_range_id;
        self.next_range_id += 1;
        RangeTask {
            range_id,
            interval,
            generation,
            attempts: 0,
            root_range_id: if root_range_id == 0 {
                range_id
            } else {
                root_range_id
            },
            created_at: Instant::now(),
        }
    }

    pub fn target(&self) -> usize {
        self.target
    }
    pub fn durable_bytes(&self) -> u64 {
        self.durable_bytes
    }
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
    pub fn active_len(&self) -> usize {
        self.active.len()
    }
    pub fn worker_for_range(&self, range_id: u64) -> Option<usize> {
        self.active.get(&range_id).map(|active| active.worker_id)
    }
    pub fn active_range_ids(&self) -> Vec<u64> {
        self.active.keys().copied().collect()
    }
    pub fn is_complete(&self) -> bool {
        self.durable_bytes >= self.total_bytes && self.pending.is_empty() && self.active.is_empty()
    }

    pub fn take_next(&mut self, worker_id: usize) -> Option<RangeTask> {
        if self.active.len() >= self.target {
            return None;
        }
        let task = self.pending.pop_front()?;
        let now = Instant::now();
        self.active.insert(
            task.range_id,
            ActiveRange {
                task: task.clone(),
                worker_id,
                committed_end: task.interval.start,
                started_at: now,
                bytes_sent: 0,
                last_sample_at: now,
                last_sample_bytes: 0,
                ewma_speed_bytes_per_second: 0.0,
            },
        );
        Some(task)
    }

    pub fn record_in_flight(&mut self, range_id: u64, bytes_sent: u64) {
        if let Some(active) = self.active.get_mut(&range_id) {
            let bytes_sent = bytes_sent.min(active.task.interval.len());
            let now = Instant::now();
            let elapsed = now.duration_since(active.last_sample_at);
            let delta = bytes_sent.saturating_sub(active.last_sample_bytes);
            if delta > 0 && elapsed >= Duration::from_millis(20) {
                let sample = delta as f64 / elapsed.as_secs_f64();
                active.ewma_speed_bytes_per_second = if active.ewma_speed_bytes_per_second == 0.0 {
                    sample
                } else {
                    active.ewma_speed_bytes_per_second * 0.7 + sample * 0.3
                };
                active.last_sample_at = now;
                active.last_sample_bytes = bytes_sent;
            }
            active.bytes_sent = bytes_sent;
        }
    }

    pub fn acknowledge(&mut self, range_id: u64, committed_end: u64) -> Option<Interval> {
        let active = self.active.remove(&range_id)?;
        let end = committed_end.clamp(active.task.interval.start, active.task.interval.end);
        let committed = Interval {
            start: active.task.interval.start,
            end,
        };
        self.durable_bytes = self
            .durable_bytes
            .saturating_add(committed.len())
            .min(self.total_bytes);
        if self.rollback_ranges.remove(&range_id) {
            if end < active.task.interval.end {
                self.rollback_intervals.push(Interval {
                    start: end,
                    end: active.task.interval.end,
                });
            }
            self.finish_rollback_if_ready();
        } else if end < active.task.interval.end {
            let remainder = Interval {
                start: end,
                end: active.task.interval.end,
            };
            let mut children = divide_interval(remainder, self.minimum_range());
            let mut ids = Vec::new();
            for interval in children.drain(..) {
                let child = self.new_task(
                    interval,
                    active.task.generation + 1,
                    active.task.root_range_id,
                );
                ids.push(child.range_id);
                self.pending.push_front(child);
            }
            if ids.len() > 1 {
                self.split_trial = Some(SplitTrial {
                    baseline_ewma: self.last_ewma,
                    baseline_window_speed: self.last_ewma,
                    previous_target: self.target,
                    child_range_ids: ids,
                    root_range_id: active.task.root_range_id,
                    started_at: Instant::now(),
                    clean_windows: 0,
                });
            }
        }
        Some(committed)
    }

    pub fn start_rollback(&mut self, child_range_ids: &[u64]) -> Vec<usize> {
        let ids: HashSet<u64> = child_range_ids.iter().copied().collect();
        let mut retained = VecDeque::with_capacity(self.pending.len());
        while let Some(task) = self.pending.pop_front() {
            if ids.contains(&task.range_id) {
                self.rollback_intervals.push(task.interval);
            } else {
                retained.push_back(task);
            }
        }
        self.pending = retained;
        self.rollback_ranges = ids
            .into_iter()
            .filter(|range_id| self.active.contains_key(range_id))
            .collect();
        let workers = self
            .rollback_ranges
            .iter()
            .filter_map(|range_id| self.worker_for_range(*range_id))
            .collect();
        self.finish_rollback_if_ready();
        workers
    }

    fn finish_rollback_if_ready(&mut self) {
        if !self.rollback_ranges.is_empty() || self.rollback_intervals.is_empty() {
            return;
        }
        let merged = super::range::IntervalSet::from_intervals(self.rollback_intervals.drain(..));
        let intervals = merged.as_slice().to_vec();
        for interval in intervals {
            let task = self.new_task(interval, 0, 0);
            self.pending.push_back(task);
        }
    }

    pub fn fail(&mut self, range_id: u64) -> bool {
        let Some(active) = self.active.remove(&range_id) else {
            return false;
        };
        let mut task = active.task;
        task.attempts += 1;
        if task.attempts <= 3 {
            self.pending.push_back(task);
            true
        } else {
            false
        }
    }

    pub fn in_flight_bytes(&self) -> u64 {
        self.active.values().map(|active| active.bytes_sent).sum()
    }

    pub fn minimum_range(&self) -> u64 {
        let ratio = if self.total_bytes == 0 {
            1.0
        } else {
            self.durable_bytes as f64 / self.total_bytes as f64
        };
        if ratio >= 0.90 {
            TAIL_90_MIN_RANGE
        } else if ratio >= 0.75 {
            TAIL_75_MIN_RANGE
        } else {
            NORMAL_MIN_RANGE
        }
    }

    pub fn on_window(&mut self, window: &MetricsWindow) -> SchedulerDecision {
        self.recent_errors = window.errors > 0;
        let mut decision = SchedulerDecision::default();

        if let Some(trial) = &mut self.split_trial {
            if window.errors == 0 {
                trial.clean_windows += 1;
            }
            let negative = trial.clean_windows >= 2
                && window.ewma_bytes_per_second < trial.baseline_ewma * 0.85
                && window.throughput_bytes_per_second < trial.baseline_window_speed * 0.80;
            let accepted = window.ewma_bytes_per_second >= trial.baseline_ewma * 0.95
                || trial.started_at.elapsed() >= Duration::from_secs(5);
            if negative && window.errors == 0 {
                decision.rollback_children = trial.child_range_ids.clone();
                self.target = trial.previous_target.min(self.receiver_limit);
                self.cooldowns.insert(
                    trial.root_range_id,
                    Instant::now() + Duration::from_secs(10),
                );
                self.split_trial = None;
            } else if accepted {
                self.split_trial = None;
            }
        }

        if self.last_ewma > 0.0 && window.ewma_bytes_per_second < self.last_ewma * 0.80 {
            self.degradation_windows += 1;
        } else {
            self.degradation_windows = 0;
        }
        if window.timeouts_or_resets >= 2 || self.degradation_windows >= 2 {
            let old = self.target;
            self.target = self.target.saturating_sub(1).max(1);
            decision.retire_workers = old - self.target;
            self.degradation_windows = 0;
        } else if !self.recent_errors
            && self.has_enough_remaining_data()
            && self.target < self.receiver_limit
        {
            let gain = self.last_scale_baseline.map(|baseline| {
                if baseline == 0.0 {
                    1.0
                } else {
                    (window.ewma_bytes_per_second - baseline) / baseline
                }
            });
            match gain {
                Some(value) if value < 0.10 => self.low_gain_windows += 1,
                Some(value) if value >= 0.15 => self.low_gain_windows = 0,
                _ => {}
            }
            if self.low_gain_windows < 2 {
                self.last_scale_baseline = Some(window.ewma_bytes_per_second);
                self.target += 1;
                decision.spawn_workers = 1;
            }
        }

        if self.pending.is_empty() && self.active.len() < self.target {
            decision.pause_for_split = self.longest_splittable_range();
        }
        self.last_ewma = window.ewma_bytes_per_second;
        decision
    }

    pub fn tail_rescue_candidate(&self) -> Option<u64> {
        let tail_enabled = self.total_bytes == 0
            || self.durable_bytes.saturating_mul(4) >= self.total_bytes.saturating_mul(3)
            || self.active.len() < INITIAL_CONCURRENCY;
        if !tail_enabled || !self.pending.is_empty() || self.recent_errors {
            return None;
        }
        self.longest_splittable_range()
    }

    fn longest_splittable_range(&self) -> Option<u64> {
        let minimum = self.minimum_range();
        self.active
            .values()
            .filter(|active| active.started_at.elapsed() >= Duration::from_secs(1))
            .filter(|active| {
                active
                    .task
                    .interval
                    .end
                    .saturating_sub(active.task.interval.start + active.bytes_sent)
                    > minimum * 2
            })
            .filter(|active| {
                self.cooldowns
                    .get(&active.task.root_range_id)
                    .is_none_or(|until| *until <= Instant::now())
            })
            .max_by(|left, right| {
                estimated_remaining_seconds(left).total_cmp(&estimated_remaining_seconds(right))
            })
            .map(|active| active.task.range_id)
    }

    fn has_enough_remaining_data(&self) -> bool {
        let remaining = self.total_bytes.saturating_sub(self.durable_bytes);
        remaining
            >= self
                .minimum_range()
                .saturating_mul((self.target + 1) as u64)
    }
}

fn estimated_remaining_seconds(active: &ActiveRange) -> f64 {
    let remaining = active
        .task
        .interval
        .end
        .saturating_sub(active.task.interval.start + active.bytes_sent);
    let average_speed = if active.started_at.elapsed().is_zero() {
        0.0
    } else {
        active.bytes_sent as f64 / active.started_at.elapsed().as_secs_f64()
    };
    let speed = if active.ewma_speed_bytes_per_second > 0.0 {
        active.ewma_speed_bytes_per_second
    } else {
        average_speed
    };
    if speed > 0.0 {
        remaining as f64 / speed
    } else {
        f64::INFINITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(ewma: f64, speed: f64, errors: u32) -> MetricsWindow {
        MetricsWindow {
            ack_bytes: speed as u64,
            throughput_bytes_per_second: speed,
            ewma_bytes_per_second: ewma,
            errors,
            timeouts_or_resets: errors,
            average_rtt: Duration::from_millis(2),
            active: 8,
            pending: 8,
        }
    }

    #[test]
    fn scheduler_starts_at_eight_and_caps_at_twelve() {
        let mut scheduler = AdaptiveScheduler::new(
            128 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 128 * NORMAL_MIN_RANGE,
            }],
            99,
        );
        assert_eq!(scheduler.target(), 8);
        for n in 1..10 {
            scheduler.on_window(&window(
                1000.0 + n as f64 * 200.0,
                1000.0 + n as f64 * 200.0,
                0,
            ));
        }
        assert!(scheduler.target() <= 12);
    }

    #[test]
    fn initial_partition_creates_broad_ranges_for_the_initial_workers() {
        let scheduler = AdaptiveScheduler::new(
            128 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 128 * NORMAL_MIN_RANGE,
            }],
            12,
        );
        assert_eq!(scheduler.pending_len(), INITIAL_CONCURRENCY);
    }

    #[test]
    fn split_candidate_uses_remaining_eta_instead_of_remaining_bytes() {
        let mut scheduler = AdaptiveScheduler::new(
            64 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 64 * NORMAL_MIN_RANGE,
            }],
            8,
        );
        let slow = scheduler.take_next(0).unwrap();
        let fast = scheduler.take_next(1).unwrap();
        for active in scheduler.active.values_mut() {
            active.started_at = Instant::now() - Duration::from_secs(2);
            active.bytes_sent = NORMAL_MIN_RANGE;
            active.ewma_speed_bytes_per_second = if active.task.range_id == slow.range_id {
                1.0
            } else {
                10_000_000.0
            };
        }
        assert_eq!(scheduler.longest_splittable_range(), Some(slow.range_id));
        assert_ne!(slow.range_id, fast.range_id);
    }

    #[test]
    fn repeated_degradation_reduces_target_without_cancelling_active_ranges() {
        let mut scheduler = AdaptiveScheduler::new(
            64 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 64 * NORMAL_MIN_RANGE,
            }],
            12,
        );
        scheduler.last_ewma = 1000.0;
        scheduler.on_window(&window(700.0, 700.0, 0));
        let decision = scheduler.on_window(&window(500.0, 500.0, 0));
        assert_eq!(decision.retire_workers, 1);
        assert_eq!(scheduler.active_len(), 0);
    }

    #[test]
    fn tail_minimum_shrinks_at_thresholds() {
        let mut scheduler = AdaptiveScheduler::new(
            1000,
            vec![Interval {
                start: 0,
                end: 1000,
            }],
            8,
        );
        assert_eq!(scheduler.minimum_range(), NORMAL_MIN_RANGE);
        scheduler.durable_bytes = 750;
        assert_eq!(scheduler.minimum_range(), TAIL_75_MIN_RANGE);
        scheduler.durable_bytes = 900;
        assert_eq!(scheduler.minimum_range(), TAIL_90_MIN_RANGE);
    }

    #[test]
    fn failed_ranges_are_requeued_only_three_times() {
        let mut scheduler = AdaptiveScheduler::new(
            NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: NORMAL_MIN_RANGE,
            }],
            8,
        );
        for attempt in 0..4 {
            let task = scheduler.take_next(0).unwrap();
            assert_eq!(scheduler.fail(task.range_id), attempt < 3);
        }
    }

    #[test]
    fn negative_split_trial_rolls_back_only_unconfirmed_remainders() {
        let mut scheduler = AdaptiveScheduler::new(
            NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: NORMAL_MIN_RANGE,
            }],
            8,
        );
        let root = scheduler.take_next(0).unwrap();
        scheduler.durable_bytes = NORMAL_MIN_RANGE * 91 / 100;
        scheduler.last_ewma = 1_000.0;
        scheduler.active.get_mut(&root.range_id).unwrap().started_at =
            Instant::now() - Duration::from_secs(2);
        assert_eq!(scheduler.tail_rescue_candidate(), Some(root.range_id));
        scheduler.acknowledge(root.range_id, TAIL_90_MIN_RANGE);
        let trial = scheduler.split_trial.clone().unwrap();
        assert!(trial.child_range_ids.len() >= 2);

        let first = scheduler.on_window(&window(700.0, 700.0, 0));
        assert!(first.rollback_children.is_empty());
        let second = scheduler.on_window(&window(700.0, 700.0, 0));
        assert_eq!(second.rollback_children, trial.child_range_ids);
        let workers = scheduler.start_rollback(&second.rollback_children);
        assert!(workers.is_empty());
        assert_eq!(scheduler.pending_len(), 1);
        let merged = scheduler.pending.front().unwrap().interval;
        assert_eq!(merged.start, TAIL_90_MIN_RANGE);
        assert_eq!(merged.end, NORMAL_MIN_RANGE);
    }
}
