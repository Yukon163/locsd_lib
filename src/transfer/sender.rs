use super::events::{
    TransferCallback, TransferDirection, TransferEvent, TransferId, TransferState,
};
use super::identity::{FileIdentity, SourceSnapshot, identify_file};
use super::metrics::MetricsCollector;
use super::protocol::{
    self, Ack3, AckStatus, ConnectionRole, Done3, Error3, Final3, FrameKind, Hello3, Invalidate3,
    Offer3, Pause3, Plan3, ProtocolError, Range3,
};
use super::range::{Interval, IntervalSet};
use super::scheduler::{
    AdaptiveScheduler, DIAGNOSTIC_WINDOW, MAX_CONCURRENCY, RangeTask, SCHEDULER_TICK,
};
use blake3::Hasher;
use log::{debug, info};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{interval, timeout};
use tokio_util::sync::CancellationToken;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const DATA_PAYLOAD_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub struct SendRequest {
    pub transfer_id: TransferId,
    pub target_ip: String,
    pub port: u16,
    pub file_path: PathBuf,
    pub sender_device_id: String,
    pub sender_name: String,
    pub callback: Arc<dyn TransferCallback>,
}

pub enum V3SendOutcome {
    Completed(Done3),
    CompatibilityRequired,
}

pub async fn send_v3(
    request: SendRequest,
    cancellation: CancellationToken,
) -> Result<V3SendOutcome, SenderError> {
    let started = Instant::now();
    let file_name = request
        .file_path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or(SenderError::InvalidFilename)?;
    emit(
        &request,
        TransferState::Hashing,
        0,
        0,
        0,
        0,
        0.0,
        started,
        None,
        None,
    );
    let identity_path = request.file_path.clone();
    let sender_id = request.sender_device_id.clone();
    let identity_name = file_name.clone();
    let identity = tokio::task::spawn_blocking(move || {
        identify_file(identity_path, &sender_id, &identity_name)
    })
    .await
    .map_err(SenderError::Join)??;
    if cancellation.is_cancelled() {
        return Err(SenderError::Cancelled);
    }

    emit(
        &request,
        TransferState::Negotiating,
        0,
        0,
        0,
        0,
        0.0,
        started,
        None,
        None,
    );
    let mut control = timeout(
        HANDSHAKE_TIMEOUT,
        TcpStream::connect((request.target_ip.as_str(), request.port)),
    )
    .await
    .map_err(|_| SenderError::HandshakeTimeout)??;
    control.set_nodelay(true)?;
    let hello = Hello3 {
        protocol_version: protocol::PROTOCOL_VERSION,
        role: ConnectionRole::Control,
        capabilities: vec![
            "range".into(),
            "ack".into(),
            "pause".into(),
            "blake3".into(),
            "resume".into(),
            "invalidate".into(),
        ],
        receiver_max_concurrency: MAX_CONCURRENCY as u16,
        session_token: None,
    };
    if protocol::write_control(&mut control, FrameKind::Hello, &hello)
        .await
        .is_err()
    {
        return Ok(V3SendOutcome::CompatibilityRequired);
    }
    match timeout(HANDSHAKE_TIMEOUT, read_hello_response(&mut control)).await {
        Ok(Ok(HelloResponse::Hello)) => {}
        Ok(Ok(HelloResponse::Unsupported)) | Err(_) => {
            return Ok(V3SendOutcome::CompatibilityRequired);
        }
        Ok(Err(error @ SenderError::Remote(_, _))) => return Err(error),
        Ok(Err(_)) => return Ok(V3SendOutcome::CompatibilityRequired),
    }

    let offer = Offer3 {
        transfer_id: request.transfer_id.clone(),
        transfer_key: identity.transfer_key.clone(),
        filename: file_name.clone(),
        total_bytes: identity.snapshot.size,
        content_hash: identity.content_id.clone(),
        sender_device_id: request.sender_device_id.clone(),
        sender_name: request.sender_name.clone(),
    };
    protocol::write_control(&mut control, FrameKind::Offer, &offer).await?;
    let plan: Plan3 = protocol::read_control(&mut control, FrameKind::Plan).await?;
    if !plan.accepted {
        return Err(SenderError::Rejected(
            plan.reason
                .unwrap_or_else(|| "receiver rejected offer".into()),
        ));
    }
    let completed = validate_plan(&plan, identity.snapshot.size)?;
    let missing = completed.complement(identity.snapshot.size);
    let durable = completed.total_len();
    emit(
        &request,
        TransferState::Transferring,
        durable,
        0,
        0,
        0,
        0.0,
        started,
        None,
        None,
    );

    transfer_ranges(
        &request,
        &identity,
        &plan.session_token,
        plan.receiver_max_concurrency as usize,
        missing,
        durable,
        started,
        cancellation.clone(),
    )
    .await?;
    if !identity.snapshot.verify_unchanged()? {
        return Err(SenderError::SourceChanged);
    }
    if cancellation.is_cancelled() {
        return Err(SenderError::Cancelled);
    }

    emit(
        &request,
        TransferState::Verifying,
        identity.snapshot.size,
        0,
        0,
        0,
        0.0,
        started,
        None,
        None,
    );
    let final_frame = Final3 {
        transfer_id: request.transfer_id.clone(),
        content_hash: identity.content_id.clone(),
    };
    protocol::write_control(&mut control, FrameKind::Final, &final_frame).await?;
    let done: Done3 = timeout(
        ACK_TIMEOUT,
        protocol::read_control(&mut control, FrameKind::Done),
    )
    .await
    .map_err(|_| SenderError::AckTimeout)??;
    if !done.verified || done.total_bytes != identity.snapshot.size {
        return Err(SenderError::FinalVerificationFailed);
    }
    emit(
        &request,
        TransferState::Completed,
        identity.snapshot.size,
        0,
        0,
        0,
        0.0,
        started,
        None,
        Some("发送完成".into()),
    );
    Ok(V3SendOutcome::Completed(done))
}

#[allow(clippy::too_many_arguments)]
async fn transfer_ranges(
    request: &SendRequest,
    identity: &FileIdentity,
    session_token: &str,
    receiver_limit: usize,
    missing: Vec<Interval>,
    durable_at_start: u64,
    started: Instant,
    cancellation: CancellationToken,
) -> Result<(), SenderError> {
    if missing.is_empty() {
        return Ok(());
    }
    let scheduler = Arc::new(Mutex::new(AdaptiveScheduler::new(
        identity.snapshot.size,
        missing,
        receiver_limit,
    )));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut controls: HashMap<usize, mpsc::UnboundedSender<WorkerControl>> = HashMap::new();
    let mut retiring = HashSet::new();
    let mut next_worker_id = 0usize;
    let initial = scheduler.lock().await.target();
    for _ in 0..initial {
        spawn_worker(
            next_worker_id,
            request,
            session_token,
            identity.snapshot.clone(),
            scheduler.clone(),
            event_tx.clone(),
            &mut controls,
            cancellation.clone(),
        );
        next_worker_id += 1;
    }

    let mut metrics = MetricsCollector::new();
    let mut scheduler_tick = interval(SCHEDULER_TICK);
    let mut metrics_tick = interval(DIAGNOSTIC_WINDOW);
    scheduler_tick.tick().await;
    metrics_tick.tick().await;
    let mut fatal: Option<SenderError> = None;
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => {
                stop_workers(&mut controls, &mut event_rx).await;
                return Err(SenderError::Cancelled);
            }
            Some(event) = event_rx.recv() => {
                match event {
                    WorkerEvent::Ack { range_id, committed_end, bytes, elapsed } => {
                        let mut locked = scheduler.lock().await;
                        locked.acknowledge(range_id, committed_end);
                        metrics.record_ack(bytes, elapsed);
                    }
                    WorkerEvent::Failed { range_id, error, timeout_or_reset } => {
                        metrics.record_error(timeout_or_reset);
                        let retrying = scheduler.lock().await.fail(range_id);
                        if !retrying { fatal = Some(error); }
                    }
                    WorkerEvent::Stopped { worker_id } => {
                        debug!("V3 worker {worker_id} stopped");
                        controls.remove(&worker_id);
                        retiring.remove(&worker_id);
                    }
                }
            }
            _ = scheduler_tick.tick() => {
                let locked = scheduler.lock().await;
                let total_durable = locked.durable_bytes().max(durable_at_start);
                let in_flight = locked.in_flight_bytes();
                emit(
                    request, TransferState::Transferring, total_durable, in_flight,
                    locked.active_len(), locked.target(), 0.0, started, None, None,
                );
                if locked.is_complete() { break; }
                drop(locked);
                if let Some(error) = fatal.take() {
                    cancellation.cancel();
                    stop_workers(&mut controls, &mut event_rx).await;
                    return Err(error);
                }
            }
            _ = metrics_tick.tick() => {
                let (active, pending) = {
                    let locked = scheduler.lock().await;
                    (locked.active_len(), locked.pending_len())
                };
                let window = metrics.roll(active, pending);
                let decision = scheduler.lock().await.on_window(&window);
                info!(
                    target: "locsd::transfer",
                    "transfer_id={} event=window ack_bytes={} throughput_bps={:.0} ewma_bps={:.0} errors={} resets={} active={} pending={} target={}",
                    request.transfer_id,
                    window.ack_bytes,
                    window.throughput_bytes_per_second,
                    window.ewma_bytes_per_second,
                    window.errors,
                    window.timeouts_or_resets,
                    active,
                    pending,
                    scheduler.lock().await.target(),
                );
                for _ in 0..decision.spawn_workers {
                    if controls.len() >= MAX_CONCURRENCY { break; }
                    spawn_worker(
                        next_worker_id, request, session_token, identity.snapshot.clone(), scheduler.clone(),
                        event_tx.clone(), &mut controls, cancellation.clone(),
                    );
                    next_worker_id += 1;
                }
                if decision.retire_workers > 0 {
                    let candidates: Vec<_> = controls
                        .keys()
                        .filter(|worker_id| !retiring.contains(*worker_id))
                        .copied()
                        .take(decision.retire_workers)
                        .collect();
                    for worker_id in candidates {
                        if let Some(control) = controls.get(&worker_id) {
                            let _ = control.send(WorkerControl::Retire);
                            retiring.insert(worker_id);
                        }
                    }
                }
                let split_candidate = decision.pause_for_split.or_else(|| {
                    if pending == 0 { scheduler.try_lock().ok().and_then(|locked| locked.tail_rescue_candidate()) } else { None }
                });
                let split_control = if let Some(range_id) = split_candidate {
                    scheduler.lock().await.worker_for_range(range_id)
                        .and_then(|worker_id| controls.get(&worker_id))
                        .map(|control| (control, range_id))
                } else {
                    None
                };
                if let Some((control, range_id)) = split_control {
                    let _ = control.send(WorkerControl::Pause(range_id));
                }
                if !decision.rollback_children.is_empty() {
                    let workers = scheduler.lock().await.start_rollback(&decision.rollback_children);
                    for worker_id in workers {
                        let range_id = decision.rollback_children.iter().copied().find(|id| {
                            scheduler.try_lock().ok().and_then(|locked| locked.worker_for_range(*id)) == Some(worker_id)
                        });
                        if let Some((control, range_id)) = controls
                            .get(&worker_id)
                            .zip(range_id)
                        {
                            let _ = control.send(WorkerControl::Pause(range_id));
                        }
                    }
                }
                let (durable, in_flight, current_active, target) = {
                    let locked = scheduler.lock().await;
                    (
                        locked.durable_bytes().max(durable_at_start),
                        locked.in_flight_bytes(),
                        locked.active_len(),
                        locked.target(),
                    )
                };
                emit(
                    request, TransferState::Transferring, durable, in_flight, current_active, target,
                    window.ewma_bytes_per_second, started, None, None,
                );
            }
        }
    }
    stop_workers(&mut controls, &mut event_rx).await;
    Ok(())
}

async fn stop_workers(
    controls: &mut HashMap<usize, mpsc::UnboundedSender<WorkerControl>>,
    event_rx: &mut mpsc::UnboundedReceiver<WorkerEvent>,
) {
    for control in controls.values() {
        let _ = control.send(WorkerControl::Retire);
    }
    while !controls.is_empty() {
        match event_rx.recv().await {
            Some(WorkerEvent::Stopped { worker_id }) => {
                controls.remove(&worker_id);
            }
            Some(_) => {}
            None => break,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_worker(
    worker_id: usize,
    request: &SendRequest,
    session_token: &str,
    snapshot: SourceSnapshot,
    scheduler: Arc<Mutex<AdaptiveScheduler>>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
    controls: &mut HashMap<usize, mpsc::UnboundedSender<WorkerControl>>,
    cancellation: CancellationToken,
) {
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    controls.insert(worker_id, control_tx);
    let target_ip = request.target_ip.clone();
    let token = session_token.to_string();
    let port = request.port;
    tokio::spawn(async move {
        data_worker(
            worker_id,
            target_ip,
            port,
            token,
            snapshot,
            scheduler,
            event_tx.clone(),
            control_rx,
            cancellation,
        )
        .await;
        let _ = event_tx.send(WorkerEvent::Stopped { worker_id });
    });
}

#[allow(clippy::too_many_arguments)]
async fn data_worker(
    worker_id: usize,
    target_ip: String,
    port: u16,
    session_token: String,
    snapshot: SourceSnapshot,
    scheduler: Arc<Mutex<AdaptiveScheduler>>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
    mut controls: mpsc::UnboundedReceiver<WorkerControl>,
    cancellation: CancellationToken,
) {
    let mut stream: Option<TcpStream> = None;
    let mut retire = false;
    loop {
        if cancellation.is_cancelled() || retire {
            return;
        }
        while let Ok(command) = controls.try_recv() {
            if matches!(command, WorkerControl::Retire) {
                retire = true;
            }
        }
        if retire {
            return;
        }
        let task = scheduler.lock().await.take_next(worker_id);
        let Some(task) = task else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        if stream.is_none() {
            let connected = tokio::select! {
                _ = cancellation.cancelled() => return,
                result = connect_data(&target_ip, port, &session_token) => result,
            };
            match connected {
                Ok(connected) => stream = Some(connected),
                Err(error) => {
                    let timeout_or_reset = error.is_timeout_or_reset();
                    let _ = event_tx.send(WorkerEvent::Failed {
                        range_id: task.range_id,
                        error,
                        timeout_or_reset,
                    });
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            }
        }
        let result = tokio::select! {
            _ = cancellation.cancelled() => return,
            result = send_range(
                stream.as_mut().unwrap(),
                &snapshot,
                &task,
                &scheduler,
                &mut controls,
                &cancellation,
            ) => result,
        };
        match result {
            Ok(result) => {
                retire |= result.retire;
                let _ = event_tx.send(WorkerEvent::Ack {
                    range_id: task.range_id,
                    committed_end: result.ack.committed_end,
                    bytes: result.ack.committed_end.saturating_sub(task.interval.start),
                    elapsed: result.elapsed,
                });
            }
            Err(error) => {
                let timeout_or_reset = error.is_timeout_or_reset();
                stream = None;
                let _ = event_tx.send(WorkerEvent::Failed {
                    range_id: task.range_id,
                    error,
                    timeout_or_reset,
                });
            }
        }
    }
}

async fn connect_data(
    target_ip: &str,
    port: u16,
    session_token: &str,
) -> Result<TcpStream, SenderError> {
    let mut stream = timeout(HANDSHAKE_TIMEOUT, TcpStream::connect((target_ip, port)))
        .await
        .map_err(|_| SenderError::HandshakeTimeout)??;
    stream.set_nodelay(true)?;
    let hello = Hello3 {
        protocol_version: protocol::PROTOCOL_VERSION,
        role: ConnectionRole::Data,
        capabilities: vec![
            "range".into(),
            "ack".into(),
            "pause".into(),
            "invalidate".into(),
        ],
        receiver_max_concurrency: 0,
        session_token: Some(session_token.into()),
    };
    protocol::write_control(&mut stream, FrameKind::Hello, &hello).await?;
    match timeout(HANDSHAKE_TIMEOUT, read_hello_response(&mut stream)).await {
        Ok(Ok(HelloResponse::Hello)) => Ok(stream),
        Ok(Ok(HelloResponse::Unsupported)) => Err(SenderError::Unsupported),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(SenderError::HandshakeTimeout),
    }
}

async fn send_range(
    stream: &mut TcpStream,
    snapshot: &SourceSnapshot,
    task: &RangeTask,
    scheduler: &Arc<Mutex<AdaptiveScheduler>>,
    controls: &mut mpsc::UnboundedReceiver<WorkerControl>,
    cancellation: &CancellationToken,
) -> Result<RangeResult, SenderError> {
    if !snapshot.verify_unchanged()? {
        return Err(SenderError::SourceChanged);
    }
    let started = Instant::now();
    let range = Range3 {
        range_id: task.range_id,
        start: task.interval.start,
        end: task.interval.end,
        generation: task.generation,
    };
    protocol::write_control(stream, FrameKind::Range, &range).await?;
    let mut file = File::open(&snapshot.path).await?;
    file.seek(SeekFrom::Start(task.interval.start)).await?;
    let mut offset = task.interval.start;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; DATA_PAYLOAD_BYTES];
    let mut retire = false;

    while offset < task.interval.end {
        if cancellation.is_cancelled() {
            return Err(SenderError::Cancelled);
        }
        while let Ok(command) = controls.try_recv() {
            match command {
                WorkerControl::Pause(range_id) if range_id == task.range_id => {
                    let pause = Pause3 {
                        range_id: task.range_id,
                        generation: task.generation,
                    };
                    protocol::write_control(stream, FrameKind::Pause, &pause).await?;
                    let ack = read_ack(stream, task, offset, &hasher).await?;
                    return Ok(RangeResult {
                        ack,
                        elapsed: started.elapsed(),
                        retire,
                    });
                }
                WorkerControl::Retire => retire = true,
                _ => {}
            }
        }
        let count = ((task.interval.end - offset) as usize).min(buffer.len());
        let read = file.read(&mut buffer[..count]).await?;
        if read == 0 {
            return Err(SenderError::UnexpectedSourceEof);
        }
        protocol::write_data(stream, offset, &buffer[..read]).await?;
        hasher.update(&buffer[..read]);
        offset += read as u64;
        scheduler
            .lock()
            .await
            .record_in_flight(task.range_id, offset - task.interval.start);
    }
    let ack = read_ack(stream, task, offset, &hasher).await?;
    Ok(RangeResult {
        ack,
        elapsed: started.elapsed(),
        retire,
    })
}

async fn read_ack(
    stream: &mut TcpStream,
    task: &RangeTask,
    expected_end: u64,
    hasher: &Hasher,
) -> Result<Ack3, SenderError> {
    let ack: Ack3 = timeout(ACK_TIMEOUT, protocol::read_control(stream, FrameKind::Ack))
        .await
        .map_err(|_| SenderError::AckTimeout)??;
    if ack.range_id != task.range_id || ack.committed_end != expected_end {
        return Err(SenderError::InvalidAck);
    }
    match ack.status {
        AckStatus::Committed | AckStatus::Duplicate => {
            let expected_hash = hasher.clone().finalize().to_hex().to_string();
            if ack.range_hash != expected_hash {
                invalidate_range(stream, task).await?;
                return Err(SenderError::RangeHashMismatch);
            }
        }
        AckStatus::Rejected => {
            return Err(SenderError::RangeRejected(ack.message.unwrap_or_default()));
        }
    }
    Ok(ack)
}

async fn invalidate_range(stream: &mut TcpStream, task: &RangeTask) -> Result<(), SenderError> {
    let invalidate = Invalidate3 {
        range_id: task.range_id,
        start: task.interval.start,
        end: task.interval.end,
        generation: task.generation,
        reason: "range hash mismatch".into(),
    };
    protocol::write_control(stream, FrameKind::Invalidate, &invalidate).await?;
    let ack: Ack3 = timeout(ACK_TIMEOUT, protocol::read_control(stream, FrameKind::Ack))
        .await
        .map_err(|_| SenderError::AckTimeout)??;
    if ack.range_id != task.range_id
        || ack.committed_end != task.interval.start
        || ack.status != AckStatus::Committed
    {
        return Err(SenderError::RangeInvalidationFailed);
    }
    Ok(())
}

fn validate_plan(plan: &Plan3, total_bytes: u64) -> Result<IntervalSet, SenderError> {
    let mut set = IntervalSet::new();
    for interval in &plan.completed_intervals {
        if interval.is_empty() || interval.end > total_bytes || set.overlaps(*interval) {
            return Err(SenderError::InvalidPlan);
        }
        set.insert(*interval);
    }
    Ok(set)
}

enum HelloResponse {
    Hello,
    Unsupported,
}

async fn read_hello_response(stream: &mut TcpStream) -> Result<HelloResponse, SenderError> {
    let (kind, payload) = protocol::read_payload(stream).await?;
    match kind {
        FrameKind::Hello => {
            let _: Hello3 = serde_json::from_slice(&payload)?;
            Ok(HelloResponse::Hello)
        }
        FrameKind::ErrorResponse => {
            let error: Error3 = serde_json::from_slice(&payload)?;
            if error.unsupported {
                Ok(HelloResponse::Unsupported)
            } else {
                Err(SenderError::Remote(error.code, error.message))
            }
        }
        actual => Err(ProtocolError::UnexpectedFrame {
            expected: FrameKind::Hello,
            actual,
        }
        .into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit(
    request: &SendRequest,
    state: TransferState,
    durable: u64,
    in_flight: u64,
    active: usize,
    target: usize,
    speed: f64,
    started: Instant,
    error_code: Option<String>,
    message: Option<String>,
) {
    let file_name = request
        .file_path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let total = request
        .file_path
        .metadata()
        .map(|value| value.len())
        .unwrap_or(0);
    let mut event = TransferEvent::new(
        request.transfer_id.clone(),
        TransferDirection::Send,
        state,
        file_name,
        total,
        started.elapsed(),
    );
    event.durable_bytes = durable.min(total);
    event.in_flight_bytes = in_flight;
    event.active_workers = active as u16;
    event.target_workers = target as u16;
    event.speed_bytes_per_second = speed;
    event.error_code = error_code;
    event.message = message;
    request.callback.on_event(event);
}

enum WorkerControl {
    Pause(u64),
    Retire,
}
enum WorkerEvent {
    Ack {
        range_id: u64,
        committed_end: u64,
        bytes: u64,
        elapsed: Duration,
    },
    Failed {
        range_id: u64,
        error: SenderError,
        timeout_or_reset: bool,
    },
    Stopped {
        worker_id: usize,
    },
}
struct RangeResult {
    ack: Ack3,
    elapsed: Duration,
    retire: bool,
}

#[derive(Debug)]
pub enum SenderError {
    Protocol(ProtocolError),
    Io(io::Error),
    Json(serde_json::Error),
    Join(tokio::task::JoinError),
    InvalidFilename,
    HandshakeTimeout,
    AckTimeout,
    Rejected(String),
    Remote(String, String),
    Unsupported,
    InvalidPlan,
    InvalidAck,
    RangeHashMismatch,
    RangeInvalidationFailed,
    RangeRejected(String),
    SourceChanged,
    UnexpectedSourceEof,
    FinalVerificationFailed,
    Cancelled,
}

impl SenderError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::HandshakeTimeout => "handshake_timeout",
            Self::AckTimeout => "ack_timeout",
            Self::Rejected(_) => "rejected",
            Self::Unsupported => "unsupported",
            Self::InvalidPlan => "invalid_plan",
            Self::InvalidAck => "invalid_ack",
            Self::RangeHashMismatch => "range_hash_mismatch",
            Self::RangeInvalidationFailed => "range_invalidation_failed",
            Self::RangeRejected(_) => "range_rejected",
            Self::SourceChanged => "source_changed",
            Self::UnexpectedSourceEof => "source_eof",
            Self::FinalVerificationFailed => "final_verification",
            Self::Cancelled => "cancelled",
            Self::Protocol(_) => "protocol",
            Self::Io(_) => "io",
            Self::Json(_) => "json",
            Self::Join(_) => "join",
            Self::InvalidFilename => "invalid_filename",
            Self::Remote(_, _) => "remote",
        }
    }

    fn is_timeout_or_reset(&self) -> bool {
        matches!(self, Self::HandshakeTimeout | Self::AckTimeout)
            || matches!(self, Self::Io(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted | io::ErrorKind::BrokenPipe))
    }
}

impl std::fmt::Display for SenderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for SenderError {}
impl From<ProtocolError> for SenderError {
    fn from(value: ProtocolError) -> Self {
        Self::Protocol(value)
    }
}
impl From<io::Error> for SenderError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<serde_json::Error> for SenderError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    struct NoopCallback;

    impl TransferCallback for NoopCallback {
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

    #[tokio::test]
    async fn pre_offer_hello_timeout_requests_v2_compatibility() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut prefix = [0u8; 4];
            stream.read_exact(&mut prefix).await.unwrap();
            assert_eq!(prefix, protocol::FRAME_MAGIC);
            tokio::time::sleep(HANDSHAKE_TIMEOUT + Duration::from_millis(100)).await;
        });

        let root = std::env::temp_dir().join(format!("locsd-v2-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bin");
        std::fs::write(&source, b"compatibility-check").unwrap();
        let request = SendRequest {
            transfer_id: "fallback-test".into(),
            target_ip: "127.0.0.1".into(),
            port,
            file_path: source,
            sender_device_id: "stable-test-device".into(),
            sender_name: "test".into(),
            callback: Arc::new(NoopCallback),
        };
        let outcome = send_v3(request, CancellationToken::new()).await.unwrap();
        assert!(matches!(outcome, V3SendOutcome::CompatibilityRequired));
        server.await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn hash_mismatch_invalidates_the_entire_scheduled_range_before_retry() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let bad_ack = Ack3 {
                range_id: 7,
                committed_end: 150,
                range_hash: "corrupt".into(),
                status: AckStatus::Committed,
                message: None,
            };
            protocol::write_control(&mut stream, FrameKind::Ack, &bad_ack)
                .await
                .unwrap();
            let invalidate: Invalidate3 =
                protocol::read_control(&mut stream, FrameKind::Invalidate)
                    .await
                    .unwrap();
            assert_eq!(invalidate.start, 100);
            assert_eq!(invalidate.end, 200);
            let ack = Ack3 {
                range_id: invalidate.range_id,
                committed_end: invalidate.start,
                range_hash: String::new(),
                status: AckStatus::Committed,
                message: None,
            };
            protocol::write_control(&mut stream, FrameKind::Ack, &ack)
                .await
                .unwrap();
        });
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let task = RangeTask {
            range_id: 7,
            interval: Interval {
                start: 100,
                end: 200,
            },
            generation: 2,
            attempts: 0,
            root_range_id: 7,
            created_at: Instant::now(),
        };
        let mut hasher = Hasher::new();
        hasher.update(b"expected bytes");
        let error = read_ack(&mut stream, &task, 150, &hasher)
            .await
            .unwrap_err();
        assert!(matches!(error, SenderError::RangeHashMismatch));
        server.await.unwrap();
    }
}
