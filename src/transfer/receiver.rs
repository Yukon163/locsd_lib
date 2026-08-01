use super::events::{
    OfferDisposition, OfferId, TransferCallback, TransferDirection, TransferEvent, TransferSegment,
    TransferState,
};
use super::identity::{SourceSnapshot, hash_file};
use super::metadata::{self, TransferMeta};
use super::protocol::{
    self, Ack3, AckStatus, CompletedSegment3, ConnectionRole, Done3, Error3, Final3, FrameKind,
    Hello3, Invalidate3, Offer3, Pause3, Plan3, ProtocolError, Range3, RangeStatus3,
    RangeStatusResult3,
};
use super::range::{Interval, IntervalSet};
use blake3::Hasher;
use log::{info, warn};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, Semaphore, SemaphorePermit, oneshot};
use tokio_util::sync::CancellationToken;

pub const RECEIVER_MAX_CONCURRENCY: usize = 20;
const OFFER_DECISION_TIMEOUT: Duration = Duration::from_secs(300);
const UI_PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

pub enum OfferDecision {
    Accept { destination: PathBuf },
    Reject,
}

#[derive(Default)]
pub struct OfferRegistry {
    pending: std::sync::Mutex<HashMap<OfferId, oneshot::Sender<OfferDecision>>>,
}

impl OfferRegistry {
    pub fn register(&self, offer_id: OfferId) -> oneshot::Receiver<OfferDecision> {
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(offer_id, sender);
        receiver
    }

    pub fn accept(&self, offer_id: &str, destination: PathBuf) -> bool {
        self.pending
            .lock()
            .unwrap()
            .remove(offer_id)
            .is_some_and(|sender| sender.send(OfferDecision::Accept { destination }).is_ok())
    }

    pub fn reject(&self, offer_id: &str) -> bool {
        self.pending
            .lock()
            .unwrap()
            .remove(offer_id)
            .is_some_and(|sender| sender.send(OfferDecision::Reject).is_ok())
    }

    fn remove(&self, offer_id: &str) {
        self.pending.lock().unwrap().remove(offer_id);
    }
}

pub struct ReceiverContext {
    save_dir: PathBuf,
    callback: Arc<dyn TransferCallback>,
    sessions: Mutex<HashMap<String, Arc<ReceiverSession>>>,
    active_keys: Mutex<HashMap<String, String>>,
    offers: Arc<OfferRegistry>,
}

struct ReceiverSession {
    offer: Offer3,
    token: String,
    save_dir: PathBuf,
    part_path: PathBuf,
    meta: Mutex<TransferMeta>,
    active: Arc<ActiveRanges>,
    data_permits: Semaphore,
    lifecycle: Arc<SessionLifecycle>,
    callback: Arc<dyn TransferCallback>,
    started: Instant,
    last_progress_emit: StdMutex<Instant>,
    preverified_final: Option<PreverifiedFinal>,
    verification_generation: AtomicU64,
}

#[derive(Clone, Debug)]
struct PreverifiedFinal {
    filename: String,
    snapshot: SourceSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionPhase {
    Accepting,
    Finalizing,
    Closed,
}

struct SessionLifecycle {
    state: StdMutex<SessionLifecycleState>,
    drained: Notify,
    cancellation: CancellationToken,
}

struct SessionLifecycleState {
    phase: SessionPhase,
    data_connections: usize,
}

#[derive(Default)]
struct ActiveRanges {
    ranges: StdMutex<HashMap<u64, ActiveRangeProgress>>,
}

#[derive(Clone, Copy)]
struct ActiveRangeProgress {
    interval: Interval,
    downloaded_end: u64,
}

struct RangeReservation {
    active: Arc<ActiveRanges>,
    range_id: u64,
    interval: Interval,
}

struct DataConnectionRegistration {
    lifecycle: Arc<SessionLifecycle>,
}

struct DataConnectionGuard<'a> {
    _registration: DataConnectionRegistration,
    _permit: SemaphorePermit<'a>,
}

enum ReserveRange {
    Reserved(RangeReservation),
    Duplicate(RangeReservation),
    Rejected(&'static str),
    Closing,
}

impl SessionLifecycle {
    fn new() -> Self {
        Self {
            state: StdMutex::new(SessionLifecycleState {
                phase: SessionPhase::Accepting,
                data_connections: 0,
            }),
            drained: Notify::new(),
            cancellation: CancellationToken::new(),
        }
    }

    fn phase(&self) -> SessionPhase {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .phase
    }

    fn register_data(self: &Arc<Self>) -> Result<DataConnectionRegistration, ReceiverError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.phase != SessionPhase::Accepting {
            return Err(ReceiverError::SessionClosing);
        }
        state.data_connections += 1;
        Ok(DataConnectionRegistration {
            lifecycle: self.clone(),
        })
    }

    async fn begin_finalizing(&self) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.phase == SessionPhase::Accepting {
                state.phase = SessionPhase::Finalizing;
            }
        }
        self.cancellation.cancel();
        self.wait_for_data_connections().await;
    }

    async fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .phase = SessionPhase::Closed;
        self.cancellation.cancel();
        self.wait_for_data_connections().await;
    }

    async fn wait_for_data_connections(&self) {
        loop {
            let notified = self.drained.notified();
            if self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .data_connections
                == 0
            {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for DataConnectionRegistration {
    fn drop(&mut self) {
        let drained = {
            let mut state = self
                .lifecycle
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            debug_assert!(state.data_connections > 0);
            state.data_connections = state.data_connections.saturating_sub(1);
            state.data_connections == 0
        };
        if drained {
            self.lifecycle.drained.notify_waiters();
        }
    }
}

impl ActiveRanges {
    fn try_reserve(
        self: &Arc<Self>,
        range_id: u64,
        interval: Interval,
    ) -> Result<RangeReservation, &'static str> {
        let mut ranges = self
            .ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ranges.contains_key(&range_id) {
            return Err("range id is already active");
        }
        if ranges
            .values()
            .any(|active| active.interval.overlaps(interval))
        {
            return Err("range overlaps active data");
        }
        ranges.insert(
            range_id,
            ActiveRangeProgress {
                interval,
                downloaded_end: interval.start,
            },
        );
        Ok(RangeReservation {
            active: self.clone(),
            range_id,
            interval,
        })
    }

    fn overlaps(&self, interval: Interval) -> bool {
        self.ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|active| active.interval.overlaps(interval))
    }

    fn record_progress(&self, range_id: u64, downloaded_end: u64) {
        let mut ranges = self
            .ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(active) = ranges.get_mut(&range_id) {
            active.downloaded_end =
                downloaded_end.clamp(active.interval.start, active.interval.end);
        }
    }

    fn snapshot(&self) -> Vec<TransferSegment> {
        self.ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|active| TransferSegment {
                start: active.interval.start,
                end: active.interval.end,
                downloaded_end: active.downloaded_end,
                running: true,
            })
            .collect()
    }

    fn in_flight_bytes(&self) -> u64 {
        self.ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|active| active.downloaded_end.saturating_sub(active.interval.start))
            .sum()
    }

    fn len(&self) -> usize {
        self.ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl Drop for RangeReservation {
    fn drop(&mut self) {
        let mut ranges = self
            .active
            .ranges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ranges
            .get(&self.range_id)
            .is_some_and(|registered| registered.interval == self.interval)
        {
            ranges.remove(&self.range_id);
        }
    }
}

impl ReceiverContext {
    pub fn new(
        save_dir: impl Into<PathBuf>,
        callback: Arc<dyn TransferCallback>,
        offers: Arc<OfferRegistry>,
    ) -> Self {
        Self {
            save_dir: save_dir.into(),
            callback,
            sessions: Mutex::new(HashMap::new()),
            active_keys: Mutex::new(HashMap::new()),
            offers,
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
        if !supports_role_capabilities(&hello) {
            let response = Error3 {
                code: "unsupported_capabilities".into(),
                message: "peer does not advertise the required V3 capabilities".into(),
                unsupported: true,
            };
            protocol::write_control(stream, FrameKind::ErrorResponse, &response).await?;
            return Ok(());
        }
        let response = Hello3 {
            protocol_version: protocol::PROTOCOL_VERSION,
            role: hello.role,
            capabilities: vec![
                protocol::CAP_RANGE.into(),
                protocol::CAP_ACK.into(),
                protocol::CAP_PAUSE.into(),
                protocol::CAP_BLAKE3.into(),
                protocol::CAP_RESUME.into(),
                protocol::CAP_INVALIDATE.into(),
                protocol::CAP_RANGE_STATUS.into(),
                protocol::CAP_DEFERRED_HASH.into(),
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
        validate_offer(&offer)?;
        let peer = stream
            .peer_addr()
            .map(|value| value.ip().to_string())
            .unwrap_or_default();
        let decision_receiver = self.offers.register(offer.transfer_id.clone());
        let destination = match self.callback.on_offer(
            offer.transfer_id.clone(),
            offer.filename.clone(),
            offer.total_bytes,
            peer,
        ) {
            OfferDisposition::Accept => {
                self.offers.remove(&offer.transfer_id);
                Some(self.save_dir.clone())
            }
            OfferDisposition::Reject => {
                self.offers.remove(&offer.transfer_id);
                None
            }
            OfferDisposition::Pending => {
                match tokio::time::timeout(OFFER_DECISION_TIMEOUT, decision_receiver).await {
                    Ok(Ok(OfferDecision::Accept { destination })) => Some(destination),
                    Ok(Ok(OfferDecision::Reject)) | Ok(Err(_)) | Err(_) => {
                        self.offers.remove(&offer.transfer_id);
                        None
                    }
                }
            }
        };
        let Some(destination) = destination else {
            let plan = Plan3 {
                accepted: false,
                session_token: String::new(),
                completed_intervals: Vec::new(),
                completed_segments: Vec::new(),
                receiver_max_concurrency: RECEIVER_MAX_CONCURRENCY as u16,
                max_frame_bytes: protocol::MAX_DATA_FRAME as u32,
                reason: Some("rejected by receiver".into()),
            };
            protocol::write_control(stream, FrameKind::Plan, &plan).await?;
            return Ok(());
        };

        let token = session_token(&offer.transfer_id, &offer.transfer_key);
        if !self.reserve_transfer_key(&offer.transfer_key, &token).await {
            let plan = Plan3 {
                accepted: false,
                session_token: String::new(),
                completed_intervals: Vec::new(),
                completed_segments: Vec::new(),
                receiver_max_concurrency: RECEIVER_MAX_CONCURRENCY as u16,
                max_frame_bytes: protocol::MAX_DATA_FRAME as u32,
                reason: Some("transfer is already active".into()),
            };
            protocol::write_control(stream, FrameKind::Plan, &plan).await?;
            return Ok(());
        }

        let transfer_key = offer.transfer_key.clone();
        let session = match self
            .prepare_session(offer, destination, token.clone())
            .await
        {
            Ok(session) => session,
            Err(error) => {
                self.release_transfer_key(&transfer_key, &token).await;
                return Err(error);
            }
        };
        let (completed, completed_segments) = {
            let meta = session.meta.lock().await;
            (
                meta.completed_set().as_slice().to_vec(),
                meta.completed_intervals
                    .iter()
                    .map(|segment| CompletedSegment3 {
                        start: segment.start,
                        end: segment.end,
                        range_hash: segment.hash.clone(),
                    })
                    .collect(),
            )
        };
        let plan = Plan3 {
            accepted: true,
            session_token: session.token.clone(),
            completed_intervals: completed,
            completed_segments,
            receiver_max_concurrency: RECEIVER_MAX_CONCURRENCY as u16,
            max_frame_bytes: protocol::MAX_DATA_FRAME as u32,
            reason: None,
        };
        self.sessions
            .lock()
            .await
            .insert(session.token.clone(), session.clone());
        let result = async {
            protocol::write_control(stream, FrameKind::Plan, &plan).await?;
            session.emit(TransferState::Transferring, None, None).await;
            let verification_session = session.clone();
            let verification_task =
                tokio::spawn(async move { verification_session.hash_while_receiving().await });
            let final_frame: Final3 = match protocol::read_control(stream, FrameKind::Final).await {
                Ok(frame) => frame,
                Err(error) => {
                    verification_task.abort();
                    return Err(error.into());
                }
            };
            if final_frame.transfer_id != session.offer.transfer_id
                || !is_blake3_hex(&final_frame.content_hash)
                || (!session.offer.content_hash.is_empty()
                    && final_frame.content_hash != session.offer.content_hash)
            {
                verification_task.abort();
                return Err(ReceiverError::IdentityMismatch);
            }
            session.begin_finalizing().await;
            session.emit(TransferState::Verifying, None, None).await;
            let actual_hash = verification_task.await.map_err(ReceiverError::Join)??;
            session
                .set_content_hash(final_frame.content_hash.clone())
                .await?;
            let done = session
                .finalize(&final_frame.content_hash, actual_hash)
                .await?;
            protocol::write_control(stream, FrameKind::Done, &done).await?;
            session
                .emit(
                    TransferState::Completed,
                    None,
                    Some(done.final_filename.clone()),
                )
                .await;
            Ok(())
        }
        .await;
        session.close().await;
        self.sessions.lock().await.remove(&session.token);
        self.release_transfer_key(&transfer_key, &token).await;
        result
    }

    async fn prepare_session(
        &self,
        offer: Offer3,
        save_dir: PathBuf,
        token: String,
    ) -> Result<Arc<ReceiverSession>, ReceiverError> {
        std::fs::create_dir_all(metadata::resume_dir(&save_dir))?;
        let expected = TransferMeta::new(
            offer.transfer_key.clone(),
            offer.sender_device_id.clone(),
            safe_filename(&offer.filename),
            offer.total_bytes,
            offer.content_hash.clone(),
        );
        let part_path = metadata::part_path(&save_dir, &offer.transfer_key);
        let loaded = metadata::load_or_quarantine(&save_dir, &offer.transfer_key)?;
        let mut reusable = None;
        let mut preverified_final = None;
        if let Some(mut existing) = loaded {
            if !existing.is_compatible_with(&expected)
                || validate_completed_intervals(&existing).is_err()
            {
                metadata::quarantine_corrupt(&save_dir, &offer.transfer_key)?;
            } else {
                let final_filename = existing.final_filename.clone();
                let published_snapshot = match final_filename.as_deref() {
                    Some(filename)
                        if existing.phase != metadata::TransferPhase::Receiving
                            && !offer.content_hash.is_empty() =>
                    {
                        let final_path = save_dir.join(filename);
                        let persisted = match existing.final_snapshot.as_ref() {
                            Some(receipt) => {
                                verify_persisted_final_snapshot(final_path.clone(), receipt)?
                            }
                            None => None,
                        };
                        if persisted.is_some() {
                            persisted
                        } else {
                            verify_file_identity_snapshot(
                                final_path,
                                offer.total_bytes,
                                &offer.content_hash,
                            )
                            .await?
                        }
                    }
                    _ => None,
                };
                if let Some(snapshot) = published_snapshot {
                    let final_filename = final_filename.expect("published receipt has a filename");
                    let receipt = final_snapshot_receipt(&snapshot);
                    preverified_final = Some(PreverifiedFinal {
                        filename: final_filename.clone(),
                        snapshot: snapshot.clone(),
                    });
                    if existing.phase != metadata::TransferPhase::Completed
                        || existing.final_snapshot.as_ref() != Some(&receipt)
                    {
                        existing.mark_completed_with_snapshot(final_filename, Some(receipt))?;
                        metadata::save_atomic(&save_dir, &existing)?;
                    }
                    match std::fs::remove_file(&part_path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    reusable = Some(existing);
                } else if path_has_len(&part_path, offer.total_bytes) {
                    let changed = verify_completed_segments(&part_path, &mut existing).await?;
                    if existing.phase == metadata::TransferPhase::Completed {
                        if let Some(filename) = final_filename {
                            existing.mark_finalizing(filename)?;
                        } else {
                            existing.mark_receiving();
                        }
                    }
                    if changed || existing.phase != metadata::TransferPhase::Receiving {
                        metadata::save_atomic(&save_dir, &existing)?;
                    }
                    reusable = Some(existing);
                }
            }
        }
        let meta = match reusable {
            Some(meta) => meta,
            None => {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&part_path)?;
                file.set_len(offer.total_bytes)?;
                file.sync_data()?;
                metadata::save_atomic(&save_dir, &expected)?;
                expected
            }
        };
        Ok(Arc::new(ReceiverSession {
            offer,
            token,
            save_dir,
            part_path,
            meta: Mutex::new(meta),
            active: Arc::new(ActiveRanges::default()),
            data_permits: Semaphore::new(RECEIVER_MAX_CONCURRENCY),
            lifecycle: Arc::new(SessionLifecycle::new()),
            callback: self.callback.clone(),
            started: Instant::now(),
            last_progress_emit: StdMutex::new(Instant::now()),
            preverified_final,
            verification_generation: AtomicU64::new(0),
        }))
    }

    async fn reserve_transfer_key(&self, transfer_key: &str, token: &str) -> bool {
        for _ in 0..40 {
            let mut active = self.active_keys.lock().await;
            if !active.contains_key(transfer_key) {
                active.insert(transfer_key.to_string(), token.to_string());
                return true;
            }
            drop(active);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    async fn release_transfer_key(&self, transfer_key: &str, token: &str) {
        let mut active = self.active_keys.lock().await;
        if active.get(transfer_key).is_some_and(|owner| owner == token) {
            active.remove(transfer_key);
        }
    }
}

impl ReceiverSession {
    async fn begin_finalizing(&self) {
        self.lifecycle.begin_finalizing().await;
    }

    async fn close(&self) {
        self.lifecycle.close().await;
    }

    async fn handle_data(&self, stream: &mut TcpStream) -> Result<(), ReceiverError> {
        let _connection = match self.enter_data_connection().await {
            Ok(connection) => connection,
            Err(ReceiverError::SessionClosing) => return Ok(()),
            Err(error) => return Err(error),
        };
        loop {
            let (kind, payload) = match self.read_session_payload(stream).await {
                Ok(value) => value,
                Err(ReceiverError::Protocol(ProtocolError::Io(error)))
                    if error.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(());
                }
                Err(ReceiverError::SessionClosing) => return Ok(()),
                Err(error) => return Err(error),
            };
            match kind {
                FrameKind::Invalidate => {
                    let invalidate: Invalidate3 = serde_json::from_slice(&payload)?;
                    self.invalidate_range(stream, &invalidate).await?;
                }
                FrameKind::RangeStatus => {
                    let query: RangeStatus3 = serde_json::from_slice(&payload)?;
                    self.report_range_status(stream, &query).await?;
                }
                FrameKind::Range => {
                    let range: Range3 = serde_json::from_slice(&payload)?;
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
                    match self.reserve_range(range.range_id, interval).await {
                        ReserveRange::Reserved(reservation) => {
                            let result = self.receive_range(stream, &range, interval).await;
                            drop(reservation);
                            match result {
                                Err(ReceiverError::SessionClosing) => return Ok(()),
                                result => result?,
                            }
                            self.emit(TransferState::Transferring, None, None).await;
                        }
                        ReserveRange::Duplicate(_reservation) => {
                            match self.receive_duplicate_range(stream, &range, interval).await {
                                Err(ReceiverError::SessionClosing) => return Ok(()),
                                result => result?,
                            }
                        }
                        ReserveRange::Rejected(reason) => {
                            self.write_rejected(stream, range.range_id, interval.start, reason)
                                .await?;
                        }
                        ReserveRange::Closing => {
                            let _ = self
                                .write_rejected(
                                    stream,
                                    range.range_id,
                                    interval.start,
                                    "session is finalizing",
                                )
                                .await;
                            return Ok(());
                        }
                    }
                }
                actual => {
                    return Err(ProtocolError::UnexpectedFrame {
                        expected: FrameKind::Range,
                        actual,
                    }
                    .into());
                }
            }
        }
    }

    async fn enter_data_connection(&self) -> Result<DataConnectionGuard<'_>, ReceiverError> {
        let registration = self.lifecycle.register_data()?;
        let permit = tokio::select! {
            biased;
            _ = self.lifecycle.cancellation.cancelled() => {
                return Err(ReceiverError::SessionClosing);
            }
            permit = self.data_permits.acquire() => {
                permit.map_err(|_| ReceiverError::SessionClosing)?
            }
        };
        if self.lifecycle.phase() != SessionPhase::Accepting {
            return Err(ReceiverError::SessionClosing);
        }
        Ok(DataConnectionGuard {
            _registration: registration,
            _permit: permit,
        })
    }

    async fn read_session_payload(
        &self,
        stream: &mut TcpStream,
    ) -> Result<(FrameKind, Vec<u8>), ReceiverError> {
        tokio::select! {
            biased;
            _ = self.lifecycle.cancellation.cancelled() => {
                Err(ReceiverError::SessionClosing)
            }
            result = protocol::read_payload(stream) => result.map_err(ReceiverError::Protocol),
        }
    }

    async fn reserve_range(&self, range_id: u64, interval: Interval) -> ReserveRange {
        if self.lifecycle.phase() != SessionPhase::Accepting {
            return ReserveRange::Closing;
        }
        let meta = self.meta.lock().await;
        let completed = meta.completed_set();
        if completed.covers(interval) {
            return match self.active.try_reserve(range_id, interval) {
                Ok(reservation) => ReserveRange::Duplicate(reservation),
                Err(reason) => ReserveRange::Rejected(reason),
            };
        }
        if completed.overlaps(interval) {
            return ReserveRange::Rejected("range overlaps durable data");
        }
        if self.lifecycle.phase() != SessionPhase::Accepting {
            return ReserveRange::Closing;
        }
        match self.active.try_reserve(range_id, interval) {
            Ok(reservation) => ReserveRange::Reserved(reservation),
            Err(reason) => ReserveRange::Rejected(reason),
        }
    }

    async fn receive_duplicate_range(
        &self,
        stream: &mut TcpStream,
        range: &Range3,
        interval: Interval,
    ) -> Result<(), ReceiverError> {
        let mut committed_end = interval.start;
        loop {
            let (kind, payload) = self.read_session_payload(stream).await?;
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
                    committed_end = committed_end.saturating_add((payload.len() - 8) as u64);
                    if committed_end > interval.end {
                        return Err(ReceiverError::RangeOverflow);
                    }
                    if committed_end == interval.end {
                        break;
                    }
                }
                FrameKind::Pause => {
                    let pause: Pause3 = serde_json::from_slice(&payload)?;
                    if pause.range_id != range.range_id || pause.generation != range.generation {
                        return Err(ReceiverError::PauseMismatch);
                    }
                    break;
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
        let data_path = self.durable_storage_path().await;
        let hash = hash_part_range(&data_path, interval.start, committed_end).await?;
        let ack = Ack3 {
            range_id: range.range_id,
            committed_end,
            range_hash: hash,
            status: AckStatus::Duplicate,
            message: None,
        };
        protocol::write_control(stream, FrameKind::Ack, &ack).await?;
        Ok(())
    }

    async fn report_range_status(
        &self,
        stream: &mut TcpStream,
        query: &RangeStatus3,
    ) -> Result<(), ReceiverError> {
        let interval = query.interval()?;
        if interval.end > self.offer.total_bytes {
            return Err(ReceiverError::RangeOverflow);
        }
        if self.lifecycle.phase() != SessionPhase::Accepting {
            return Err(ReceiverError::SessionClosing);
        }
        // A prior connection may still be fsyncing and committing this range
        // after the sender's ACK timeout. Reporting "not committed" during
        // that window would make the retry collide with the live reservation
        // and rapidly burn through all attempts.
        if self.active.overlaps(interval) {
            let result = RangeStatusResult3 {
                range_id: query.range_id,
                committed_end: interval.start,
                range_hash: String::new(),
                busy: true,
            };
            protocol::write_control(stream, FrameKind::RangeStatusResult, &result).await?;
            return Ok(());
        }
        let (committed_end, range_hash) = loop {
            let before = {
                let meta = self.meta.lock().await;
                durable_status_snapshot(&meta, interval)
            };
            let range_hash = if let Some(hash) = before.1.clone() {
                hash
            } else if before.0 > interval.start {
                let data_path = self.durable_storage_path().await;
                hash_part_range(&data_path, interval.start, before.0).await?
            } else {
                String::new()
            };
            let after = {
                let meta = self.meta.lock().await;
                durable_status_snapshot(&meta, interval)
            };
            if after == before {
                break (before.0, range_hash);
            }
        };
        let result = RangeStatusResult3 {
            range_id: query.range_id,
            committed_end,
            range_hash,
            busy: false,
        };
        protocol::write_control(stream, FrameKind::RangeStatusResult, &result).await?;
        Ok(())
    }

    async fn durable_storage_path(&self) -> PathBuf {
        if self.part_path.exists() {
            return self.part_path.clone();
        }
        let meta = self.meta.lock().await;
        meta.final_filename
            .as_ref()
            .filter(|_| meta.phase != metadata::TransferPhase::Receiving)
            .map(|filename| self.save_dir.join(filename))
            .unwrap_or_else(|| self.part_path.clone())
    }

    async fn invalidate_range(
        &self,
        stream: &mut TcpStream,
        invalidate: &Invalidate3,
    ) -> Result<(), ReceiverError> {
        let interval = invalidate.interval()?;
        if interval.end > self.offer.total_bytes {
            self.write_rejected(
                stream,
                invalidate.range_id,
                interval.start,
                "cannot invalidate out-of-bounds range",
            )
            .await?;
            return Ok(());
        }
        let mut meta = self.meta.lock().await;
        if self.lifecycle.phase() != SessionPhase::Accepting || self.active.overlaps(interval) {
            drop(meta);
            self.write_rejected(
                stream,
                invalidate.range_id,
                interval.start,
                "cannot invalidate an active or finalizing range",
            )
            .await?;
            return Ok(());
        }
        meta.invalidate(interval)?;
        metadata::save_atomic(&self.save_dir, &meta)?;
        drop(meta);
        self.verification_generation.fetch_add(1, Ordering::AcqRel);
        let ack = Ack3 {
            range_id: invalidate.range_id,
            committed_end: interval.start,
            range_hash: String::new(),
            status: AckStatus::Committed,
            message: Some("range invalidated".into()),
        };
        protocol::write_control(stream, FrameKind::Ack, &ack).await?;
        Ok(())
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
            let (kind, payload) = self.read_session_payload(stream).await?;
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
                    self.active.record_progress(range.range_id, committed_end);
                    if self.should_emit_progress() {
                        self.emit(TransferState::Transferring, None, None).await;
                    }
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

    async fn set_content_hash(&self, content_hash: String) -> Result<(), ReceiverError> {
        let mut meta = self.meta.lock().await;
        meta.set_content_hash(content_hash);
        metadata::save_atomic(&self.save_dir, &meta)?;
        Ok(())
    }

    async fn hash_while_receiving(&self) -> Result<String, ReceiverError> {
        let path = self.durable_storage_path().await;
        let mut file = OpenOptions::new().read(true).open(path).await?;
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut hasher = Hasher::new();
        let mut offset = 0u64;
        let mut generation = self.verification_generation.load(Ordering::Acquire);
        loop {
            let current_generation = self.verification_generation.load(Ordering::Acquire);
            if current_generation != generation {
                generation = current_generation;
                offset = 0;
                hasher = Hasher::new();
                file.seek(SeekFrom::Start(0)).await?;
            }
            let durable_prefix = {
                let meta = self.meta.lock().await;
                meta.completed_set()
                    .as_slice()
                    .first()
                    .filter(|interval| interval.start == 0)
                    .map_or(0, |interval| interval.end)
            };
            if offset < durable_prefix {
                let count = (durable_prefix - offset).min(buffer.len() as u64) as usize;
                let read = file.read(&mut buffer[..count]).await?;
                if read == 0 {
                    return Err(ReceiverError::RangeOverflow);
                }
                if self.verification_generation.load(Ordering::Acquire) != generation {
                    continue;
                }
                hasher.update(&buffer[..read]);
                offset += read as u64;
                continue;
            }
            if offset == self.offer.total_bytes
                && self.lifecycle.phase() != SessionPhase::Accepting
                && self.verification_generation.load(Ordering::Acquire) == generation
            {
                return Ok(hasher.finalize().to_hex().to_string());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn finalize(
        &self,
        expected_hash: &str,
        actual_hash: String,
    ) -> Result<Done3, ReceiverError> {
        self.begin_finalizing().await;
        if self.active.len() != 0 {
            return Err(ReceiverError::ActiveRangesDuringFinalize);
        }
        let completed = self.meta.lock().await.completed_set();
        if !completed.covers(Interval {
            start: 0,
            end: self.offer.total_bytes,
        }) {
            return Err(ReceiverError::IncompleteTransfer);
        }
        if let Some(done) = self.recover_published_receipt(expected_hash).await? {
            return Ok(done);
        }
        if actual_hash != expected_hash {
            let mut meta = self.meta.lock().await;
            if meta.reset_after_hash_mismatch() {
                metadata::save_atomic(&self.save_dir, &meta)?;
            }
            return Err(ReceiverError::HashMismatch {
                expected: expected_hash.to_string(),
                actual: actual_hash,
            });
        }
        let (final_path, final_filename) = self.publish_verified_part(expected_hash).await?;
        info!("V3 received and verified {}", final_path.display());
        Ok(self.done(final_filename))
    }

    async fn recover_published_receipt(
        &self,
        content_hash: &str,
    ) -> Result<Option<Done3>, ReceiverError> {
        let receipt = {
            let meta = self.meta.lock().await;
            if meta.phase == metadata::TransferPhase::Receiving {
                None
            } else {
                meta.final_filename.clone()
            }
        };
        let Some(final_filename) = receipt else {
            return Ok(None);
        };
        let reused_verification = self.preverified_final.as_ref().is_some_and(|verified| {
            verified.filename == final_filename
                && verified.snapshot.verify_unchanged().unwrap_or(false)
        });
        if !reused_verification
            && !verify_file_identity(
                self.save_dir.join(&final_filename),
                self.offer.total_bytes,
                content_hash,
            )
            .await?
        {
            return Ok(None);
        }
        if reused_verification {
            info!(
                target: "locsd::transfer",
                "transfer_id={} event=verification_reused file={}",
                self.offer.transfer_id,
                final_filename,
            );
        }
        {
            let mut meta = self.meta.lock().await;
            if meta.phase != metadata::TransferPhase::Completed {
                let snapshot = SourceSnapshot::capture(self.save_dir.join(&final_filename)).ok();
                meta.mark_completed_with_snapshot(
                    final_filename.clone(),
                    snapshot.as_ref().map(final_snapshot_receipt),
                )?;
                metadata::save_atomic(&self.save_dir, &meta)?;
            }
        }
        match std::fs::remove_file(&self.part_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warn!("could not remove completed staging file: {error}"),
        }
        Ok(Some(self.done(final_filename)))
    }

    async fn publish_verified_part(
        &self,
        content_hash: &str,
    ) -> Result<(PathBuf, String), ReceiverError> {
        let preferred = {
            let meta = self.meta.lock().await;
            meta.final_filename.clone()
        };
        if let Some(candidate) = preferred.as_deref()
            && let Some(path) = self
                .try_publish_candidate(candidate, true, content_hash)
                .await?
        {
            return Ok((path, candidate.to_string()));
        }

        let filename = safe_filename(&self.offer.filename);
        let (base, extension) = split_filename(&filename);
        for index in 0..=9999u16 {
            let candidate = if index == 0 {
                filename.clone()
            } else {
                format!("{base}({index}){extension}")
            };
            if preferred.as_deref() == Some(candidate.as_str()) {
                continue;
            }
            if let Some(path) = self
                .try_publish_candidate(&candidate, false, content_hash)
                .await?
            {
                return Ok((path, candidate));
            }
        }
        let timestamp = timestamp_ms();
        for attempt in 0..1024u16 {
            let candidate = format!("{base}({timestamp}-{attempt}){extension}");
            if preferred.as_deref() == Some(candidate.as_str()) {
                continue;
            }
            if let Some(path) = self
                .try_publish_candidate(&candidate, false, content_hash)
                .await?
            {
                return Ok((path, candidate));
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a collision-free destination filename",
        )
        .into())
    }

    async fn try_publish_candidate(
        &self,
        candidate: &str,
        recover_existing: bool,
        content_hash: &str,
    ) -> Result<Option<PathBuf>, ReceiverError> {
        {
            let mut meta = self.meta.lock().await;
            meta.mark_finalizing(candidate.to_string())?;
            metadata::save_atomic(&self.save_dir, &meta)?;
        }
        let final_path =
            match publish_candidate(&self.part_path, &self.save_dir, candidate, content_hash) {
                Ok(path) => path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if !recover_existing
                        || !verify_file_identity(
                            self.save_dir.join(candidate),
                            self.offer.total_bytes,
                            content_hash,
                        )
                        .await?
                    {
                        return Ok(None);
                    }
                    let path = self.save_dir.join(candidate);
                    match std::fs::remove_file(&self.part_path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => warn!("could not remove recovered staging file: {error}"),
                    }
                    path
                }
                Err(error) => return Err(error.into()),
            };
        {
            let mut meta = self.meta.lock().await;
            let snapshot = SourceSnapshot::capture(&final_path)?;
            meta.mark_completed_with_snapshot(
                candidate.to_string(),
                Some(final_snapshot_receipt(&snapshot)),
            )?;
            metadata::save_atomic(&self.save_dir, &meta)?;
        }
        Ok(Some(final_path))
    }

    fn done(&self, final_filename: String) -> Done3 {
        Done3 {
            transfer_id: self.offer.transfer_id.clone(),
            final_filename,
            total_bytes: self.offer.total_bytes,
            verified: true,
        }
    }

    async fn emit(
        &self,
        state: TransferState,
        error_code: Option<String>,
        message: Option<String>,
    ) {
        let meta = self.meta.lock().await;
        let mut event = TransferEvent::new(
            self.offer.transfer_id.clone(),
            TransferDirection::Receive,
            state,
            self.offer.filename.clone(),
            self.offer.total_bytes,
            self.started.elapsed(),
        );
        let completed = meta.completed_set();
        event.durable_bytes = completed.total_len();
        event.segments = completed
            .as_slice()
            .iter()
            .map(|interval| TransferSegment {
                start: interval.start,
                end: interval.end,
                downloaded_end: interval.end,
                running: false,
            })
            .collect();
        let active_segments = self.active.snapshot();
        event.in_flight_bytes = self.active.in_flight_bytes();
        event
            .segments
            .extend(active_segments.into_iter().filter(|segment| {
                !completed.covers(Interval {
                    start: segment.start,
                    end: segment.end,
                })
            }));
        event
            .segments
            .sort_unstable_by_key(|segment| (segment.start, segment.end, segment.running));
        event.active_workers = self.active.len() as u16;
        event.target_workers = RECEIVER_MAX_CONCURRENCY as u16;
        event.error_code = error_code;
        event.message = message;
        drop(meta);
        self.callback.on_event(event);
    }

    fn should_emit_progress(&self) -> bool {
        let now = Instant::now();
        let mut last = self
            .last_progress_emit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if now.duration_since(*last) < UI_PROGRESS_INTERVAL {
            return false;
        }
        *last = now;
        true
    }
}

async fn hash_part_range(path: &Path, start: u64, end: u64) -> Result<String, ReceiverError> {
    let mut file = OpenOptions::new().read(true).open(path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let mut remaining = end.saturating_sub(start);
    let mut buffer = vec![0u8; 256 * 1024];
    let mut hasher = Hasher::new();
    while remaining > 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..count]).await?;
        if read == 0 {
            return Err(ReceiverError::RangeOverflow);
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn verify_file_identity(
    path: PathBuf,
    total_bytes: u64,
    content_hash: &str,
) -> Result<bool, ReceiverError> {
    Ok(
        verify_file_identity_snapshot(path, total_bytes, content_hash)
            .await?
            .is_some(),
    )
}

async fn verify_file_identity_snapshot(
    path: PathBuf,
    total_bytes: u64,
    content_hash: &str,
) -> Result<Option<SourceSnapshot>, ReceiverError> {
    let snapshot = match SourceSnapshot::capture(&path) {
        Ok(snapshot) if snapshot.size == total_bytes => snapshot,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let hash_path = path.clone();
    let actual_hash = tokio::task::spawn_blocking(move || hash_file(hash_path))
        .await
        .map_err(ReceiverError::Join)??;
    if actual_hash != content_hash || !snapshot.verify_unchanged()? {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

fn final_snapshot_receipt(snapshot: &SourceSnapshot) -> metadata::FinalFileSnapshot {
    metadata::FinalFileSnapshot {
        size: snapshot.size,
        modified_nanos: snapshot.modified_nanos,
    }
}

fn verify_persisted_final_snapshot(
    path: PathBuf,
    receipt: &metadata::FinalFileSnapshot,
) -> Result<Option<SourceSnapshot>, ReceiverError> {
    if receipt.modified_nanos.is_none() {
        return Ok(None);
    }
    let current = match SourceSnapshot::capture(path) {
        Ok(snapshot) => snapshot,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if current.size == receipt.size && current.modified_nanos == receipt.modified_nanos {
        Ok(Some(current))
    } else {
        Ok(None)
    }
}

async fn verify_completed_segments(
    part_path: &Path,
    meta: &mut TransferMeta,
) -> Result<bool, ReceiverError> {
    let completed = meta.completed_intervals.clone();
    let mut invalid = Vec::new();
    for segment in completed {
        if !is_blake3_hex(&segment.hash)
            || hash_part_range(part_path, segment.start, segment.end).await? != segment.hash
        {
            invalid.push(segment.interval());
        }
    }
    for interval in &invalid {
        meta.invalidate(*interval)?;
    }
    Ok(!invalid.is_empty())
}

fn path_has_len(path: &Path, total_bytes: u64) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.len() == total_bytes)
        .unwrap_or(false)
}

fn durable_prefix_end(completed: &IntervalSet, query: Interval) -> u64 {
    completed
        .as_slice()
        .iter()
        .find(|known| known.start <= query.start && known.end > query.start)
        .map_or(query.start, |known| known.end.min(query.end))
}

fn durable_status_snapshot(meta: &TransferMeta, query: Interval) -> (u64, Option<String>) {
    let committed_end = durable_prefix_end(&meta.completed_set(), query);
    let exact_hash = meta
        .completed_intervals
        .iter()
        .find(|completed| {
            completed.start == query.start
                && completed.end == committed_end
                && !completed.hash.is_empty()
        })
        .map(|completed| completed.hash.clone());
    (committed_end, exact_hash)
}

fn validate_offer(offer: &Offer3) -> Result<(), ReceiverError> {
    metadata::validate_transfer_key(&offer.transfer_key)
        .map_err(|_| ReceiverError::InvalidTransferKey)?;
    if !is_blake3_hex(&offer.transfer_key) {
        return Err(ReceiverError::InvalidTransferKey);
    }
    if !offer.content_hash.is_empty() && !is_blake3_hex(&offer.content_hash) {
        return Err(ReceiverError::InvalidContentHash);
    }
    Ok(())
}

fn supports_role_capabilities(hello: &Hello3) -> bool {
    let required: &[&str] = match hello.role {
        ConnectionRole::Control => &protocol::REQUIRED_TRANSFER_CAPABILITIES,
        ConnectionRole::Data => &protocol::REQUIRED_DATA_CAPABILITIES,
    };
    required
        .iter()
        .all(|capability| hello.capabilities.iter().any(|value| value == capability))
}

fn is_blake3_hex(value: &str) -> bool {
    value.len() == blake3::OUT_LEN * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn validate_completed_intervals(meta: &TransferMeta) -> Result<(), ReceiverError> {
    let mut set = IntervalSet::new();
    for completed in &meta.completed_intervals {
        let interval = completed.interval();
        if interval.start >= interval.end
            || interval.end > meta.total_bytes
            || set.overlaps(interval)
        {
            return Err(ReceiverError::InvalidMetadata);
        }
        set.insert(interval);
    }
    if meta.phase != metadata::TransferPhase::Receiving
        && !set.covers(Interval {
            start: 0,
            end: meta.total_bytes,
        })
    {
        return Err(ReceiverError::InvalidMetadata);
    }
    Ok(())
}

fn safe_filename(filename: &str) -> String {
    let basename = filename
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or("received_file");
    let mut sanitized: String = basename
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
            {
                '_'
            } else {
                character
            }
        })
        .collect();
    sanitized.truncate(
        sanitized
            .char_indices()
            // Leave room for the largest collision suffix while staying below
            // the common 255-byte component limit.
            .take_while(|(index, character)| index + character.len_utf8() <= 220)
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0),
    );
    let trimmed_len = sanitized.trim_end_matches([' ', '.']).len();
    sanitized.truncate(trimmed_len);
    let reserved_stem = sanitized
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if is_reserved_windows_filename(&reserved_stem) {
        sanitized.insert(0, '_');
    }
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "received_file".into()
    } else {
        sanitized
    }
}

fn is_reserved_windows_filename(stem: &str) -> bool {
    matches!(stem, "con" | "prn" | "aux" | "nul")
        || stem
            .strip_prefix("com")
            .or_else(|| stem.strip_prefix("lpt"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}

fn split_filename(filename: &str) -> (&str, &str) {
    match filename.rfind('.') {
        Some(index) if index > 0 && index < filename.len() - 1 => {
            (&filename[..index], &filename[index..])
        }
        _ => (filename, ""),
    }
}

#[cfg(test)]
fn publish_part_no_clobber(
    part_path: &Path,
    save_dir: &Path,
    filename: &str,
) -> io::Result<(PathBuf, String)> {
    let filename = safe_filename(filename);
    let (base, extension) = split_filename(&filename);
    let expected_hash = hash_file(part_path)?;

    for index in 0..=9999u16 {
        let candidate = if index == 0 {
            filename.clone()
        } else {
            format!("{base}({index}){extension}")
        };
        match publish_candidate(part_path, save_dir, &candidate, &expected_hash) {
            Ok(path) => return Ok((path, candidate)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let timestamp = timestamp_ms();
    for attempt in 0..1024u16 {
        let candidate = format!("{base}({timestamp}-{attempt}){extension}");
        match publish_candidate(part_path, save_dir, &candidate, &expected_hash) {
            Ok(path) => return Ok((path, candidate)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a collision-free destination filename",
    ))
}

fn publish_candidate(
    part_path: &Path,
    save_dir: &Path,
    filename: &str,
    expected_hash: &str,
) -> io::Result<PathBuf> {
    let final_path = save_dir.join(filename);
    match atomicwrites::move_atomic(part_path, &final_path) {
        Ok(()) => {}
        Err(error) if publish_copy_fallback_allowed(&error) => {
            copy_part_no_clobber(part_path, &final_path, expected_hash)?;
        }
        Err(error) => return Err(error),
    }
    sync_directory(save_dir)?;
    if let Some(resume_dir) = part_path.parent() {
        sync_directory(resume_dir)?;
    }
    Ok(final_path)
}

/// Android's emulated external storage supports rename but deliberately does
/// not expose hard links. `atomicwrites::move_atomic` uses renameat2/MoveFileEx
/// first and can fall back to a hard link on older filesystems; if that final
/// fallback is unavailable, copy into an O_EXCL destination and fsync it before
/// removing the durable part file. The destination is app-private staging, so
/// it is never exposed to MediaStore until this function has returned.
fn publish_copy_fallback_allowed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied | io::ErrorKind::Other
    )
}

fn copy_part_no_clobber(
    part_path: &Path,
    final_path: &Path,
    expected_hash: &str,
) -> io::Result<()> {
    let mut source = std::fs::File::open(part_path)?;
    let expected_len = source.metadata()?.len();
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(final_path)?;
    let mut copied = 0u64;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = match std::io::Read::read(&mut source, &mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                drop(destination);
                let _ = std::fs::remove_file(final_path);
                return Err(error);
            }
        };
        if let Err(error) = std::io::Write::write_all(&mut destination, &buffer[..read]) {
            drop(destination);
            let _ = std::fs::remove_file(final_path);
            return Err(error);
        }
        hasher.update(&buffer[..read]);
        copied = copied.saturating_add(read as u64);
    }
    if copied != expected_len || hasher.finalize().to_hex().as_str() != expected_hash {
        drop(destination);
        let _ = std::fs::remove_file(final_path);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staging file changed while it was being copied for publication",
        ));
    }
    if let Err(error) = destination.sync_all() {
        drop(destination);
        let _ = std::fs::remove_file(final_path);
        return Err(error);
    }
    drop(destination);
    if let Err(error) = std::fs::remove_file(part_path) {
        let _ = std::fs::remove_file(final_path);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)
        .and_then(|file| file.sync_all());
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
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
    SessionClosing,
    IncompleteTransfer,
    ActiveRangesDuringFinalize,
    InvalidMetadata,
    InvalidTransferKey,
    InvalidContentHash,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use tokio::sync::Barrier;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Callback;

    impl TransferCallback for Callback {
        fn on_receive_request(
            &self,
            _file_name: String,
            _file_size: u64,
            _sender_ip: String,
        ) -> bool {
            true
        }

        fn on_progress(&self, _transferred: u64, _total: u64) {}

        fn on_complete(&self, _success: bool, _msg: String) {}
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "locsd-receiver-{label}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn test_session(save_dir: PathBuf) -> Arc<ReceiverSession> {
        let transfer_key = "a".repeat(blake3::OUT_LEN * 2);
        let content_hash = "b".repeat(blake3::OUT_LEN * 2);
        Arc::new(ReceiverSession {
            offer: Offer3 {
                transfer_id: "transfer".into(),
                transfer_key: transfer_key.clone(),
                filename: "file.bin".into(),
                total_bytes: 1024,
                content_hash: content_hash.clone(),
                sender_device_id: "sender".into(),
                sender_name: "Sender".into(),
            },
            token: "token".into(),
            part_path: metadata::part_path(&save_dir, &transfer_key),
            meta: Mutex::new(TransferMeta::new(
                transfer_key,
                "sender".into(),
                "file.bin".into(),
                1024,
                content_hash,
            )),
            save_dir,
            active: Arc::new(ActiveRanges::default()),
            data_permits: Semaphore::new(RECEIVER_MAX_CONCURRENCY),
            lifecycle: Arc::new(SessionLifecycle::new()),
            callback: Arc::new(Callback),
            started: Instant::now(),
            last_progress_emit: StdMutex::new(Instant::now()),
            preverified_final: None,
            verification_generation: AtomicU64::new(0),
        })
    }

    fn offer_for(content: &[u8], filename: &str) -> Offer3 {
        Offer3 {
            transfer_id: format!("transfer-{}", TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)),
            transfer_key: "c".repeat(blake3::OUT_LEN * 2),
            filename: filename.into(),
            total_bytes: content.len() as u64,
            content_hash: blake3::hash(content).to_hex().to_string(),
            sender_device_id: "sender".into(),
            sender_name: "Sender".into(),
        }
    }

    fn meta_for(offer: &Offer3, content: &[u8]) -> TransferMeta {
        let mut meta = TransferMeta::new(
            offer.transfer_key.clone(),
            offer.sender_device_id.clone(),
            safe_filename(&offer.filename),
            offer.total_bytes,
            offer.content_hash.clone(),
        );
        if !content.is_empty() {
            meta.commit(
                Interval {
                    start: 0,
                    end: content.len() as u64,
                },
                blake3::hash(content).to_hex().to_string(),
            )
            .unwrap();
        }
        meta
    }

    fn test_context(root: &Path) -> ReceiverContext {
        ReceiverContext::new(
            root.to_path_buf(),
            Arc::new(Callback),
            Arc::new(OfferRegistry::default()),
        )
    }

    #[tokio::test]
    async fn pending_offer_can_be_accepted_with_a_destination() {
        let registry = OfferRegistry::default();
        let receiver = registry.register("offer-1".into());
        let destination = PathBuf::from("chosen-directory");
        assert!(registry.accept("offer-1", destination.clone()));
        match receiver.await.unwrap() {
            OfferDecision::Accept {
                destination: actual,
            } => assert_eq!(actual, destination),
            OfferDecision::Reject => panic!("offer was unexpectedly rejected"),
        }
        assert!(!registry.accept("offer-1", PathBuf::from("second-choice")));
    }

    #[tokio::test]
    async fn pending_offer_can_be_rejected_only_once() {
        let registry = OfferRegistry::default();
        let receiver = registry.register("offer-2".into());
        assert!(registry.reject("offer-2"));
        assert!(matches!(receiver.await.unwrap(), OfferDecision::Reject));
        assert!(!registry.reject("offer-2"));
    }

    #[tokio::test]
    async fn one_transfer_key_has_only_one_session_owner() {
        let context = Arc::new(ReceiverContext::new(
            PathBuf::from("unused"),
            Arc::new(Callback),
            Arc::new(OfferRegistry::default()),
        ));
        assert!(context.reserve_transfer_key("same-key", "first").await);
        let waiting = {
            let context = context.clone();
            tokio::spawn(async move { context.reserve_transfer_key("same-key", "second").await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished());
        context
            .release_transfer_key("same-key", "wrong-owner")
            .await;
        assert!(!waiting.is_finished());
        context.release_transfer_key("same-key", "first").await;
        assert!(waiting.await.unwrap());
    }

    #[test]
    fn offer_identity_fields_require_canonical_blake3_hex() {
        let mut offer = test_session(PathBuf::from("unused")).offer.clone();
        assert!(validate_offer(&offer).is_ok());

        offer.transfer_key = "../resume".into();
        assert!(matches!(
            validate_offer(&offer),
            Err(ReceiverError::InvalidTransferKey)
        ));
        offer.transfer_key = "A".repeat(blake3::OUT_LEN * 2);
        assert!(matches!(
            validate_offer(&offer),
            Err(ReceiverError::InvalidTransferKey)
        ));
        offer.transfer_key = "a".repeat(blake3::OUT_LEN * 2);
        offer.content_hash.clear();
        assert!(validate_offer(&offer).is_ok());
        offer.content_hash = "b".repeat(blake3::OUT_LEN * 2 - 1);
        assert!(matches!(
            validate_offer(&offer),
            Err(ReceiverError::InvalidContentHash)
        ));
    }

    #[tokio::test]
    async fn overlapping_ranges_are_reserved_atomically_and_released_by_guard() {
        let session = test_session(PathBuf::from("unused"));
        let first = match session
            .reserve_range(1, Interval { start: 0, end: 700 })
            .await
        {
            ReserveRange::Reserved(reservation) => reservation,
            _ => panic!("first interval should be reserved"),
        };

        let contender = {
            let session = session.clone();
            tokio::spawn(async move {
                matches!(
                    session
                        .reserve_range(
                            2,
                            Interval {
                                start: 512,
                                end: 1024,
                            },
                        )
                        .await,
                    ReserveRange::Rejected(_)
                )
            })
        };
        assert!(contender.await.unwrap());
        assert_eq!(session.active.len(), 1);
        drop(first);
        assert_eq!(session.active.len(), 0);
        assert!(matches!(
            session
                .reserve_range(
                    2,
                    Interval {
                        start: 512,
                        end: 1024,
                    },
                )
                .await,
            ReserveRange::Reserved(_)
        ));
    }

    #[tokio::test]
    async fn data_connections_are_limited_by_the_session_semaphore() {
        let session = test_session(PathBuf::from("unused"));
        let entered = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(RECEIVER_MAX_CONCURRENCY + 2));
        let release = CancellationToken::new();
        let mut tasks = Vec::new();

        for _ in 0..=RECEIVER_MAX_CONCURRENCY {
            let session = session.clone();
            let entered = entered.clone();
            let start = start.clone();
            let release = release.clone();
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                let _connection = session.enter_data_connection().await.unwrap();
                entered.fetch_add(1, Ordering::SeqCst);
                release.cancelled().await;
            }));
        }
        start.wait().await;

        tokio::time::timeout(Duration::from_secs(2), async {
            while entered.load(Ordering::SeqCst) < RECEIVER_MAX_CONCURRENCY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(entered.load(Ordering::SeqCst), RECEIVER_MAX_CONCURRENCY);

        release.cancel();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(entered.load(Ordering::SeqCst), RECEIVER_MAX_CONCURRENCY + 1);
    }

    #[tokio::test]
    async fn finalizing_waits_for_data_connections_and_rejects_new_ones() {
        let session = test_session(PathBuf::from("unused"));
        let connection = session.enter_data_connection().await.unwrap();
        let finalizing = {
            let session = session.clone();
            tokio::spawn(async move { session.begin_finalizing().await })
        };

        tokio::time::timeout(Duration::from_secs(1), async {
            while session.lifecycle.phase() != SessionPhase::Finalizing {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            session.enter_data_connection().await,
            Err(ReceiverError::SessionClosing)
        ));
        assert!(!finalizing.is_finished());

        drop(connection);
        tokio::time::timeout(Duration::from_secs(1), finalizing)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.active.len(), 0);
    }

    #[tokio::test]
    async fn cold_resume_discards_segments_whose_bytes_no_longer_match() {
        let root = temp_root("cold-corruption");
        std::fs::create_dir_all(metadata::resume_dir(&root)).unwrap();
        let expected_content = b"good";
        let offer = offer_for(expected_content, "cold.bin");
        let part_path = metadata::part_path(&root, &offer.transfer_key);
        std::fs::write(&part_path, b"baad").unwrap();
        let meta = meta_for(&offer, expected_content);
        metadata::save_atomic(&root, &meta).unwrap();

        let session = test_context(&root)
            .prepare_session(offer.clone(), root.clone(), "token".into())
            .await
            .unwrap();
        let resumed = session.meta.lock().await.clone();
        assert_eq!(resumed.durable_bytes(), 0);
        assert_eq!(resumed.phase, metadata::TransferPhase::Receiving);
        assert!(resumed.final_filename.is_none());
        assert_eq!(
            metadata::load(&root, &offer.transfer_key)
                .unwrap()
                .unwrap()
                .durable_bytes(),
            0
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn finalizing_part_reuses_recorded_name_and_keeps_completed_receipt() {
        let root = temp_root("finalizing-part");
        std::fs::create_dir_all(metadata::resume_dir(&root)).unwrap();
        let content = b"verified payload";
        let offer = offer_for(content, "original.bin");
        let part_path = metadata::part_path(&root, &offer.transfer_key);
        std::fs::write(&part_path, content).unwrap();
        let mut meta = meta_for(&offer, content);
        meta.mark_finalizing("recorded.bin").unwrap();
        metadata::save_atomic(&root, &meta).unwrap();

        let context = test_context(&root);
        let session = context
            .prepare_session(offer.clone(), root.clone(), "token-1".into())
            .await
            .unwrap();
        assert_eq!(
            session.meta.lock().await.phase,
            metadata::TransferPhase::Finalizing
        );
        let expected_hash = offer.content_hash.clone();
        let done = session
            .finalize(&expected_hash, expected_hash.clone())
            .await
            .unwrap();
        assert_eq!(done.final_filename, "recorded.bin");
        assert_eq!(std::fs::read(root.join("recorded.bin")).unwrap(), content);
        assert!(!part_path.exists());

        let receipt = metadata::load(&root, &offer.transfer_key).unwrap().unwrap();
        assert_eq!(receipt.phase, metadata::TransferPhase::Completed);
        assert_eq!(receipt.final_filename.as_deref(), Some("recorded.bin"));

        let resumed = context
            .prepare_session(offer, root.clone(), "token-2".into())
            .await
            .unwrap();
        assert_eq!(
            resumed.meta.lock().await.durable_bytes(),
            content.len() as u64
        );
        assert_eq!(
            resumed
                .finalize(&expected_hash, expected_hash.clone())
                .await
                .unwrap()
                .final_filename,
            "recorded.bin"
        );
        assert!(!root.join("original.bin").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn finalizing_receipt_recovers_when_publish_won_but_part_is_gone() {
        let root = temp_root("finalizing-published");
        std::fs::create_dir_all(metadata::resume_dir(&root)).unwrap();
        let content = b"already published";
        let offer = offer_for(content, "published.bin");
        std::fs::write(root.join("published.bin"), content).unwrap();
        let mut meta = meta_for(&offer, content);
        meta.mark_finalizing("published.bin").unwrap();
        metadata::save_atomic(&root, &meta).unwrap();

        let session = test_context(&root)
            .prepare_session(offer.clone(), root.clone(), "token".into())
            .await
            .unwrap();
        assert!(session.preverified_final.is_some());
        assert_eq!(
            session.meta.lock().await.phase,
            metadata::TransferPhase::Completed
        );
        assert_eq!(
            session
                .finalize(&offer.content_hash, offer.content_hash.clone())
                .await
                .unwrap()
                .final_filename,
            "published.bin"
        );
        assert_eq!(
            metadata::load(&root, &offer.transfer_key)
                .unwrap()
                .unwrap()
                .phase,
            metadata::TransferPhase::Completed
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn durable_status_reports_only_the_contiguous_prefix() {
        let completed = IntervalSet::from_intervals([
            Interval { start: 0, end: 100 },
            Interval {
                start: 100,
                end: 200,
            },
            Interval {
                start: 300,
                end: 400,
            },
        ]);
        assert_eq!(
            durable_prefix_end(
                &completed,
                Interval {
                    start: 50,
                    end: 350,
                }
            ),
            200
        );
        assert_eq!(
            durable_prefix_end(
                &completed,
                Interval {
                    start: 250,
                    end: 350,
                }
            ),
            250
        );

        let mut meta = TransferMeta::new(
            "a".repeat(blake3::OUT_LEN * 2),
            "sender".into(),
            "file.bin".into(),
            400,
            "b".repeat(blake3::OUT_LEN * 2),
        );
        meta.commit(Interval { start: 0, end: 100 }, "cached-hash".into())
            .unwrap();
        assert_eq!(
            durable_status_snapshot(&meta, Interval { start: 0, end: 100 }),
            (100, Some("cached-hash".into()))
        );
    }

    #[test]
    fn filenames_are_sanitized_for_android_and_windows_before_transfer() {
        assert_eq!(safe_filename("../folder\\report?.txt"), "report_.txt");
        assert_eq!(safe_filename("CON.txt"), "_CON.txt");
        assert_eq!(safe_filename("..."), "received_file");
        assert!(!safe_filename(&"文".repeat(200)).is_empty());
        assert!(safe_filename(&"文".repeat(200)).len() <= 220);
    }

    #[test]
    fn copy_publish_fallback_is_no_clobber_and_hash_checked() {
        let root = temp_root("copy-fallback");
        std::fs::create_dir_all(&root).unwrap();
        let part = root.join("part.bin");
        let final_path = root.join("final.bin");
        std::fs::write(&part, b"durable bytes").unwrap();
        std::fs::write(&final_path, b"existing").unwrap();
        let expected = blake3::hash(b"durable bytes").to_hex().to_string();

        let collision = copy_part_no_clobber(&part, &final_path, &expected).unwrap_err();
        assert_eq!(collision.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing");
        assert!(part.exists());

        std::fs::remove_file(&final_path).unwrap();
        copy_part_no_clobber(&part, &final_path, &expected).unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"durable bytes");
        assert!(!part.exists());

        let corrupt_part = root.join("corrupt.part");
        let corrupt_final = root.join("corrupt.bin");
        std::fs::write(&corrupt_part, b"changed").unwrap();
        let mismatch = copy_part_no_clobber(&corrupt_part, &corrupt_final, &expected).unwrap_err();
        assert_eq!(mismatch.kind(), io::ErrorKind::InvalidData);
        assert!(corrupt_part.exists());
        assert!(!corrupt_final.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn final_publish_never_overwrites_an_existing_name() {
        let root = temp_root("no-clobber");
        std::fs::create_dir_all(metadata::resume_dir(&root)).unwrap();
        let existing = root.join("file.bin");
        let part = metadata::resume_dir(&root).join("part.bin");
        std::fs::write(&existing, b"existing").unwrap();
        std::fs::write(&part, b"incoming").unwrap();

        let (published, name) = publish_part_no_clobber(&part, &root, "file.bin").unwrap();
        assert_eq!(name, "file(1).bin");
        assert_eq!(std::fs::read(existing).unwrap(), b"existing");
        assert_eq!(std::fs::read(published).unwrap(), b"incoming");
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_final_publishers_get_distinct_names() {
        let root = temp_root("concurrent-no-clobber");
        let resume = metadata::resume_dir(&root);
        std::fs::create_dir_all(&resume).unwrap();
        let first_part = resume.join("first.part");
        let second_part = resume.join("second.part");
        std::fs::write(&first_part, b"first").unwrap();
        std::fs::write(&second_part, b"second").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let first = {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                publish_part_no_clobber(&first_part, &root, "same.bin").unwrap()
            })
        };
        let second = {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                publish_part_no_clobber(&second_part, &root, "same.bin").unwrap()
            })
        };
        barrier.wait();
        let (first_path, first_name) = first.join().unwrap();
        let (second_path, second_name) = second.join().unwrap();
        assert_ne!(first_name, second_name);
        let mut contents = vec![
            std::fs::read(first_path).unwrap(),
            std::fs::read(second_path).unwrap(),
        ];
        contents.sort();
        assert_eq!(contents, vec![b"first".to_vec(), b"second".to_vec()]);
        let _ = std::fs::remove_dir_all(root);
    }
}
