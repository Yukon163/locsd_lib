use super::range::{Interval, IntervalSet};
use atomicwrites::{AllowOverwrite, AtomicFile};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const META_SCHEMA_VERSION: u32 = 3;
pub const V3_RESUME_DIR: &str = ".localsend_v3";
const MAX_COMPLETED_INTERVALS: usize = 16_384;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletedInterval {
    pub start: u64,
    pub end: u64,
    pub hash: String,
}

impl CompletedInterval {
    pub const fn interval(&self) -> Interval {
        Interval {
            start: self.start,
            end: self.end,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferMeta {
    pub schema_version: u32,
    pub transfer_key: String,
    pub sender_device_id: String,
    pub filename: String,
    pub total_bytes: u64,
    pub hash_algorithm: String,
    pub content_hash: String,
    pub completed_intervals: Vec<CompletedInterval>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl TransferMeta {
    pub fn new(
        transfer_key: String,
        sender_device_id: String,
        filename: String,
        total_bytes: u64,
        content_hash: String,
    ) -> Self {
        let now = timestamp_ms();
        Self {
            schema_version: META_SCHEMA_VERSION,
            transfer_key,
            sender_device_id,
            filename,
            total_bytes,
            hash_algorithm: "blake3".to_string(),
            content_hash,
            completed_intervals: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn is_compatible_with(&self, expected: &Self) -> bool {
        self.schema_version == META_SCHEMA_VERSION
            && self.transfer_key == expected.transfer_key
            && self.sender_device_id == expected.sender_device_id
            && self.filename == expected.filename
            && self.total_bytes == expected.total_bytes
            && self.hash_algorithm == "blake3"
            && self.content_hash == expected.content_hash
    }

    pub fn completed_set(&self) -> IntervalSet {
        IntervalSet::from_intervals(
            self.completed_intervals
                .iter()
                .map(|entry| entry.interval()),
        )
    }

    pub fn durable_bytes(&self) -> u64 {
        self.completed_set().total_len()
    }

    pub fn commit(&mut self, interval: Interval, hash: String) -> io::Result<()> {
        if interval.end > self.total_bytes || interval.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid committed interval",
            ));
        }
        if self.completed_set().overlaps(interval) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "committed interval overlaps durable data",
            ));
        }
        self.completed_intervals.push(CompletedInterval {
            start: interval.start,
            end: interval.end,
            hash,
        });
        self.completed_intervals.sort_by_key(|entry| entry.start);
        self.updated_at = timestamp_ms();
        self.compact_if_needed();
        Ok(())
    }

    fn compact_if_needed(&mut self) {
        if self.completed_intervals.len() <= MAX_COMPLETED_INTERVALS {
            return;
        }
        let set = self.completed_set();
        self.completed_intervals = set
            .as_slice()
            .iter()
            .map(|interval| CompletedInterval {
                start: interval.start,
                end: interval.end,
                hash: String::new(),
            })
            .collect();
    }
}

pub fn resume_dir(save_dir: impl AsRef<Path>) -> PathBuf {
    save_dir.as_ref().join(V3_RESUME_DIR)
}

pub fn meta_path(save_dir: impl AsRef<Path>, transfer_key: &str) -> PathBuf {
    resume_dir(save_dir).join(format!("{transfer_key}.json"))
}

pub fn part_path(save_dir: impl AsRef<Path>, transfer_key: &str) -> PathBuf {
    resume_dir(save_dir).join(format!("{transfer_key}.part"))
}

pub fn load(save_dir: impl AsRef<Path>, transfer_key: &str) -> io::Result<Option<TransferMeta>> {
    let path = meta_path(save_dir, transfer_key);
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn save_atomic(save_dir: impl AsRef<Path>, meta: &TransferMeta) -> io::Result<()> {
    let directory = resume_dir(save_dir);
    std::fs::create_dir_all(&directory)?;
    let data = serde_json::to_vec(meta).map_err(io::Error::other)?;
    Ok(AtomicFile::new(
        meta_path(directory.parent().unwrap_or(&directory), &meta.transfer_key),
        AllowOverwrite,
    )
    .write(|file| {
        file.write_all(&data)?;
        file.sync_data()
    })?)
}

pub fn remove(save_dir: impl AsRef<Path>, transfer_key: &str) -> io::Result<()> {
    match std::fs::remove_file(meta_path(save_dir, transfer_key)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_round_trip_is_atomic_and_preserves_intervals() {
        let root = std::env::temp_dir().join(format!("locsd-meta-{}", timestamp_ms()));
        let mut meta = TransferMeta::new(
            "key".into(),
            "sender".into(),
            "a|b\n.txt".into(),
            100,
            "hash".into(),
        );
        meta.commit(Interval { start: 0, end: 50 }, "range-hash".into())
            .unwrap();
        save_atomic(&root, &meta).unwrap();
        assert_eq!(load(&root, "key").unwrap(), Some(meta));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn duplicate_or_overlapping_commits_are_rejected() {
        let mut meta = TransferMeta::new(
            "key".into(),
            "sender".into(),
            "file".into(),
            100,
            "hash".into(),
        );
        meta.commit(Interval { start: 0, end: 50 }, "first".into())
            .unwrap();
        assert!(
            meta.commit(Interval { start: 25, end: 75 }, "overlap".into())
                .is_err()
        );
        assert_eq!(meta.durable_bytes(), 50);
    }
}
