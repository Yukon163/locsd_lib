use super::range::{Interval, IntervalSet};
use atomicwrites::{AllowOverwrite, AtomicFile};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const META_SCHEMA_VERSION: u32 = 6;
pub const V3_RESUME_DIR: &str = ".localsend_v3";
const MIN_COMPATIBLE_SCHEMA_VERSION: u32 = 3;
const MAX_TRANSFER_KEY_LEN: usize = 128;
const MAX_COMPLETED_INTERVALS: usize = 16_384;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferPhase {
    #[default]
    Receiving,
    Finalizing,
    Completed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletedInterval {
    pub start: u64,
    pub end: u64,
    pub hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalFileSnapshot {
    pub size: u64,
    pub modified_nanos: Option<u128>,
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
    #[serde(default)]
    pub phase: TransferPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_snapshot: Option<FinalFileSnapshot>,
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
            phase: TransferPhase::Receiving,
            final_filename: None,
            final_snapshot: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn is_compatible_with(&self, expected: &Self) -> bool {
        (MIN_COMPATIBLE_SCHEMA_VERSION..=META_SCHEMA_VERSION).contains(&self.schema_version)
            && self.transfer_key == expected.transfer_key
            && self.sender_device_id == expected.sender_device_id
            && self.filename == expected.filename
            && self.total_bytes == expected.total_bytes
            && self.hash_algorithm == "blake3"
            && (expected.content_hash.is_empty() || self.content_hash == expected.content_hash)
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
        if interval.start >= interval.end || interval.end > self.total_bytes {
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
        self.touch();
        self.compact_if_needed();
        Ok(())
    }

    pub fn invalidate(&mut self, interval: Interval) -> io::Result<bool> {
        if interval.start >= interval.end || interval.end > self.total_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid interval to invalidate",
            ));
        }
        let mut changed = false;
        let mut retained = Vec::with_capacity(self.completed_intervals.len() + 1);
        for completed in self.completed_intervals.drain(..) {
            let current = completed.interval();
            if !current.overlaps(interval) {
                retained.push(completed);
                continue;
            }
            changed = true;
            if current.start < interval.start {
                retained.push(CompletedInterval {
                    start: current.start,
                    end: interval.start,
                    hash: String::new(),
                });
            }
            if current.end > interval.end {
                retained.push(CompletedInterval {
                    start: interval.end,
                    end: current.end,
                    hash: String::new(),
                });
            }
        }
        retained.sort_by_key(|entry| entry.start);
        self.completed_intervals = retained;
        if changed {
            self.phase = TransferPhase::Receiving;
            self.final_filename = None;
            self.final_snapshot = None;
            self.touch();
        }
        Ok(changed)
    }

    /// Clears all durable ranges after a final whole-file hash mismatch.
    ///
    /// Active/pending work is deliberately not represented in metadata. The
    /// caller can rebuild the complete missing-range plan from this empty set.
    pub fn reset_after_hash_mismatch(&mut self) -> bool {
        let changed = !self.completed_intervals.is_empty()
            || self.phase != TransferPhase::Receiving
            || self.final_filename.is_some()
            || self.final_snapshot.is_some();
        self.completed_intervals.clear();
        self.phase = TransferPhase::Receiving;
        self.final_filename = None;
        self.final_snapshot = None;
        if changed {
            self.touch();
        }
        changed
    }

    pub fn set_content_hash(&mut self, content_hash: String) {
        if self.content_hash != content_hash {
            self.content_hash = content_hash;
            self.touch();
        }
    }

    pub fn mark_finalizing(&mut self, final_filename: impl Into<String>) -> io::Result<()> {
        self.set_final_phase(TransferPhase::Finalizing, final_filename.into())?;
        self.final_snapshot = None;
        Ok(())
    }

    pub fn mark_completed(&mut self, final_filename: impl Into<String>) -> io::Result<()> {
        self.mark_completed_with_snapshot(final_filename, None)
    }

    pub fn mark_completed_with_snapshot(
        &mut self,
        final_filename: impl Into<String>,
        snapshot: Option<FinalFileSnapshot>,
    ) -> io::Result<()> {
        self.set_final_phase(TransferPhase::Completed, final_filename.into())?;
        self.final_snapshot = snapshot;
        Ok(())
    }

    pub fn mark_receiving(&mut self) {
        if self.phase != TransferPhase::Receiving
            || self.final_filename.is_some()
            || self.final_snapshot.is_some()
        {
            self.phase = TransferPhase::Receiving;
            self.final_filename = None;
            self.final_snapshot = None;
            self.touch();
        }
    }

    fn set_final_phase(&mut self, phase: TransferPhase, final_filename: String) -> io::Result<()> {
        debug_assert!(phase != TransferPhase::Receiving);
        validate_final_filename(&final_filename)?;
        if !self.completed_set().covers(Interval {
            start: 0,
            end: self.total_bytes,
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot finalize incomplete transfer metadata",
            ));
        }
        self.phase = phase;
        self.final_filename = Some(final_filename);
        self.touch();
        Ok(())
    }

    fn touch(&mut self) {
        self.schema_version = META_SCHEMA_VERSION;
        self.updated_at = timestamp_ms();
    }

    fn compact_if_needed(&mut self) {
        if self.completed_intervals.len() <= MAX_COMPLETED_INTERVALS {
            return;
        }
        // A non-empty segment hash is required for cold-resume verification,
        // so compaction must never replace verified entries with a merged
        // hashless interval. Only already-hashless adjacent fragments (created
        // by invalidation or an older schema) are safe to coalesce.
        let mut compacted: Vec<CompletedInterval> =
            Vec::with_capacity(self.completed_intervals.len());
        for current in self.completed_intervals.drain(..) {
            if let Some(previous) = compacted.last_mut()
                && previous.hash.is_empty()
                && current.hash.is_empty()
                && previous.end == current.start
            {
                previous.end = current.end;
            } else {
                compacted.push(current);
            }
        }
        self.completed_intervals = compacted;
    }
}

pub fn resume_dir(save_dir: impl AsRef<Path>) -> PathBuf {
    save_dir.as_ref().join(V3_RESUME_DIR)
}

/// Validates a transfer key before it is used as a filesystem component.
///
/// Identity-generated keys are lowercase BLAKE3 hex strings. The slightly
/// wider character set keeps compatibility with older/test peers while still
/// rejecting separators, dot components, Unicode lookalikes, and control
/// characters.
pub fn validate_transfer_key(transfer_key: &str) -> io::Result<()> {
    let valid = !transfer_key.is_empty()
        && transfer_key.len() <= MAX_TRANSFER_KEY_LEN
        && transfer_key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        && !is_windows_reserved_component(transfer_key);
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid transfer key",
        ))
    }
}

fn is_windows_reserved_component(value: &str) -> bool {
    matches!(value, "con" | "prn" | "aux" | "nul")
        || value
            .strip_prefix("com")
            .or_else(|| value.strip_prefix("lpt"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}

pub fn meta_path(save_dir: impl AsRef<Path>, transfer_key: &str) -> PathBuf {
    resume_dir(save_dir).join(format!("{}.json", path_safe_transfer_key(transfer_key)))
}

pub fn part_path(save_dir: impl AsRef<Path>, transfer_key: &str) -> PathBuf {
    resume_dir(save_dir).join(format!("{}.part", path_safe_transfer_key(transfer_key)))
}

pub fn load(save_dir: impl AsRef<Path>, transfer_key: &str) -> io::Result<Option<TransferMeta>> {
    validate_transfer_key(transfer_key)?;
    let path = meta_path(save_dir, transfer_key);
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut meta: TransferMeta = serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if meta.transfer_key != transfer_key {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "metadata transfer key does not match its path",
                ));
            }
            validate_phase_state(&meta)?;
            if (MIN_COMPATIBLE_SCHEMA_VERSION..=META_SCHEMA_VERSION).contains(&meta.schema_version)
            {
                meta.schema_version = META_SCHEMA_VERSION;
            }
            Ok(Some(meta))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Loads metadata, isolating malformed JSON (or invalid persisted state) so a
/// fresh transfer can be created at the canonical path.
///
/// The corrupt file is renamed next to the canonical metadata file using a
/// `.corrupt-*` suffix. I/O errors and invalid caller-supplied transfer keys
/// are still returned to the caller.
pub fn load_or_quarantine(
    save_dir: impl AsRef<Path>,
    transfer_key: &str,
) -> io::Result<Option<TransferMeta>> {
    let save_dir = save_dir.as_ref();
    match load(save_dir, transfer_key) {
        Ok(meta) => Ok(meta),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            quarantine_corrupt(save_dir, transfer_key)?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Moves the current metadata file aside and returns its quarantine path.
pub fn quarantine_corrupt(
    save_dir: impl AsRef<Path>,
    transfer_key: &str,
) -> io::Result<Option<PathBuf>> {
    validate_transfer_key(transfer_key)?;
    let save_dir = save_dir.as_ref();
    let source = meta_path(save_dir, transfer_key);
    if !source.exists() {
        return Ok(None);
    }
    let directory = resume_dir(save_dir);
    let component = path_safe_transfer_key(transfer_key);
    for attempt in 0..1_024u16 {
        let destination = directory.join(format!(
            "{component}.corrupt-{}-{}-{attempt}.json",
            timestamp_ms(),
            std::process::id()
        ));
        if destination.exists() {
            continue;
        }
        match std::fs::rename(&source, &destination) {
            Ok(()) => {
                sync_directory_entry_change(&directory)?;
                return Ok(Some(destination));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to allocate corrupt metadata quarantine path",
    ))
}

pub fn save_atomic(save_dir: impl AsRef<Path>, meta: &TransferMeta) -> io::Result<()> {
    validate_transfer_key(&meta.transfer_key)?;
    validate_phase_state(meta)?;
    let save_dir = save_dir.as_ref();
    let directory = resume_dir(save_dir);
    std::fs::create_dir_all(&directory)?;
    let data = serde_json::to_vec(meta).map_err(io::Error::other)?;
    AtomicFile::new(meta_path(save_dir, &meta.transfer_key), AllowOverwrite).write(|file| {
        file.write_all(&data)?;
        file.sync_data()
    })?;
    sync_directory_entry_change(&directory)
}

pub fn remove(save_dir: impl AsRef<Path>, transfer_key: &str) -> io::Result<()> {
    validate_transfer_key(transfer_key)?;
    let save_dir = save_dir.as_ref();
    let directory = resume_dir(save_dir);
    match std::fs::remove_file(meta_path(save_dir, transfer_key)) {
        Ok(()) => sync_directory_entry_change(&directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn path_safe_transfer_key(transfer_key: &str) -> String {
    if validate_transfer_key(transfer_key).is_ok() {
        transfer_key.to_owned()
    } else {
        format!("invalid-{}", blake3::hash(transfer_key.as_bytes()).to_hex())
    }
}

fn validate_final_filename(filename: &str) -> io::Result<()> {
    let valid = !filename.is_empty()
        && filename != "."
        && filename != ".."
        && !filename.contains(['/', '\\', '\0']);
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid final filename",
        ))
    }
}

fn validate_phase_state(meta: &TransferMeta) -> io::Result<()> {
    match (meta.phase, meta.final_filename.as_deref()) {
        (TransferPhase::Receiving, None) => Ok(()),
        (TransferPhase::Finalizing | TransferPhase::Completed, Some(filename)) => {
            validate_final_filename(filename)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata phase and final filename are inconsistent",
        )),
    }
}

#[cfg(unix)]
fn sync_directory_entry_change(directory: &Path) -> io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}

#[cfg(windows)]
fn sync_directory_entry_change(directory: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    // Windows does not consistently support flushing directory handles, but
    // FILE_FLAG_BACKUP_SEMANTICS lets us make the best available attempt.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let result = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)
        .and_then(|file| file.sync_all());
    // FlushFileBuffers may reject directory handles depending on the backing
    // filesystem. The data file itself has already been synced, so failure of
    // this additional durability barrier must not make a completed save/remove
    // unusable on those filesystems.
    let _ = result;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_directory_entry_change(_directory: &Path) -> io::Result<()> {
    Ok(())
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
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root(label: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "locsd-meta-{label}-{}-{}-{}",
            std::process::id(),
            timestamp_ms(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn metadata_round_trip_is_atomic_and_preserves_intervals() {
        let root = temp_root("round-trip");
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

    #[test]
    fn compaction_never_discards_non_empty_segment_hashes() {
        let mut meta = TransferMeta::new(
            "compact-key".into(),
            "sender".into(),
            "file".into(),
            (MAX_COMPLETED_INTERVALS + 1) as u64,
            "hash".into(),
        );
        meta.completed_intervals = (0..=MAX_COMPLETED_INTERVALS)
            .map(|index| CompletedInterval {
                start: index as u64,
                end: index as u64 + 1,
                hash: if index < 2 {
                    String::new()
                } else {
                    format!("hash-{index}")
                },
            })
            .collect();

        meta.compact_if_needed();

        assert_eq!(meta.completed_intervals.len(), MAX_COMPLETED_INTERVALS);
        assert_eq!(
            meta.completed_intervals[0].interval(),
            Interval { start: 0, end: 2 }
        );
        assert_eq!(
            meta.completed_intervals[1].hash, "hash-2",
            "verified segment hashes must survive compaction"
        );
        assert_eq!(
            meta.completed_intervals.last().unwrap().hash,
            format!("hash-{MAX_COMPLETED_INTERVALS}")
        );
    }

    #[test]
    fn schema_three_without_phase_fields_loads_as_receiving() {
        let root = temp_root("legacy");
        let directory = resume_dir(&root);
        std::fs::create_dir_all(&directory).unwrap();
        let meta = TransferMeta::new(
            "legacy-key".into(),
            "sender".into(),
            "file.txt".into(),
            100,
            "hash".into(),
        );
        let mut value = serde_json::to_value(meta).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("schemaVersion".into(), serde_json::json!(3));
        object.remove("phase");
        object.remove("finalFilename");
        std::fs::write(
            meta_path(&root, "legacy-key"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();

        let loaded = load(&root, "legacy-key").unwrap().unwrap();
        assert_eq!(loaded.schema_version, META_SCHEMA_VERSION);
        assert_eq!(loaded.phase, TransferPhase::Receiving);
        assert_eq!(loaded.final_filename, None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn finalizing_completed_and_hash_mismatch_reset_are_explicit() {
        let mut meta = TransferMeta::new(
            "phase-key".into(),
            "sender".into(),
            "file.txt".into(),
            100,
            "hash".into(),
        );
        assert!(meta.mark_finalizing("file.txt").is_err());
        meta.commit(Interval { start: 0, end: 100 }, "range-hash".into())
            .unwrap();
        meta.mark_finalizing("file (1).txt").unwrap();
        assert_eq!(meta.phase, TransferPhase::Finalizing);
        assert_eq!(meta.final_filename.as_deref(), Some("file (1).txt"));
        meta.mark_completed("file (1).txt").unwrap();
        assert_eq!(meta.phase, TransferPhase::Completed);

        assert!(meta.reset_after_hash_mismatch());
        assert_eq!(meta.phase, TransferPhase::Receiving);
        assert_eq!(meta.final_filename, None);
        assert!(meta.completed_intervals.is_empty());
        assert_eq!(meta.durable_bytes(), 0);
    }

    #[test]
    fn transfer_key_cannot_escape_resume_directory() {
        let root = temp_root("unsafe-key");
        let unsafe_key = "../../outside";
        assert_eq!(
            validate_transfer_key(unsafe_key).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(validate_transfer_key("CON").is_err());
        assert!(validate_transfer_key("con").is_err());
        assert!(validate_transfer_key("ABCDEF").is_err());
        let path = meta_path(&root, unsafe_key);
        assert_eq!(path.parent(), Some(resume_dir(&root).as_path()));
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("invalid-")
        );

        let meta = TransferMeta::new(
            unsafe_key.into(),
            "sender".into(),
            "file".into(),
            1,
            "hash".into(),
        );
        assert_eq!(
            save_atomic(&root, &meta).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn malformed_json_is_quarantined_and_canonical_path_is_reusable() {
        let root = temp_root("corrupt");
        let directory = resume_dir(&root);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(meta_path(&root, "corrupt-key"), b"{not json").unwrap();

        assert_eq!(load_or_quarantine(&root, "corrupt-key").unwrap(), None);
        assert!(!meta_path(&root, "corrupt-key").exists());
        let quarantined = std::fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("corrupt-key.corrupt-"))
            .collect::<Vec<_>>();
        assert_eq!(quarantined.len(), 1);

        let meta = TransferMeta::new(
            "corrupt-key".into(),
            "sender".into(),
            "file".into(),
            1,
            "hash".into(),
        );
        save_atomic(&root, &meta).unwrap();
        assert_eq!(load(&root, "corrupt-key").unwrap(), Some(meta));
        remove(&root, "corrupt-key").unwrap();
        remove(&root, "corrupt-key").unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalidating_a_bad_subrange_preserves_only_disjoint_durable_bytes() {
        let mut meta = TransferMeta::new(
            "key".into(),
            "sender".into(),
            "file".into(),
            100,
            "hash".into(),
        );
        meta.commit(Interval { start: 0, end: 100 }, "whole".into())
            .unwrap();
        assert!(meta.invalidate(Interval { start: 25, end: 75 }).unwrap());
        assert_eq!(
            meta.completed_set().as_slice(),
            &[
                Interval { start: 0, end: 25 },
                Interval {
                    start: 75,
                    end: 100
                }
            ]
        );
        assert_eq!(meta.durable_bytes(), 50);
    }

    #[test]
    fn reversed_intervals_are_rejected_before_length_arithmetic() {
        let mut meta = TransferMeta::new(
            "key".into(),
            "sender".into(),
            "file".into(),
            100,
            "hash".into(),
        );
        let reversed = Interval { start: 80, end: 20 };
        assert_eq!(
            meta.commit(reversed, "hash".into()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            meta.invalidate(reversed).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
