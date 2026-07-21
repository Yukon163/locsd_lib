use super::events::{TransferCallback, TransferDirection, TransferEvent, TransferState};
use super::identity::hash_file;
use super::metadata::{self, TransferMeta};
use super::protocol::{
    self, Ack3, AckStatus, ConnectionRole, Done3, Error3, Final3, FrameKind, Hello3, Offer3,
    Pause3, Plan3, ProtocolError, Range3,
};
use super::range::{Interval, IntervalSet};
use blake3::Hasher;
use log::{info, warn};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

pub const RECEIVER_MAX_CONCURRENCY: usize = 12;

pub struct ReceiverContext {
    save_dir: PathBuf,
    callback: Arc<dyn TransferCallback>,
    sessions: Mutex<HashMap<String, Arc<ReceiverSession>>>,
}

struct ReceiverSession {
    offer: Offer3,
    token: String,
    save_dir: PathBuf,
    part_path: PathBuf,
    meta: Mutex<TransferMeta>,
    active: Mutex<IntervalSet>,
    callback: Arc<dyn TransferCallback>,
    started: Instant,
}

impl ReceiverContext {
    pub fn new(save_dir: impl Into<PathBuf>, callback: Arc<dyn TransferCallback>) -> Self {
        Self {
            save_dir: save_dir.into(),
            callback,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn handle_connection(self: Arc<Self>, mut stream: TcpStream) {
        if let Err(error) = self.handle_connection_inner(&mut stream).await {
            warn!("V3 receiver connection failed: {error}");
            let response = Error3 {
                code: "protocol_error".into(),
                message: error.to_string(),
                unsupported: false,
            };
            let _ = protocol::write_control(&mut stream, FrameKind::ErrorResponse, &response).await;
        }
    }

    async fn handle_connection_inner(&self, stream: &mut TcpStream) -> Result<(), ReceiverError> {
        let hello: Hello3 = protocol::read_control(stream, FrameKind::Hello).await?;
        if hello.protocol_version != protocol::PROTOCOL_VERSION {
            let response = Error3 {
                code: "unsupported_version".into(),
                message: format!("protocol {} is unsupported", hello.protocol_version),
                unsupported: true,
            };
            protocol::write_control(stream, FrameKind::ErrorResponse, &response).await?;
            return Ok(());
        }
        let response = Hello3 {
            protocol_version: protocol::PROTOCOL_VERSION,
            role: hello.role,
            capabilities: vec![
                "range".into(),
                "ack".into(),
                "pause".into(),
                "blake3".into(),
                "resume".into(),
            ],
            receiver_max_concurrency: RECEIVER_MAX_CONCURRENCY as u16,
            session_token: hello.session_token.clone(),
        };
        protocol::write_control(stream, FrameKind::Hello, &response).await?;

        match hello.role {
            ConnectionRole::Control => self.handle_control(stream).await,
            ConnectionRole::Data => {
                let token = hello
                    .session_token
                    .ok_or(ReceiverError::MissingSessionToken)?;
                let session = self
                    .sessions
                    .lock()
                    .await
                    .get(&token)
                    .cloned()
                    .ok_or(ReceiverError::UnknownSession)?;
                session.handle_data(stream).await
            }
        }
    }

    async fn handle_control(&self, stream: &mut TcpStream) -> Result<(), ReceiverError> {
        let offer: Offer3 = protocol::read_control(stream, FrameKind::Offer).await?;
        let peer = stream
            .peer_addr()
            .map(|value| value.ip().to_string())
            .unwrap_or_default();
        if !self
            .callback
            .on_receive_request(offer.filename.clone(), offer.total_bytes, peer)
        {
            let plan = Plan3 {
                accepted: false,
                session_token: String::new(),
                completed_intervals: Vec::new(),
                receiver_max_concurrency: RECEIVER_MAX_CONCURRENCY as u16,
                max_frame_bytes: protocol::MAX_DATA_FRAME as u32,
                reason: Some("rejected by receiver".into()),
            };
            protocol::write_control(stream, FrameKind::Plan, &plan).await?;
            return Ok(());
        }

        let session = self.prepare_session(offer).await?;
        let completed = session
            .meta
            .lock()
            .await
            .completed_set()
            .as_slice()
            .to_vec();
        let plan = Plan3 {
            accepted: true,
            session_token: session.token.clone(),
            completed_intervals: completed,
            receiver_max_concurrency: RECEIVER_MAX_CONCURRENCY as u16,
            max_frame_bytes: protocol::MAX_DATA_FRAME as u32,
            reason: None,
        };
        self.sessions
            .lock()
            .await
            .insert(session.token.clone(), session.clone());
        protocol::write_control(stream, FrameKind::Plan, &plan).await?;
        session.emit(TransferState::Transferring, None, None).await;

        let final_frame: Final3 = protocol::read_control(stream, FrameKind::Final).await?;
        if final_frame.transfer_id != session.offer.transfer_id
            || final_frame.content_hash != session.offer.content_hash
        {
            return Err(ReceiverError::IdentityMismatch);
        }
        session.emit(TransferState::Verifying, None, None).await;
        let done = session.finalize().await?;
        protocol::write_control(stream, FrameKind::Done, &done).await?;
        self.sessions.lock().await.remove(&session.token);
        session
            .emit(
                TransferState::Completed,
                None,
                Some(done.final_filename.clone()),
            )
            .await;
        Ok(())
    }

    async fn prepare_session(&self, offer: Offer3) -> Result<Arc<ReceiverSession>, ReceiverError> {
        std::fs::create_dir_all(metadata::resume_dir(&self.save_dir))?;
        let expected = TransferMeta::new(
            offer.transfer_key.clone(),
            offer.sender_device_id.clone(),
            safe_filename(&offer.filename),
            offer.total_bytes,
            offer.content_hash.clone(),
        );
        let part_path = metadata::part_path(&self.save_dir, &offer.transfer_key);
        let meta = match metadata::load(&self.save_dir, &offer.transfer_key)? {
            Some(existing)
                if existing.is_compatible_with(&expected)
                    && part_path
                        .metadata()
                        .map(|value| value.len() == offer.total_bytes)
                        .unwrap_or(false) =>
            {
                existing
            }
            _ => {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&part_path)?;
                file.set_len(offer.total_bytes)?;
                file.sync_data()?;
                metadata::save_atomic(&self.save_dir, &expected)?;
                expected
            }
        };
        validate_completed_intervals(&meta)?;
        let token = session_token(&offer.transfer_id, &offer.transfer_key);
        Ok(Arc::new(ReceiverSession {
            offer,
            token,
            save_dir: self.save_dir.clone(),
            part_path,
            meta: Mutex::new(meta),
            active: Mutex::new(IntervalSet::new()),
            callback: self.callback.clone(),
            started: Instant::now(),
        }))
    }
}

impl ReceiverSession {
    async fn handle_data(&self, stream: &mut TcpStream) -> Result<(), ReceiverError> {
        loop {
            let range: Range3 = match protocol::read_control(stream, FrameKind::Range).await {
                Ok(value) => value,
                Err(ProtocolError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            let interval = range.interval()?;
            if interval.end > self.offer.total_bytes {
                self.write_rejected(
                    stream,
                    range.range_id,
                    interval.start,
                    "range exceeds file size",
                )
                .await?;
                continue;
            }
            let completed = self.meta.lock().await.completed_set();
            if completed.covers(interval) {
                let ack = Ack3 {
                    range_id: range.range_id,
                    committed_end: interval.end,
                    range_hash: String::new(),
                    status: AckStatus::Duplicate,
                    message: None,
                };
                protocol::write_control(stream, FrameKind::Ack, &ack).await?;
                continue;
            }
            if completed.overlaps(interval) || self.active.lock().await.overlaps(interval) {
                self.write_rejected(
                    stream,
                    range.range_id,
                    interval.start,
                    "range overlaps registered data",
                )
                .await?;
                continue;
            }
            self.active.lock().await.insert(interval);
            let result = self.receive_range(stream, &range, interval).await;
            self.active.lock().await.remove(interval);
            result?;
        }
    }

    async fn receive_range(
        &self,
        stream: &mut TcpStream,
        range: &Range3,
        interval: Interval,
    ) -> Result<(), ReceiverError> {
        let mut file = OpenOptions::new().write(true).open(&self.part_path).await?;
        file.seek(SeekFrom::Start(interval.start)).await?;
        let mut committed_end = interval.start;
        let mut hasher = Hasher::new();

        loop {
            let (kind, payload) = protocol::read_payload(stream).await?;
            match kind {
                FrameKind::Data => {
                    if payload.len() < 8 {
                        return Err(ProtocolError::InvalidDataFrame.into());
                    }
                    let offset = u64::from_be_bytes(payload[..8].try_into().unwrap());
                    if offset != committed_end {
                        return Err(ProtocolError::InvalidOffset {
                            offset,
                            expected: committed_end,
                        }
                        .into());
                    }
                    let bytes = &payload[8..];
                    if committed_end.saturating_add(bytes.len() as u64) > interval.end {
                        return Err(ReceiverError::RangeOverflow);
                    }
                    file.write_all(bytes).await?;
                    hasher.update(bytes);
                    committed_end += bytes.len() as u64;
                    if committed_end == interval.end {
                        self.commit_and_ack(
                            stream,
                            &mut file,
                            range.range_id,
                            interval.start,
                            committed_end,
                            hasher,
                        )
                        .await?;
                        return Ok(());
                    }
                }
                FrameKind::Pause => {
                    let pause: Pause3 = serde_json::from_slice(&payload)?;
                    if pause.range_id != range.range_id || pause.generation != range.generation {
                        return Err(ReceiverError::PauseMismatch);
                    }
                    self.commit_and_ack(
                        stream,
                        &mut file,
                        range.range_id,
                        interval.start,
                        committed_end,
                        hasher,
                    )
                    .await?;
                    return Ok(());
                }
                actual => {
                    return Err(ProtocolError::UnexpectedFrame {
                        expected: FrameKind::Data,
                        actual,
                    }
                    .into());
                }
            }
        }
    }

    async fn commit_and_ack(
        &self,
        stream: &mut TcpStream,
        file: &mut tokio::fs::File,
        range_id: u64,
        start: u64,
        committed_end: u64,
        hasher: Hasher,
    ) -> Result<(), ReceiverError> {
        file.flush().await?;
        file.sync_data().await?;
        let hash = hasher.finalize().to_hex().to_string();
        if committed_end > start {
            let mut meta = self.meta.lock().await;
            meta.commit(
                Interval {
                    start,
                    end: committed_end,
                },
                hash.clone(),
            )?;
            metadata::save_atomic(&self.save_dir, &meta)?;
        }
        let ack = Ack3 {
            range_id,
            committed_end,
            range_hash: hash,
            status: AckStatus::Committed,
            message: None,
        };
        protocol::write_control(stream, FrameKind::Ack, &ack).await?;
        self.emit(TransferState::Transferring, None, None).await;
        Ok(())
    }

    async fn write_rejected(
        &self,
        stream: &mut TcpStream,
        range_id: u64,
        committed_end: u64,
        reason: &str,
    ) -> Result<(), ReceiverError> {
        let ack = Ack3 {
            range_id,
            committed_end,
            range_hash: String::new(),
            status: AckStatus::Rejected,
            message: Some(reason.into()),
        };
        protocol::write_control(stream, FrameKind::Ack, &ack).await?;
        Ok(())
    }

    async fn finalize(&self) -> Result<Done3, ReceiverError> {
        let completed = self.meta.lock().await.completed_set();
        if !completed.covers(Interval {
            start: 0,
            end: self.offer.total_bytes,
        }) {
            return Err(ReceiverError::IncompleteTransfer);
        }
        let path = self.part_path.clone();
        let actual_hash = tokio::task::spawn_blocking(move || hash_file(path))
            .await
            .map_err(ReceiverError::Join)??;
        if actual_hash != self.offer.content_hash {
            return Err(ReceiverError::HashMismatch {
                expected: self.offer.content_hash.clone(),
                actual: actual_hash,
            });
        }
        let (final_path, final_filename) =
            available_final_path(&self.save_dir, &self.offer.filename);
        std::fs::rename(&self.part_path, &final_path)?;
        metadata::remove(&self.save_dir, &self.offer.transfer_key)?;
        info!("V3 received and verified {}", final_path.display());
        Ok(Done3 {
            transfer_id: self.offer.transfer_id.clone(),
            final_filename,
            total_bytes: self.offer.total_bytes,
            verified: true,
        })
    }

    async fn emit(
        &self,
        state: TransferState,
        error_code: Option<String>,
        message: Option<String>,
    ) {
        let meta = self.meta.lock().await;
        let active = self.active.lock().await;
        let mut event = TransferEvent::new(
            self.offer.transfer_id.clone(),
            TransferDirection::Receive,
            state,
            self.offer.filename.clone(),
            self.offer.total_bytes,
            self.started.elapsed(),
        );
        event.durable_bytes = meta.durable_bytes();
        event.active_workers = active.as_slice().len() as u16;
        event.target_workers = RECEIVER_MAX_CONCURRENCY as u16;
        event.error_code = error_code;
        event.message = message;
        drop(active);
        drop(meta);
        self.callback.on_event(event);
    }
}

fn validate_completed_intervals(meta: &TransferMeta) -> Result<(), ReceiverError> {
    let mut set = IntervalSet::new();
    for completed in &meta.completed_intervals {
        let interval = completed.interval();
        if interval.is_empty() || interval.end > meta.total_bytes || set.overlaps(interval) {
            return Err(ReceiverError::InvalidMetadata);
        }
        set.insert(interval);
    }
    Ok(())
}

fn safe_filename(filename: &str) -> String {
    Path::new(filename)
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "received_file".into())
}

fn available_final_path(save_dir: &Path, filename: &str) -> (PathBuf, String) {
    let filename = safe_filename(filename);
    let direct = save_dir.join(&filename);
    if !direct.exists() {
        return (direct, filename);
    }
    let dot = filename.rfind('.');
    let (base, extension) = match dot {
        Some(index) if index > 0 && index < filename.len() - 1 => {
            (&filename[..index], &filename[index..])
        }
        _ => (filename.as_str(), ""),
    };
    for index in 1..=9999 {
        let candidate = format!("{base}({index}){extension}");
        let path = save_dir.join(&candidate);
        if !path.exists() {
            return (path, candidate);
        }
    }
    let candidate = format!("{base}({}){extension}", timestamp_ms());
    (save_dir.join(&candidate), candidate)
}

fn session_token(transfer_id: &str, transfer_key: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(transfer_id.as_bytes());
    hasher.update(transfer_key.as_bytes());
    hasher.update(&timestamp_ms().to_be_bytes());
    hasher.finalize().to_hex().to_string()
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[derive(Debug)]
pub enum ReceiverError {
    Protocol(ProtocolError),
    Io(io::Error),
    Json(serde_json::Error),
    Join(tokio::task::JoinError),
    MissingSessionToken,
    UnknownSession,
    IdentityMismatch,
    RangeOverflow,
    PauseMismatch,
    IncompleteTransfer,
    InvalidMetadata,
    HashMismatch { expected: String, actual: String },
}

impl std::fmt::Display for ReceiverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for ReceiverError {}
impl From<ProtocolError> for ReceiverError {
    fn from(value: ProtocolError) -> Self {
        Self::Protocol(value)
    }
}
impl From<io::Error> for ReceiverError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<serde_json::Error> for ReceiverError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}
