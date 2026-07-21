use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interval {
    pub start: u64,
    pub end: u64,
}

impl Interval {
    pub fn new(start: u64, end: u64) -> Result<Self, RangeError> {
        if start > end {
            return Err(RangeError::Reversed { start, end });
        }
        Ok(Self { start, end })
    }

    pub const fn len(self) -> u64 {
        self.end - self.start
    }

    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub const fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub const fn touches(self, other: Self) -> bool {
        self.end == other.start || other.end == self.start
    }

    pub fn split(self, at: u64) -> Result<(Self, Self), RangeError> {
        if at <= self.start || at >= self.end {
            return Err(RangeError::SplitOutside { interval: self, at });
        }
        Ok((
            Self {
                start: self.start,
                end: at,
            },
            Self {
                start: at,
                end: self.end,
            },
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RangeError {
    Reversed { start: u64, end: u64 },
    SplitOutside { interval: Interval, at: u64 },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntervalSet {
    intervals: Vec<Interval>,
}

impl IntervalSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_intervals(intervals: impl IntoIterator<Item = Interval>) -> Self {
        let mut set = Self::new();
        for interval in intervals {
            set.insert(interval);
        }
        set
    }

    pub fn as_slice(&self) -> &[Interval] {
        &self.intervals
    }

    pub fn total_len(&self) -> u64 {
        self.intervals.iter().map(|range| range.len()).sum()
    }

    pub fn covers(&self, interval: Interval) -> bool {
        interval.is_empty()
            || self
                .intervals
                .iter()
                .any(|known| known.start <= interval.start && known.end >= interval.end)
    }

    pub fn overlaps(&self, interval: Interval) -> bool {
        self.intervals.iter().any(|known| known.overlaps(interval))
    }

    pub fn insert(&mut self, interval: Interval) {
        if interval.is_empty() {
            return;
        }

        let mut merged = interval;
        let mut output = Vec::with_capacity(self.intervals.len() + 1);
        let mut inserted = false;
        for current in self.intervals.drain(..) {
            if current.end < merged.start {
                output.push(current);
            } else if merged.end < current.start {
                if !inserted {
                    output.push(merged);
                    inserted = true;
                }
                output.push(current);
            } else {
                merged.start = merged.start.min(current.start);
                merged.end = merged.end.max(current.end);
            }
        }
        if !inserted {
            output.push(merged);
        }
        self.intervals = output;
    }

    pub fn complement(&self, total_bytes: u64) -> Vec<Interval> {
        let mut missing = Vec::new();
        let mut cursor = 0;
        for interval in &self.intervals {
            let start = interval.start.min(total_bytes);
            let end = interval.end.min(total_bytes);
            if cursor < start {
                missing.push(Interval {
                    start: cursor,
                    end: start,
                });
            }
            cursor = cursor.max(end);
            if cursor == total_bytes {
                break;
            }
        }
        if cursor < total_bytes {
            missing.push(Interval {
                start: cursor,
                end: total_bytes,
            });
        }
        missing
    }

    pub fn remove(&mut self, removal: Interval) {
        if removal.is_empty() {
            return;
        }
        let mut output = Vec::with_capacity(self.intervals.len() + 1);
        for current in self.intervals.drain(..) {
            if !current.overlaps(removal) {
                output.push(current);
                continue;
            }
            if current.start < removal.start {
                output.push(Interval {
                    start: current.start,
                    end: removal.start,
                });
            }
            if current.end > removal.end {
                output.push(Interval {
                    start: removal.end,
                    end: current.end,
                });
            }
        }
        self.intervals = output;
    }
}

pub fn divide_interval(interval: Interval, minimum_size: u64) -> Vec<Interval> {
    if interval.is_empty() {
        return Vec::new();
    }
    let minimum_size = minimum_size.max(1);
    let count = interval.len().div_ceil(minimum_size).max(1);
    let width = interval.len().div_ceil(count);
    let mut ranges = Vec::with_capacity(count as usize);
    let mut start = interval.start;
    while start < interval.end {
        let end = (start + width).min(interval.end);
        ranges.push(Interval { start, end });
        start = end;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(start: u64, end: u64) -> Interval {
        Interval::new(start, end).unwrap()
    }

    #[test]
    fn merge_and_complement_are_gap_free() {
        let set = IntervalSet::from_intervals([i(10, 20), i(0, 4), i(4, 10), i(30, 40)]);
        assert_eq!(set.as_slice(), &[i(0, 20), i(30, 40)]);
        assert_eq!(set.complement(50), vec![i(20, 30), i(40, 50)]);
    }

    #[test]
    fn remove_can_split_an_interval() {
        let mut set = IntervalSet::from_intervals([i(0, 100)]);
        set.remove(i(25, 75));
        assert_eq!(set.as_slice(), &[i(0, 25), i(75, 100)]);
    }

    #[test]
    fn divided_ranges_have_no_gaps_or_overlap() {
        for total in 0..4096u64 {
            let ranges = divide_interval(i(0, total), 127);
            let mut cursor = 0;
            for range in ranges {
                assert_eq!(range.start, cursor);
                assert!(range.end > range.start);
                cursor = range.end;
            }
            assert_eq!(cursor, total);
        }
    }

    #[test]
    fn split_and_remerge_round_trip() {
        let original = i(100, 1000);
        let (left, right) = original.split(333).unwrap();
        let set = IntervalSet::from_intervals([right, left]);
        assert_eq!(set.as_slice(), &[original]);
    }
}
