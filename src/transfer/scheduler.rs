use super::events::TransferSegment;
use super::metrics::MetricsWindow;
use super::range::{Interval, IntervalSet, divide_interval};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

pub const INITIAL_CONCURRENCY: usize = 8;
pub const MAX_CONCURRENCY: usize = 20;
pub const NORMAL_MIN_RANGE: u64 = 512 * 1024;
pub const TAIL_75_MIN_RANGE: u64 = 256 * 1024;
pub const TAIL_90_MIN_RANGE: u64 = 128 * 1024;
pub const SCHEDULER_TICK: Duration = Duration::from_millis(200);
pub const DIAGNOSTIC_WINDOW: Duration = Duration::from_secs(1);
const SCALE_UP_STEP: usize = 2;
const BASELINE_MIN_WINDOWS: usize = 3;
const SPEED_MEDIAN_WINDOWS: usize = 5;
const TRIAL_CLEAN_WINDOWS: u8 = 5;
const TRIAL_NEGATIVE_WINDOWS: u8 = 2;
const TRIAL_MIN_GAIN: f64 = 0.04;
const TRIAL_NEGATIVE_RATIO: f64 = 0.94;
const TRIAL_SPLIT_TIMEOUT: Duration = Duration::from_secs(5);
const SPLIT_COOLDOWN: Duration = Duration::from_secs(10);

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SplitTrialKind {
    ScaleUp,
    TailRescue,
}

#[derive(Clone, Debug)]
pub struct SplitTrial {
    pub kind: SplitTrialKind,
    pub baseline_ewma: f64,
    pub baseline_window_speed: f64,
    /// Target captured before the trial changes concurrency.
    pub before_target: usize,
    /// Kept for source compatibility with the first scheduler prototype.
    pub previous_target: usize,
    pub after_target: usize,
    /// Number of workers created solely for this trial. A rejected trial must
    /// retire this many workers after restoring `before_target`.
    pub introduced_workers: usize,
    pub child_range_ids: Vec<u64>,
    pub root_range_id: u64,
    pub paused_range_id: Option<u64>,
    pub awaiting_split: bool,
    pub pause_requested: bool,
    pub started_at: Instant,
    pub observing_since: Option<Instant>,
    pub last_observation_at: Option<Instant>,
    pub clean_windows: u8,
    pub negative_windows: u8,
    pub window_speed_samples: Vec<f64>,
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
    completed: IntervalSet,
    receiver_limit: usize,
    target: usize,
    next_range_id: u64,
    pending: VecDeque<RangeTask>,
    active: HashMap<u64, ActiveRange>,
    cooldowns: HashMap<u64, Instant>,
    split_trial: Option<SplitTrial>,
    degradation_windows: u8,
    last_ewma: f64,
    last_window_speed: f64,
    recent_window_speeds: VecDeque<f64>,
    recent_errors: bool,
    rollback_planned: bool,
    rollback_ranges: HashSet<u64>,
    rollback_intervals: Vec<Interval>,
    rollback_root_range_id: Option<u64>,
}

impl AdaptiveScheduler {
    pub fn new(total_bytes: u64, missing: Vec<Interval>, receiver_limit: usize) -> Self {
        let receiver_limit = receiver_limit.clamp(1, MAX_CONCURRENCY);
        let missing_set = IntervalSet::from_intervals(missing.iter().copied());
        let completed = IntervalSet::from_intervals(missing_set.complement(total_bytes));
        let mut scheduler = Self {
            total_bytes,
            durable_bytes: completed.total_len(),
            completed,
            receiver_limit,
            target: INITIAL_CONCURRENCY.min(receiver_limit),
            next_range_id: 1,
            pending: VecDeque::new(),
            active: HashMap::new(),
            cooldowns: HashMap::new(),
            split_trial: None,
            degradation_windows: 0,
            last_ewma: 0.0,
            last_window_speed: 0.0,
            recent_window_speeds: VecDeque::with_capacity(SPEED_MEDIAN_WINDOWS),
            recent_errors: false,
            rollback_planned: false,
            rollback_ranges: HashSet::new(),
            rollback_intervals: Vec::new(),
            rollback_root_range_id: None,
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

    #[allow(clippy::too_many_arguments)]
    fn tail_trial(
        target: usize,
        baseline_ewma: f64,
        baseline_window_speed: f64,
        root_range_id: u64,
        paused_range_id: Option<u64>,
        child_range_ids: Vec<u64>,
        awaiting_split: bool,
    ) -> SplitTrial {
        let now = Instant::now();
        SplitTrial {
            kind: SplitTrialKind::TailRescue,
            baseline_ewma,
            baseline_window_speed,
            before_target: target,
            previous_target: target,
            after_target: target,
            introduced_workers: 0,
            child_range_ids,
            root_range_id,
            paused_range_id,
            awaiting_split,
            pause_requested: paused_range_id.is_some(),
            started_at: now,
            observing_since: (!awaiting_split).then_some(now),
            last_observation_at: (!awaiting_split).then_some(now),
            clean_windows: 0,
            negative_windows: 0,
            window_speed_samples: Vec::with_capacity(TRIAL_CLEAN_WINDOWS as usize),
        }
    }

    fn scale_up_trial(
        before_target: usize,
        after_target: usize,
        baseline_ewma: f64,
        baseline_window_speed: f64,
        awaiting_split: bool,
    ) -> SplitTrial {
        let now = Instant::now();
        SplitTrial {
            kind: SplitTrialKind::ScaleUp,
            baseline_ewma,
            baseline_window_speed,
            before_target,
            previous_target: before_target,
            after_target,
            introduced_workers: after_target.saturating_sub(before_target),
            child_range_ids: Vec::new(),
            root_range_id: 0,
            paused_range_id: None,
            awaiting_split,
            pause_requested: false,
            started_at: now,
            observing_since: (!awaiting_split).then_some(now),
            last_observation_at: (!awaiting_split).then_some(now),
            clean_windows: 0,
            negative_windows: 0,
            window_speed_samples: Vec::with_capacity(TRIAL_CLEAN_WINDOWS as usize),
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
    pub fn segment_snapshot(&self) -> Vec<TransferSegment> {
        let mut segments = Vec::with_capacity(self.completed.as_slice().len() + self.active.len());
        segments.extend(
            self.completed
                .as_slice()
                .iter()
                .map(|interval| TransferSegment {
                    start: interval.start,
                    end: interval.end,
                    downloaded_end: interval.end,
                    running: false,
                }),
        );
        segments.extend(self.active.values().map(|active| {
            TransferSegment {
                start: active.task.interval.start,
                end: active.task.interval.end,
                downloaded_end: active
                    .task
                    .interval
                    .start
                    .saturating_add(active.bytes_sent)
                    .min(active.task.interval.end),
                running: true,
            }
        }));
        segments.sort_unstable_by_key(|segment| (segment.start, segment.end, segment.running));
        segments
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
        self.completed.insert(committed);
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
            // A split should create only enough pieces to occupy currently idle
            // worker slots. Dividing solely by minimum range can otherwise
            // create dozens of tiny tail tasks in one tick.
            let minimum = self.minimum_range();
            let available_slots = self.target.saturating_sub(self.active.len()).max(1);
            let pieces_by_size = remainder.len().div_ceil(minimum).max(1) as usize;
            let desired_pieces = available_slots.min(pieces_by_size).max(1);
            let piece_size = remainder
                .len()
                .div_ceil(desired_pieces as u64)
                .max(minimum.min(remainder.len()));
            let mut children = divide_interval(remainder, piece_size);
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

            let belongs_to_trial = self.split_trial.as_ref().is_some_and(|trial| {
                trial.paused_range_id == Some(range_id) && trial.awaiting_split
            });
            if belongs_to_trial {
                let trial = self.split_trial.as_mut().expect("trial checked above");
                trial.child_range_ids = ids;
                trial.awaiting_split = false;
                trial.pause_requested = false;
                trial.observing_since = Some(Instant::now());
                trial.last_observation_at = trial.observing_since;
                trial.started_at = Instant::now();
                trial.clean_windows = 0;
                trial.negative_windows = 0;
                trial.window_speed_samples.clear();
            } else if self.split_trial.is_none() && ids.len() > 1 && active.task.attempts == 0 {
                // Compatibility path for callers that used
                // `tail_rescue_candidate` and issued Pause directly.
                self.split_trial = Some(Self::tail_trial(
                    self.target,
                    self.last_ewma,
                    self.last_window_speed,
                    active.task.root_range_id,
                    Some(range_id),
                    ids,
                    false,
                ));
            }
        } else {
            // The range may have completed between a Pause request and the
            // worker observing it. Release the structural action so a scale-up
            // trial can select another range; a tail-only trial is already done.
            let completed_requested_split = self
                .split_trial
                .as_ref()
                .is_some_and(|trial| trial.paused_range_id == Some(range_id));
            if completed_requested_split {
                if self
                    .split_trial
                    .as_ref()
                    .is_some_and(|trial| trial.kind == SplitTrialKind::TailRescue)
                {
                    self.split_trial = None;
                } else if let Some(trial) = self.split_trial.as_mut() {
                    trial.paused_range_id = None;
                    trial.pause_requested = false;
                    trial.root_range_id = 0;
                    trial.started_at = Instant::now();
                }
            }
        }
        Some(committed)
    }

    /// Removes pending trial children and returns every active child together
    /// with the worker that must be paused. Returning the pair atomically from
    /// this scheduler lock avoids a second best-effort lookup that could miss
    /// a pause under lock contention and leave rollback permanently waiting.
    pub fn start_rollback(&mut self, child_range_ids: &[u64]) -> Vec<(usize, u64)> {
        self.rollback_planned = false;
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
        self.rollback_ranges.extend(
            ids.into_iter()
                .filter(|range_id| self.active.contains_key(range_id)),
        );
        let mut pauses: Vec<_> = self
            .rollback_ranges
            .iter()
            .filter_map(|range_id| {
                self.worker_for_range(*range_id)
                    .map(|worker_id| (worker_id, *range_id))
            })
            .collect();
        pauses.sort_unstable();
        self.finish_rollback_if_ready();
        pauses
    }

    fn finish_rollback_if_ready(&mut self) {
        if !self.rollback_ranges.is_empty() {
            return;
        }
        if self.rollback_intervals.is_empty() {
            self.rollback_root_range_id = None;
            return;
        }
        let root_range_id = self.rollback_root_range_id.take().unwrap_or(0);
        let merged = super::range::IntervalSet::from_intervals(self.rollback_intervals.drain(..));
        let intervals = merged.as_slice().to_vec();
        for interval in intervals {
            let task = self.new_task(interval, 0, root_range_id);
            self.pending.push_back(task);
        }
    }

    pub fn fail(&mut self, range_id: u64) -> bool {
        let Some(active) = self.active.remove(&range_id) else {
            return false;
        };
        self.recent_errors = true;

        if self.rollback_ranges.remove(&range_id) {
            // Preserve the original range id and mark the attempt ambiguous so
            // the sender performs RangeStatus reconciliation before resending.
            // Folding it into a fresh merged task would lose that signal and
            // could collide with a prefix the receiver already made durable.
            let mut task = active.task;
            task.attempts = task.attempts.saturating_add(1);
            let retrying = task.attempts <= 3;
            if retrying {
                self.pending.push_back(task);
            }
            self.finish_rollback_if_ready();
            return retrying;
        }

        let failed_requested_split = self
            .split_trial
            .as_ref()
            .is_some_and(|trial| trial.paused_range_id == Some(range_id));
        if failed_requested_split && let Some(trial) = self.split_trial.as_mut() {
            // Keep paused_range_id/awaiting_split. A retry may synthesize
            // the missing ACK through RangeStatus and must still bind the
            // resulting children to this exact trial.
            trial.pause_requested = false;
        }

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

    /// Runs on the 200 ms cadence. This method only emits structural work: it
    /// never decides whether another worker improved throughput.
    pub fn on_fast_tick(&mut self) -> SchedulerDecision {
        let mut decision = SchedulerDecision::default();
        if self.rollback_planned || !self.rollback_ranges.is_empty() || self.recent_errors {
            return decision;
        }

        if self.split_trial.is_some() {
            let should_request_split = self.split_trial.as_ref().is_some_and(|trial| {
                trial.awaiting_split
                    && !trial.pause_requested
                    && self.pending.is_empty()
                    && self.active.len() < self.target
            });
            if !should_request_split {
                return decision;
            }

            let retrying_paused_range = self
                .split_trial
                .as_ref()
                .and_then(|trial| trial.paused_range_id)
                .filter(|range_id| self.active.contains_key(range_id));
            let Some(range_id) = retrying_paused_range.or_else(|| self.longest_splittable_range())
            else {
                return decision;
            };
            let root_range_id = self
                .active
                .get(&range_id)
                .map(|active| active.task.root_range_id)
                .unwrap_or(range_id);
            let trial = self.split_trial.as_mut().expect("trial checked above");
            trial.root_range_id = root_range_id;
            trial.paused_range_id = Some(range_id);
            trial.pause_requested = true;
            decision.pause_for_split = Some(range_id);
            return decision;
        }

        if !self.tail_rescue_ready() {
            return decision;
        }
        let Some(range_id) = self.longest_splittable_range() else {
            return decision;
        };
        let root_range_id = self
            .active
            .get(&range_id)
            .map(|active| active.task.root_range_id)
            .unwrap_or(range_id);
        self.split_trial = Some(Self::tail_trial(
            self.target,
            self.last_ewma,
            self.recent_window_speed_median(),
            root_range_id,
            Some(range_id),
            Vec::new(),
            true,
        ));
        decision.pause_for_split = Some(range_id);
        decision
    }

    /// Runs on the 1 second cadence. It evaluates throughput and changes the
    /// target worker count, leaving Pause/split selection to `on_fast_tick`.
    pub fn on_diagnostic_window(&mut self, window: &MetricsWindow) -> SchedulerDecision {
        let mut decision = SchedulerDecision::default();
        self.recent_errors = window.errors > 0;
        let ewma = diagnostic_ewma(window);
        let window_speed = diagnostic_window_speed(window);

        let mut resolved_trial = false;
        let mut trial_outcome = TrialOutcome::Pending;
        if let Some(trial) = self.split_trial.as_mut() {
            if window.errors > 0 || window.timeouts_or_resets > 0 {
                trial_outcome = TrialOutcome::Rollback;
            } else if trial.awaiting_split {
                if trial.started_at.elapsed() >= TRIAL_SPLIT_TIMEOUT {
                    trial_outcome = TrialOutcome::Rollback;
                }
            } else {
                let complete_window = trial
                    .last_observation_at
                    .is_none_or(|sampled_at| sampled_at.elapsed() >= DIAGNOSTIC_WINDOW);
                if complete_window {
                    trial.last_observation_at = Some(Instant::now());
                    trial.clean_windows = trial.clean_windows.saturating_add(1);
                    trial.window_speed_samples.push(window_speed);
                    if trial.window_speed_samples.len() > SPEED_MEDIAN_WINDOWS {
                        trial.window_speed_samples.remove(0);
                    }
                    let negative = trial_is_negative(
                        ewma,
                        window_speed,
                        trial.baseline_ewma,
                        trial.baseline_window_speed,
                        TRIAL_NEGATIVE_RATIO,
                    );
                    if negative {
                        trial.negative_windows = trial.negative_windows.saturating_add(1);
                    } else {
                        trial.negative_windows = 0;
                    }

                    if trial.negative_windows >= TRIAL_NEGATIVE_WINDOWS {
                        trial_outcome = TrialOutcome::Rollback;
                    } else if trial.clean_windows >= TRIAL_CLEAN_WINDOWS {
                        let median_window_speed = median_speed(&trial.window_speed_samples);
                        let gained = trial_gained_enough(
                            ewma,
                            median_window_speed,
                            trial.baseline_ewma,
                            trial.baseline_window_speed,
                        );
                        trial_outcome = if gained {
                            TrialOutcome::Accept
                        } else {
                            TrialOutcome::Rollback
                        };
                    }
                }
            }
        }

        match trial_outcome {
            TrialOutcome::Accept => {
                if let Some(trial) = self.split_trial.take() {
                    self.recent_window_speeds.clear();
                    self.recent_window_speeds.extend(
                        trial
                            .window_speed_samples
                            .into_iter()
                            .filter(|speed| *speed > 0.0),
                    );
                }
                resolved_trial = true;
            }
            TrialOutcome::Rollback => {
                self.reject_trial(&mut decision);
                resolved_trial = true;
            }
            TrialOutcome::Pending => {}
        }

        // Never overlap trials or combine a trial resolution with another
        // concurrency adjustment in the same diagnostic window.
        if self.split_trial.is_some() || resolved_trial || self.rollback_planned {
            self.last_ewma = ewma;
            self.last_window_speed = window_speed;
            return decision;
        }

        if self.last_ewma > 0.0 && ewma < self.last_ewma * 0.80 {
            self.degradation_windows = self.degradation_windows.saturating_add(1);
        } else {
            self.degradation_windows = 0;
        }

        if window.timeouts_or_resets >= 2 || self.degradation_windows >= 2 {
            let old = self.target;
            self.target = self.target.saturating_sub(1).max(1);
            decision.retire_workers = old - self.target;
            self.degradation_windows = 0;
        } else {
            self.record_recent_window_speed(window_speed);
        }

        if decision.retire_workers == 0
            && !self.recent_errors
            && self.rollback_ranges.is_empty()
            && self.has_enough_remaining_data()
            && self.target < self.receiver_limit
            && self.active.len() >= self.target
            && (!self.pending.is_empty() || self.longest_splittable_range().is_some())
            && ewma > 0.0
            && self.recent_window_speeds.len() >= BASELINE_MIN_WINDOWS
        {
            let before_target = self.target;
            let after_target = before_target
                .saturating_add(SCALE_UP_STEP)
                .min(self.receiver_limit);
            let awaiting_split = self.pending.is_empty();
            self.split_trial = Some(Self::scale_up_trial(
                before_target,
                after_target,
                ewma,
                self.recent_window_speed_median(),
                awaiting_split,
            ));
            self.target = after_target;
            decision.spawn_workers = after_target.saturating_sub(before_target);
        }

        self.last_ewma = ewma;
        self.last_window_speed = window_speed;
        decision
    }

    /// Backwards-compatible alias for the original 1 second API.
    pub fn on_window(&mut self, window: &MetricsWindow) -> SchedulerDecision {
        self.on_diagnostic_window(window)
    }

    pub fn tail_rescue_candidate(&self) -> Option<u64> {
        if !self.tail_rescue_ready() || self.split_trial.is_some() {
            return None;
        }
        self.longest_splittable_range()
    }

    fn tail_rescue_ready(&self) -> bool {
        let progress_at_least_75 = self.total_bytes == 0
            || self.durable_bytes.saturating_mul(4) >= self.total_bytes.saturating_mul(3);
        self.pending.is_empty()
            && self.active.len() < self.target
            && (progress_at_least_75 || self.active.len() < INITIAL_CONCURRENCY)
            && self.split_trial.is_none()
            && !self.recent_errors
            && !self.rollback_planned
            && self.rollback_ranges.is_empty()
    }

    fn reject_trial(&mut self, decision: &mut SchedulerDecision) {
        let Some(trial) = self.split_trial.take() else {
            return;
        };
        self.target = trial.before_target.min(self.receiver_limit).max(1);
        decision.retire_workers = decision
            .retire_workers
            .saturating_add(trial.introduced_workers);
        decision.rollback_children = trial.child_range_ids;
        if trial.root_range_id != 0 {
            self.cooldowns
                .insert(trial.root_range_id, Instant::now() + SPLIT_COOLDOWN);
        }
        self.rollback_root_range_id = (trial.root_range_id != 0).then_some(trial.root_range_id);
        self.rollback_planned = !decision.rollback_children.is_empty();
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
                .saturating_mul((self.target + SCALE_UP_STEP) as u64)
    }

    fn record_recent_window_speed(&mut self, speed: f64) {
        if speed <= 0.0 {
            return;
        }
        if self.recent_window_speeds.len() >= SPEED_MEDIAN_WINDOWS {
            self.recent_window_speeds.pop_front();
        }
        self.recent_window_speeds.push_back(speed);
    }

    fn recent_window_speed_median(&self) -> f64 {
        if self.recent_window_speeds.is_empty() {
            self.last_window_speed
        } else {
            median_speed(
                &self
                    .recent_window_speeds
                    .iter()
                    .copied()
                    .collect::<Vec<_>>(),
            )
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrialOutcome {
    Pending,
    Accept,
    Rollback,
}

fn diagnostic_ewma(window: &MetricsWindow) -> f64 {
    if window.wire_reporting_enabled {
        window.useful_wire_ewma_bytes_per_second
    } else {
        window.ewma_bytes_per_second
    }
}

fn diagnostic_window_speed(window: &MetricsWindow) -> f64 {
    if window.wire_reporting_enabled {
        window.useful_wire_bytes_per_second
    } else {
        window.throughput_bytes_per_second
    }
}

fn below_ratio(value: f64, baseline: f64, ratio: f64) -> bool {
    baseline > 0.0 && value < baseline * ratio
}

fn trial_is_negative(
    ewma: f64,
    window_speed: f64,
    baseline_ewma: f64,
    baseline_window_speed: f64,
    ratio: f64,
) -> bool {
    let has_baseline = baseline_ewma > 0.0 || baseline_window_speed > 0.0;
    has_baseline
        && (baseline_ewma <= 0.0 || below_ratio(ewma, baseline_ewma, ratio))
        && (baseline_window_speed <= 0.0 || below_ratio(window_speed, baseline_window_speed, ratio))
}

fn trial_gained_enough(
    ewma: f64,
    window_speed: f64,
    baseline_ewma: f64,
    baseline_window_speed: f64,
) -> bool {
    if baseline_ewma > 0.0 || baseline_window_speed > 0.0 {
        (baseline_ewma > 0.0 && ewma >= baseline_ewma * (1.0 + TRIAL_MIN_GAIN))
            || (baseline_window_speed > 0.0
                && window_speed >= baseline_window_speed * (1.0 + TRIAL_MIN_GAIN))
    } else {
        ewma > 0.0 || window_speed > 0.0
    }
}

fn median_speed(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
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
            wire_bytes: speed as u64,
            retransmitted_bytes: 0,
            wire_reporting_enabled: true,
            throughput_bytes_per_second: speed,
            durable_bytes_per_second: speed,
            wire_bytes_per_second: speed,
            useful_wire_bytes_per_second: speed,
            ewma_bytes_per_second: ewma,
            wire_ewma_bytes_per_second: ewma,
            useful_wire_ewma_bytes_per_second: ewma,
            errors,
            timeouts_or_resets: errors,
            average_rtt: Duration::from_millis(2),
            active: 8,
            pending: 8,
        }
    }

    fn start_all_initial_ranges(scheduler: &mut AdaptiveScheduler) -> Vec<RangeTask> {
        let target = scheduler.target();
        (0..target)
            .map(|worker_id| scheduler.take_next(worker_id).unwrap())
            .collect()
    }

    fn make_ranges_splittable(scheduler: &mut AdaptiveScheduler) {
        for active in scheduler.active.values_mut() {
            active.started_at = Instant::now() - Duration::from_secs(2);
            active.ewma_speed_bytes_per_second = 1_000.0;
        }
    }

    fn age_trial_window(scheduler: &mut AdaptiveScheduler) {
        scheduler.split_trial.as_mut().unwrap().last_observation_at =
            Some(Instant::now() - DIAGNOSTIC_WINDOW);
    }

    fn begin_scale_trial(
        scheduler: &mut AdaptiveScheduler,
        ewma: f64,
        speed: f64,
    ) -> SchedulerDecision {
        let mut decision = SchedulerDecision::default();
        for _ in 0..BASELINE_MIN_WINDOWS {
            decision = scheduler.on_diagnostic_window(&window(ewma, speed, 0));
        }
        decision
    }

    #[test]
    fn scheduler_starts_at_eight_and_caps_at_twenty() {
        let scheduler = AdaptiveScheduler::new(
            128 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 128 * NORMAL_MIN_RANGE,
            }],
            99,
        );
        assert_eq!(scheduler.target(), 8);
        assert_eq!(scheduler.receiver_limit, MAX_CONCURRENCY);
    }

    #[test]
    fn segment_snapshot_combines_resumed_and_active_byte_ranges() {
        let mut scheduler = AdaptiveScheduler::new(
            1_000,
            vec![Interval {
                start: 200,
                end: 1_000,
            }],
            8,
        );
        let task = scheduler.take_next(0).unwrap();
        scheduler.record_in_flight(task.range_id, 300);

        assert_eq!(
            scheduler.segment_snapshot(),
            vec![
                TransferSegment {
                    start: 0,
                    end: 200,
                    downloaded_end: 200,
                    running: false,
                },
                TransferSegment {
                    start: 200,
                    end: 1_000,
                    downloaded_end: 500,
                    running: true,
                },
            ]
        );

        scheduler.acknowledge(task.range_id, task.interval.end);
        assert_eq!(
            scheduler.segment_snapshot(),
            vec![TransferSegment {
                start: 0,
                end: 1_000,
                downloaded_end: 1_000,
                running: false,
            }]
        );
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
    fn diagnostic_scale_trial_captures_target_before_fast_split() {
        let mut scheduler = AdaptiveScheduler::new(
            128 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 128 * NORMAL_MIN_RANGE,
            }],
            12,
        );
        start_all_initial_ranges(&mut scheduler);
        make_ranges_splittable(&mut scheduler);

        let diagnostic = begin_scale_trial(&mut scheduler, 1_000.0, 1_000.0);
        assert_eq!(diagnostic.spawn_workers, 2);
        assert_eq!(diagnostic.pause_for_split, None);
        assert_eq!(scheduler.target(), 10);
        let trial = scheduler.split_trial.as_ref().unwrap();
        assert_eq!(trial.kind, SplitTrialKind::ScaleUp);
        assert_eq!(trial.before_target, 8);
        assert_eq!(trial.previous_target, 8);
        assert_eq!(trial.after_target, 10);
        assert_eq!(trial.introduced_workers, 2);

        let first_fast = scheduler.on_fast_tick();
        assert!(first_fast.pause_for_split.is_some());
        assert_eq!(scheduler.on_fast_tick().pause_for_split, None);
        assert!(scheduler.split_trial.as_ref().unwrap().pause_requested);
    }

    #[test]
    fn no_gain_rolls_back_target_and_retires_introduced_worker() {
        let mut scheduler = AdaptiveScheduler::new(
            128 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 128 * NORMAL_MIN_RANGE,
            }],
            12,
        );
        start_all_initial_ranges(&mut scheduler);
        make_ranges_splittable(&mut scheduler);
        begin_scale_trial(&mut scheduler, 1_000.0, 1_000.0);
        let paused = scheduler.on_fast_tick().pause_for_split.unwrap();
        let interval = scheduler.active.get(&paused).unwrap().task.interval;
        scheduler.acknowledge(paused, interval.start + NORMAL_MIN_RANGE);
        let children = scheduler
            .split_trial
            .as_ref()
            .unwrap()
            .child_range_ids
            .clone();
        assert_eq!(children.len(), 3);

        let mut decision = SchedulerDecision::default();
        for _ in 0..TRIAL_CLEAN_WINDOWS {
            age_trial_window(&mut scheduler);
            decision = scheduler.on_diagnostic_window(&window(1_000.0, 1_000.0, 0));
        }

        assert_eq!(scheduler.target(), 8);
        assert_eq!(decision.retire_workers, 2);
        assert_eq!(decision.rollback_children, children);
        assert!(scheduler.rollback_planned);
    }

    #[test]
    fn five_window_median_accepts_a_four_percent_batch_gain() {
        let mut scheduler = AdaptiveScheduler::new(
            128 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 128 * NORMAL_MIN_RANGE,
            }],
            MAX_CONCURRENCY,
        );
        start_all_initial_ranges(&mut scheduler);
        make_ranges_splittable(&mut scheduler);
        begin_scale_trial(&mut scheduler, 1_000.0, 1_000.0);
        let paused = scheduler.on_fast_tick().pause_for_split.unwrap();
        let interval = scheduler.active.get(&paused).unwrap().task.interval;
        scheduler.acknowledge(paused, interval.start + NORMAL_MIN_RANGE);

        for speed in [1_040.0, 900.0, 1_040.0, 1_040.0, 1_040.0] {
            age_trial_window(&mut scheduler);
            let decision = scheduler.on_diagnostic_window(&window(1_000.0, speed, 0));
            assert!(decision.rollback_children.is_empty());
        }

        assert_eq!(scheduler.target(), 10);
        assert!(scheduler.split_trial.is_none());
        assert_eq!(scheduler.recent_window_speed_median(), 1_040.0);
    }

    #[test]
    fn successful_batch_trials_can_reach_twenty_workers() {
        let mut scheduler = AdaptiveScheduler::new(
            1_024 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 1_024 * NORMAL_MIN_RANGE,
            }],
            99,
        );
        start_all_initial_ranges(&mut scheduler);
        make_ranges_splittable(&mut scheduler);
        let mut baseline = 1_000.0;

        for expected_target in (10..=MAX_CONCURRENCY).step_by(SCALE_UP_STEP) {
            let decision = if expected_target == 10 {
                begin_scale_trial(&mut scheduler, baseline, baseline)
            } else {
                scheduler.on_diagnostic_window(&window(baseline, baseline, 0))
            };
            assert_eq!(decision.spawn_workers, SCALE_UP_STEP);
            assert_eq!(scheduler.target(), expected_target);

            let paused = scheduler.on_fast_tick().pause_for_split.unwrap();
            let interval = scheduler.active.get(&paused).unwrap().task.interval;
            scheduler.acknowledge(paused, interval.start + NORMAL_MIN_RANGE);
            let mut worker_id = 100;
            while scheduler.active_len() < scheduler.target() {
                scheduler.take_next(worker_id).unwrap();
                worker_id += 1;
            }

            let gained = baseline * (1.0 + TRIAL_MIN_GAIN);
            for _ in 0..TRIAL_CLEAN_WINDOWS {
                age_trial_window(&mut scheduler);
                scheduler.on_diagnostic_window(&window(baseline, gained, 0));
            }
            assert!(scheduler.split_trial.is_none());
            assert_eq!(scheduler.target(), expected_target);
            baseline = gained;
        }

        let decision = scheduler.on_diagnostic_window(&window(baseline, baseline, 0));
        assert_eq!(scheduler.target(), MAX_CONCURRENCY);
        assert_eq!(decision.spawn_workers, 0);
    }

    #[test]
    fn an_error_rejects_a_trial_immediately() {
        let mut scheduler = AdaptiveScheduler::new(
            128 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 128 * NORMAL_MIN_RANGE,
            }],
            12,
        );
        start_all_initial_ranges(&mut scheduler);
        make_ranges_splittable(&mut scheduler);
        begin_scale_trial(&mut scheduler, 1_000.0, 1_000.0);

        let decision = scheduler.on_diagnostic_window(&window(1_000.0, 1_000.0, 1));

        assert_eq!(scheduler.target(), 8);
        assert_eq!(decision.retire_workers, 2);
        assert!(scheduler.split_trial.is_none());
    }

    #[test]
    fn a_pending_trial_prevents_overlapping_scale_trials() {
        let mut scheduler = AdaptiveScheduler::new(
            128 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 128 * NORMAL_MIN_RANGE,
            }],
            12,
        );
        start_all_initial_ranges(&mut scheduler);
        make_ranges_splittable(&mut scheduler);
        let first = begin_scale_trial(&mut scheduler, 1_000.0, 1_000.0);
        let second = scheduler.on_diagnostic_window(&window(2_000.0, 2_000.0, 0));

        assert_eq!(first.spawn_workers, 2);
        assert_eq!(second.spawn_workers, 0);
        assert_eq!(scheduler.target(), 10);
    }

    #[test]
    fn tail_rescue_requires_an_idle_target_slot_and_deduplicates_pause() {
        let mut scheduler = AdaptiveScheduler::new(
            64 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 64 * NORMAL_MIN_RANGE,
            }],
            8,
        );
        let ranges = start_all_initial_ranges(&mut scheduler);
        make_ranges_splittable(&mut scheduler);
        assert_eq!(scheduler.on_fast_tick().pause_for_split, None);

        let completed = ranges[0].clone();
        scheduler.acknowledge(completed.range_id, completed.interval.end);
        let first = scheduler.on_fast_tick();
        assert!(first.pause_for_split.is_some());
        assert_eq!(scheduler.on_fast_tick().pause_for_split, None);
        assert_eq!(
            scheduler.split_trial.as_ref().unwrap().kind,
            SplitTrialKind::TailRescue
        );
    }

    #[test]
    fn tail_rescue_does_not_undo_a_reduced_target() {
        let mut scheduler = AdaptiveScheduler::new(
            64 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 64 * NORMAL_MIN_RANGE,
            }],
            8,
        );
        start_all_initial_ranges(&mut scheduler);
        make_ranges_splittable(&mut scheduler);
        scheduler.target = 6;
        while scheduler.active.len() > scheduler.target {
            let range_id = *scheduler.active.keys().next().unwrap();
            let end = scheduler.active.get(&range_id).unwrap().task.interval.end;
            scheduler.acknowledge(range_id, end);
        }

        assert_eq!(scheduler.active_len(), scheduler.target());
        assert_eq!(scheduler.on_fast_tick().pause_for_split, None);
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
    fn rollback_keeps_acknowledged_prefix_and_merges_only_remainders() {
        let mut scheduler = AdaptiveScheduler::new(
            8 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 8 * NORMAL_MIN_RANGE,
            }],
            8,
        );
        let root = scheduler.take_next(0).unwrap();
        scheduler.pending.clear();
        scheduler.target = 2;
        scheduler.durable_bytes = scheduler.total_bytes * 91 / 100;
        scheduler.last_ewma = 1_000.0;
        scheduler.active.get_mut(&root.range_id).unwrap().started_at =
            Instant::now() - Duration::from_secs(2);
        let paused = scheduler.on_fast_tick().pause_for_split.unwrap();
        let split_at = root.interval.start + TAIL_90_MIN_RANGE;
        scheduler.acknowledge(paused, split_at);
        let trial = scheduler.split_trial.clone().unwrap();
        assert_eq!(trial.child_range_ids.len(), 2);

        age_trial_window(&mut scheduler);
        let first = scheduler.on_diagnostic_window(&window(700.0, 700.0, 0));
        age_trial_window(&mut scheduler);
        let second = scheduler.on_diagnostic_window(&window(700.0, 700.0, 0));
        assert!(first.rollback_children.is_empty());
        assert_eq!(second.rollback_children, trial.child_range_ids);
        let workers = scheduler.start_rollback(&second.rollback_children);
        assert!(workers.is_empty());
        assert_eq!(scheduler.pending_len(), 1);
        let merged = scheduler.pending.front().unwrap().interval;
        assert_eq!(
            scheduler.pending.front().unwrap().root_range_id,
            trial.root_range_id
        );
        assert_eq!(merged.start, split_at);
        assert_eq!(merged.end, root.interval.end);
    }

    #[test]
    fn failure_during_rollback_cannot_leave_the_wait_set_stuck() {
        let mut scheduler = AdaptiveScheduler::new(
            8 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 8 * NORMAL_MIN_RANGE,
            }],
            8,
        );
        let root = scheduler.take_next(0).unwrap();
        scheduler.pending.clear();
        scheduler.target = 2;
        scheduler.durable_bytes = scheduler.total_bytes * 91 / 100;
        scheduler.last_ewma = 1_000.0;
        scheduler.active.get_mut(&root.range_id).unwrap().started_at =
            Instant::now() - Duration::from_secs(2);
        let paused = scheduler.on_fast_tick().pause_for_split.unwrap();
        let split_at = root.interval.start + TAIL_90_MIN_RANGE;
        scheduler.acknowledge(paused, split_at);
        let children = scheduler
            .split_trial
            .as_ref()
            .unwrap()
            .child_range_ids
            .clone();
        let active_child = scheduler.take_next(7).unwrap();
        scheduler.split_trial = None;

        let workers = scheduler.start_rollback(&children);
        assert_eq!(workers, vec![(7, active_child.range_id)]);
        assert!(scheduler.fail(active_child.range_id));

        assert!(scheduler.rollback_ranges.is_empty());
        assert!(scheduler.rollback_intervals.is_empty());
        assert_eq!(scheduler.pending_len(), 2);
        let retried = scheduler
            .pending
            .iter()
            .find(|task| task.range_id == active_child.range_id)
            .unwrap();
        assert_eq!(retried.attempts, 1);
        let mut intervals: Vec<_> = scheduler.pending.iter().map(|task| task.interval).collect();
        intervals.sort_unstable_by_key(|interval| interval.start);
        assert_eq!(intervals[0].start, split_at);
        assert_eq!(intervals[0].end, intervals[1].start);
        assert_eq!(intervals[1].end, root.interval.end);
    }

    #[test]
    fn ambiguous_split_retry_keeps_its_trial_binding() {
        let mut scheduler = AdaptiveScheduler::new(
            128 * NORMAL_MIN_RANGE,
            vec![Interval {
                start: 0,
                end: 128 * NORMAL_MIN_RANGE,
            }],
            12,
        );
        start_all_initial_ranges(&mut scheduler);
        make_ranges_splittable(&mut scheduler);
        begin_scale_trial(&mut scheduler, 1_000.0, 1_000.0);
        let paused = scheduler.on_fast_tick().pause_for_split.unwrap();
        let original = scheduler.active.get(&paused).unwrap().task.clone();

        assert!(scheduler.fail(paused));
        let trial = scheduler.split_trial.as_ref().unwrap();
        assert_eq!(trial.paused_range_id, Some(paused));
        assert!(trial.awaiting_split);
        assert!(!trial.pause_requested);

        let retry = scheduler.take_next(99).unwrap();
        assert_eq!(retry.range_id, original.range_id);
        assert_eq!(retry.attempts, 1);
        scheduler.acknowledge(retry.range_id, retry.interval.start + 2 * NORMAL_MIN_RANGE);

        let trial = scheduler.split_trial.as_ref().unwrap();
        assert!(!trial.awaiting_split);
        assert!(!trial.child_range_ids.is_empty());
    }
}
