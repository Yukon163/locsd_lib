use super::events::{
    TransferCallback, TransferDiagnostics, TransferDirection, TransferEvent, TransferId,
    TransferSegment, TransferState,
};
use super::identity::{
    CachedFileIdentity, FileIdentity, SendSource, SendSourceSnapshot, hash_source_snapshot,
    prepare_source, restore_cached_identity,
};
use super::metrics::MetricsCollector;
use super::protocol::{
    self, Ack3, AckStatus, CompletedSegment3, ConnectionRole, Done3, Error3, Final3, FrameKind,
    Hello3, Invalidate3, Offer3, Pause3, Plan3, ProtocolError, Range3, RangeStatus3,
    RangeStatusResult3,
};
use super::range::{Interval, IntervalSet};
use super::scheduler::{
    AdaptiveScheduler, DIAGNOSTIC_WINDOW, MAX_CONCURRENCY, RangeTask, SCHEDULER_TICK,
};
use blake3::Hasher;
use log::{debug, info};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{MissedTickBehavior, interval, timeout};
use tokio_util::sync::CancellationToken;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const FINAL_HASH_BYTES_PER_SECOND: u64 = 32 * 1024 * 1024;
const FINAL_VERIFICATION_MAX_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const RANGE_STATUS_BUSY_RETRY: Duration = Duration::from_millis(200);
const RANGE_STATUS_HASH_BYTES_PER_SECOND: u64 = 4 * 1024 * 1024;
const RANGE_STATUS_MAX_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const DATA_PAYLOAD_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub struct SendRequest {
    pub transfer_id: TransferId,
    pub target_ip: String,
    pub port: u16,
    pub source: SendSource,
    pub file_name: String,
    pub total_bytes: u64,
    pub identity_cache: Arc<StdMutex<Option<FileIdentity>>>,
    pub source_key: Option<String>,
    pub shared_identity_cache: Arc<StdMutex<HashMap<String, CachedFileIdentity>>>,
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
    let mut diagnostics = TransferDiagnostics::default();
    let file_name = request.file_name.clone();
    if file_name.is_empty() {
        return Err(SenderError::InvalidFilename);
    }
    emit(
        &request,
        TransferState::Negotiating,
        0,
        0,
        0,
        0,
        0,
        Vec::new(),
        0.0,
        diagnostics.clone(),
        started,
        None,
        None,
    );
    let shared_cache_key = request
        .source_key
        .as_ref()
        .map(|source_key| format!("{}|{}|{}", request.sender_device_id, file_name, source_key));
    let cached_identity = request.identity_cache.lock().unwrap().clone().or_else(|| {
        let cache_key = shared_cache_key.as_ref()?;
        let cached = request
            .shared_identity_cache
            .lock()
            .unwrap()
            .get(cache_key)
            .cloned()?;
        restore_cached_identity(request.source.clone(), &cached)
            .ok()
            .flatten()
    });
    let identity = if let Some(identity) = cached_identity
        && identity.snapshot.verify_unchanged()?
    {
        identity
    } else {
        prepare_source(
            request.source.clone(),
            &request.sender_device_id,
            &file_name,
            request.source_key.as_deref(),
        )?
    };
    if cancellation.is_cancelled() {
        return Err(SenderError::Cancelled);
    }

    let negotiating_started = Instant::now();
    emit(
        &request,
        TransferState::Negotiating,
        0,
        0,
        0,
        0,
        0,
        Vec::new(),
        0.0,
        diagnostics.clone(),
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
            protocol::CAP_RANGE_STATUS.into(),
            protocol::CAP_DEFERRED_HASH.into(),
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
    let peer_hello = match timeout(HANDSHAKE_TIMEOUT, read_hello_response(&mut control)).await {
        Ok(Ok(HelloResponse::Hello(hello)))
            if hello.role == ConnectionRole::Control
                && hello.receiver_max_concurrency > 0
                && supports_required_capabilities(&hello) =>
        {
            hello
        }
        Ok(Ok(HelloResponse::Hello(_))) => {
            return Ok(V3SendOutcome::CompatibilityRequired);
        }
        Ok(Ok(HelloResponse::Unsupported)) | Err(_) => {
            return Ok(V3SendOutcome::CompatibilityRequired);
        }
        Ok(Err(error @ SenderError::Remote(_, _))) => return Err(error),
        Ok(Err(_)) => return Ok(V3SendOutcome::CompatibilityRequired),
    };

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
    if plan.session_token.is_empty() || plan.receiver_max_concurrency == 0 {
        return Err(SenderError::InvalidPlan);
    }
    let resume_plan = validate_plan(&plan, identity.snapshot.size)?;
    let completed = resume_plan.completed;
    let data_payload_bytes = usize::try_from(plan.max_frame_bytes)
        .unwrap_or(usize::MAX)
        .min(protocol::MAX_DATA_FRAME)
        .checked_sub(8)
        .filter(|size| *size > 0)
        .map(|size| size.min(DATA_PAYLOAD_BYTES))
        .ok_or(SenderError::InvalidPlan)?;
    let missing = completed.complement(identity.snapshot.size);
    let durable = completed.total_len();
    diagnostics.negotiating_ms = negotiating_started.elapsed().as_millis() as u64;
    emit(
        &request,
        TransferState::Transferring,
        durable,
        0,
        0,
        0,
        0,
        completed
            .as_slice()
            .iter()
            .map(|interval| TransferSegment {
                start: interval.start,
                end: interval.end,
                downloaded_end: interval.end,
                running: false,
            })
            .collect(),
        0.0,
        diagnostics.clone(),
        started,
        None,
        None,
    );

    let receiver_limit = usize::from(
        plan.receiver_max_concurrency
            .min(peer_hello.receiver_max_concurrency),
    );
    let supports_range_status = has_capability(&peer_hello, protocol::CAP_RANGE_STATUS);
    let hashing_started = Instant::now();
    let known_content_hash = identity.content_id.clone();
    let hash_snapshot = identity.snapshot.clone();
    let hash_future = async move {
        if !known_content_hash.is_empty() {
            return Ok::<(String, u64), SenderError>((known_content_hash, 0));
        }
        let content_hash =
            tokio::task::spawn_blocking(move || hash_source_snapshot(&hash_snapshot, |_| {}))
                .await
                .map_err(SenderError::Join)??;
        Ok((content_hash, hashing_started.elapsed().as_millis() as u64))
    };
    let transfer_future = transfer_ranges(
        &request,
        &identity,
        &plan.session_token,
        receiver_limit,
        missing,
        durable,
        data_payload_bytes,
        supports_range_status,
        diagnostics,
        started,
        cancellation.clone(),
    );
    let resume_future = validate_resumed_segments(
        &request,
        &identity.snapshot,
        &plan.session_token,
        resume_plan.segments,
        cancellation.clone(),
    );
    let (transfer_result, resume_result, hash_result) =
        tokio::join!(transfer_future, resume_future, hash_future);
    diagnostics = transfer_result?;
    let invalidated = IntervalSet::from_intervals(resume_result?);
    let (content_hash, hashing_ms) = hash_result?;
    diagnostics.hashing_ms = hashing_ms;
    let mut identity = identity;
    identity.content_id = content_hash;
    *request.identity_cache.lock().unwrap() = Some(identity.clone());
    if let Some(cache_key) = shared_cache_key {
        request
            .shared_identity_cache
            .lock()
            .unwrap()
            .insert(cache_key, identity.cache_entry());
    }
    if !invalidated.as_slice().is_empty() {
        diagnostics = transfer_ranges(
            &request,
            &identity,
            &plan.session_token,
            receiver_limit,
            invalidated.as_slice().to_vec(),
            identity
                .snapshot
                .size
                .saturating_sub(invalidated.total_len()),
            data_payload_bytes,
            supports_range_status,
            diagnostics,
            started,
            cancellation.clone(),
        )
        .await?;
    }
    if !identity.snapshot.verify_unchanged()? {
        return Err(SenderError::SourceChanged);
    }
    if cancellation.is_cancelled() {
        return Err(SenderError::Cancelled);
    }

    let verifying_started = Instant::now();
    emit(
        &request,
        TransferState::Verifying,
        identity.snapshot.size,
        0,
        0,
        0,
        0,
        vec![TransferSegment {
            start: 0,
            end: identity.snapshot.size,
            downloaded_end: identity.snapshot.size,
            running: false,
        }],
        0.0,
        diagnostics.clone(),
        started,
        None,
        None,
    );
    let final_frame = Final3 {
        transfer_id: request.transfer_id.clone(),
        content_hash: identity.content_id.clone(),
    };
    protocol::write_control(&mut control, FrameKind::Final, &final_frame).await?;
    let done: Done3 = tokio::select! {
        _ = cancellation.cancelled() => return Err(SenderError::Cancelled),
        result = timeout(
            final_verification_timeout(identity.snapshot.size),
            protocol::read_control(&mut control, FrameKind::Done),
        ) => result.map_err(|_| SenderError::AckTimeout)??,
    };
    if !done.verified
        || done.transfer_id != request.transfer_id
        || done.total_bytes != identity.snapshot.size
        || !is_safe_final_filename(&done.final_filename)
    {
        return Err(SenderError::FinalVerificationFailed);
    }
    diagnostics.verifying_ms = verifying_started.elapsed().as_millis() as u64;
    emit(
        &request,
        TransferState::Completed,
        identity.snapshot.size,
        0,
        0,
        0,
        0,
        vec![TransferSegment {
            start: 0,
            end: identity.snapshot.size,
            downloaded_end: identity.snapshot.size,
            running: false,
        }],
        0.0,
        diagnostics.clone(),
        started,
        None,
        Some("发送完成".into()),
    );
    info!(
        target: "locsd::transfer",
        "transfer_id={} event=completed hashing_ms={} negotiating_ms={} transferring_ms={} verifying_ms={} wire_bytes={} retransmitted_bytes={} split_trials={} rollbacks={} retries={} timeouts_or_resets={}",
        request.transfer_id,
        diagnostics.hashing_ms,
        diagnostics.negotiating_ms,
        diagnostics.transferring_ms,
        diagnostics.verifying_ms,
        diagnostics.wire_bytes,
        diagnostics.retransmitted_bytes,
        diagnostics.split_trials,
        diagnostics.rollbacks,
        diagnostics.retries,
        diagnostics.timeouts_or_resets,
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
    data_payload_bytes: usize,
    supports_range_status: bool,
    mut diagnostics: TransferDiagnostics,
    started: Instant,
    cancellation: CancellationToken,
) -> Result<TransferDiagnostics, SenderError> {
    let transfer_started = Instant::now();
    let prior_transferring_ms = diagnostics.transferring_ms;
    if missing.is_empty() {
        diagnostics.transferring_ms = prior_transferring_ms;
        return Ok(diagnostics);
    }
    let scheduler = Arc::new(Mutex::new(AdaptiveScheduler::new(
        identity.snapshot.size,
        missing,
        receiver_limit,
    )));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let wire_bytes = Arc::new(AtomicU64::new(0));
    let retransmitted_bytes = Arc::new(AtomicU64::new(0));
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
            data_payload_bytes,
            supports_range_status,
            wire_bytes.clone(),
            retransmitted_bytes.clone(),
        );
        next_worker_id += 1;
    }

    let mut metrics = MetricsCollector::new();
    let mut scheduler_tick = interval(SCHEDULER_TICK);
    let mut metrics_tick = interval(DIAGNOSTIC_WINDOW);
    scheduler_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    metrics_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    scheduler_tick.tick().await;
    metrics_tick.tick().await;
    let mut fatal: Option<SenderError> = None;
    let mut displayed_speed = 0.0;
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
                        diagnostics.retries = diagnostics.retries.saturating_add(u32::from(retrying));
                        diagnostics.timeouts_or_resets = diagnostics
                            .timeouts_or_resets
                            .saturating_add(u32::from(timeout_or_reset));
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
                let mut locked = scheduler.lock().await;
                let total_durable = locked.durable_bytes().max(durable_at_start);
                let in_flight = locked.in_flight_bytes();
                diagnostics.transferring_ms = prior_transferring_ms
                    .saturating_add(transfer_started.elapsed().as_millis() as u64);
                let mut event_diagnostics = diagnostics.clone();
                event_diagnostics.wire_bytes = event_diagnostics
                    .wire_bytes
                    .saturating_add(wire_bytes.load(Ordering::Relaxed));
                event_diagnostics.retransmitted_bytes = event_diagnostics
                    .retransmitted_bytes
                    .saturating_add(retransmitted_bytes.load(Ordering::Relaxed));
                emit(
                    request, TransferState::Transferring, total_durable, in_flight,
                    locked.active_len(), controls.len(), locked.target(), locked.segment_snapshot(), displayed_speed,
                    event_diagnostics, started, None, None,
                );
                if locked.is_complete() { break; }
                let decision = locked.on_fast_tick();
                drop(locked);
                if let Some(error) = fatal.take() {
                    cancellation.cancel();
                    stop_workers(&mut controls, &mut event_rx).await;
                    return Err(error);
                }
                if let Some(range_id) = decision.pause_for_split {
                    let split_control = scheduler.lock().await.worker_for_range(range_id)
                        .and_then(|worker_id| controls.get(&worker_id));
                    if let Some(control) = split_control
                        && control.send(WorkerControl::Pause(range_id)).is_ok()
                    {
                        diagnostics.split_trials = diagnostics.split_trials.saturating_add(1);
                    }
                }
            }
            _ = metrics_tick.tick() => {
                let wire_delta = wire_bytes.swap(0, Ordering::Relaxed);
                let retransmitted_delta = retransmitted_bytes.swap(0, Ordering::Relaxed);
                diagnostics.wire_bytes = diagnostics.wire_bytes.saturating_add(wire_delta);
                diagnostics.retransmitted_bytes = diagnostics
                    .retransmitted_bytes
                    .saturating_add(retransmitted_delta);
                diagnostics.transferring_ms = prior_transferring_ms
                    .saturating_add(transfer_started.elapsed().as_millis() as u64);
                metrics.record_wire(wire_delta);
                metrics.record_retransmitted(retransmitted_delta);
                let (active, pending) = {
                    let locked = scheduler.lock().await;
                    (locked.active_len(), locked.pending_len())
                };
                let window = metrics.roll(active, pending);
                displayed_speed = window.wire_ewma_bytes_per_second;
                let decision = scheduler.lock().await.on_diagnostic_window(&window);
                info!(
                    target: "locsd::transfer",
                    "transfer_id={} event=window ack_bytes={} wire_bytes={} retransmitted_bytes={} durable_bps={:.0} wire_bps={:.0} useful_wire_bps={:.0} wire_ewma_bps={:.0} useful_wire_ewma_bps={:.0} errors={} resets={} active={} pending={} target={}",
                    request.transfer_id,
                    window.ack_bytes,
                    window.wire_bytes,
                    window.retransmitted_bytes,
                    window.durable_bytes_per_second,
                    window.wire_bytes_per_second,
                    window.useful_wire_bytes_per_second,
                    window.wire_ewma_bytes_per_second,
                    window.useful_wire_ewma_bytes_per_second,
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
                        data_payload_bytes, supports_range_status,
                        wire_bytes.clone(), retransmitted_bytes.clone(),
                    );
                    next_worker_id += 1;
                }
                if decision.retire_workers > 0 {
                    let mut candidates: Vec<_> = controls
                        .keys()
                        .filter(|worker_id| !retiring.contains(*worker_id))
                        .copied()
                        .collect();
                    candidates.sort_unstable_by(|left, right| right.cmp(left));
                    candidates.truncate(decision.retire_workers);
                    for worker_id in candidates {
                        if let Some(control) = controls.get(&worker_id) {
                            let _ = control.send(WorkerControl::Retire);
                            retiring.insert(worker_id);
                        }
                    }
                }
                if !decision.rollback_children.is_empty() {
                    diagnostics.rollbacks = diagnostics.rollbacks.saturating_add(1);
                    let pauses = scheduler
                        .lock()
                        .await
                        .start_rollback(&decision.rollback_children);
                    for (worker_id, range_id) in pauses {
                        let sent = controls
                            .get(&worker_id)
                            .is_some_and(|control| {
                                control.send(WorkerControl::Pause(range_id)).is_ok()
                            });
                        if !sent {
                            let retrying = scheduler.lock().await.fail(range_id);
                            if !retrying {
                                fatal = Some(SenderError::WorkerUnavailable);
                            }
                        }
                    }
                }
                let (durable, in_flight, current_active, target, segments) = {
                    let locked = scheduler.lock().await;
                    (
                        locked.durable_bytes().max(durable_at_start),
                        locked.in_flight_bytes(),
                        locked.active_len(),
                        locked.target(),
                        locked.segment_snapshot(),
                    )
                };
                emit(
                    request, TransferState::Transferring, durable, in_flight, current_active, controls.len(), target,
                    segments, displayed_speed, diagnostics.clone(), started, None, None,
                );
            }
        }
    }
    stop_workers(&mut controls, &mut event_rx).await;
    diagnostics.wire_bytes = diagnostics
        .wire_bytes
        .saturating_add(wire_bytes.swap(0, Ordering::Relaxed));
    diagnostics.retransmitted_bytes = diagnostics
        .retransmitted_bytes
        .saturating_add(retransmitted_bytes.swap(0, Ordering::Relaxed));
    diagnostics.transferring_ms =
        prior_transferring_ms.saturating_add(transfer_started.elapsed().as_millis() as u64);
    Ok(diagnostics)
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
    snapshot: SendSourceSnapshot,
    scheduler: Arc<Mutex<AdaptiveScheduler>>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
    controls: &mut HashMap<usize, mpsc::UnboundedSender<WorkerControl>>,
    cancellation: CancellationToken,
    data_payload_bytes: usize,
    supports_range_status: bool,
    wire_bytes: Arc<AtomicU64>,
    retransmitted_bytes: Arc<AtomicU64>,
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
            data_payload_bytes,
            supports_range_status,
            wire_bytes,
            retransmitted_bytes,
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
    snapshot: SendSourceSnapshot,
    scheduler: Arc<Mutex<AdaptiveScheduler>>,
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
    mut controls: mpsc::UnboundedReceiver<WorkerControl>,
    cancellation: CancellationToken,
    data_payload_bytes: usize,
    supports_range_status: bool,
    wire_bytes: Arc<AtomicU64>,
    retransmitted_bytes: Arc<AtomicU64>,
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
                SendRangeContext {
                    scheduler: &scheduler,
                    controls: &mut controls,
                    cancellation: &cancellation,
                    retire_requested: &mut retire,
                    data_payload_bytes,
                    supports_range_status,
                    wire_bytes: &wire_bytes,
                    retransmitted_bytes: &retransmitted_bytes,
                },
            ) => result,
        };
        match result {
            Ok(result) => {
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
            protocol::CAP_RANGE_STATUS.into(),
            protocol::CAP_DEFERRED_HASH.into(),
        ],
        receiver_max_concurrency: 0,
        session_token: Some(session_token.into()),
    };
    protocol::write_control(&mut stream, FrameKind::Hello, &hello).await?;
    match timeout(HANDSHAKE_TIMEOUT, read_hello_response(&mut stream)).await {
        Ok(Ok(HelloResponse::Hello(hello)))
            if hello.role == ConnectionRole::Data && supports_required_capabilities(&hello) =>
        {
            Ok(stream)
        }
        Ok(Ok(HelloResponse::Hello(_))) => Err(SenderError::Unsupported),
        Ok(Ok(HelloResponse::Unsupported)) => Err(SenderError::Unsupported),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(SenderError::HandshakeTimeout),
    }
}

async fn send_range(
    stream: &mut TcpStream,
    snapshot: &SendSourceSnapshot,
    task: &RangeTask,
    context: SendRangeContext<'_>,
) -> Result<RangeResult, SenderError> {
    if !snapshot.verify_unchanged()? {
        return Err(SenderError::SourceChanged);
    }
    let started = Instant::now();
    if context.supports_range_status
        && task.attempts > 0
        && let Some(ack) = query_committed_prefix(stream, snapshot, task).await?
    {
        return Ok(RangeResult {
            ack,
            elapsed: started.elapsed(),
        });
    }
    let range = Range3 {
        range_id: task.range_id,
        start: task.interval.start,
        end: task.interval.end,
        generation: task.generation,
    };
    protocol::write_control(stream, FrameKind::Range, &range).await?;
    let reader = snapshot.open_reader()?;
    let mut offset = task.interval.start;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; context.data_payload_bytes];
    while offset < task.interval.end {
        if context.cancellation.is_cancelled() {
            return Err(SenderError::Cancelled);
        }
        while let Ok(command) = context.controls.try_recv() {
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
                    });
                }
                WorkerControl::Retire => *context.retire_requested = true,
                _ => {}
            }
        }
        let count = ((task.interval.end - offset) as usize).min(buffer.len());
        let read = reader.read_at(&mut buffer[..count], offset)?;
        if read == 0 {
            return Err(SenderError::UnexpectedSourceEof);
        }
        protocol::write_data(stream, offset, &buffer[..read]).await?;
        context.wire_bytes.fetch_add(read as u64, Ordering::Relaxed);
        if task.attempts > 0 {
            context
                .retransmitted_bytes
                .fetch_add(read as u64, Ordering::Relaxed);
        }
        hasher.update(&buffer[..read]);
        offset += read as u64;
        context
            .scheduler
            .lock()
            .await
            .record_in_flight(task.range_id, offset - task.interval.start);
    }
    let ack = read_ack(stream, task, offset, &hasher).await?;
    Ok(RangeResult {
        ack,
        elapsed: started.elapsed(),
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

async fn query_committed_prefix(
    stream: &mut TcpStream,
    snapshot: &SendSourceSnapshot,
    task: &RangeTask,
) -> Result<Option<Ack3>, SenderError> {
    let query = RangeStatus3 {
        range_id: task.range_id,
        start: task.interval.start,
        end: task.interval.end,
        generation: task.generation,
    };
    let deadline = Instant::now() + range_status_timeout(task.interval);
    let result = loop {
        protocol::write_control(stream, FrameKind::RangeStatus, &query).await?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SenderError::AckTimeout);
        }
        let result: RangeStatusResult3 = timeout(
            remaining,
            protocol::read_control(stream, FrameKind::RangeStatusResult),
        )
        .await
        .map_err(|_| SenderError::AckTimeout)??;
        if result.range_id != task.range_id
            || result.committed_end < task.interval.start
            || result.committed_end > task.interval.end
        {
            return Err(SenderError::InvalidAck);
        }
        if !result.busy {
            break result;
        }
        if result.committed_end != task.interval.start || !result.range_hash.is_empty() {
            return Err(SenderError::InvalidAck);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SenderError::AckTimeout);
        }
        tokio::time::sleep(RANGE_STATUS_BUSY_RETRY.min(remaining)).await;
    };
    if result.committed_end == task.interval.start {
        if !result.range_hash.is_empty() {
            return Err(SenderError::InvalidAck);
        }
        return Ok(None);
    }

    let committed = Interval {
        start: task.interval.start,
        end: result.committed_end,
    };
    let expected_hash = hash_source_range(snapshot, committed).await?;
    if result.range_hash != expected_hash {
        invalidate_interval(stream, task, committed).await?;
        return Err(SenderError::RangeHashMismatch);
    }
    Ok(Some(Ack3 {
        range_id: task.range_id,
        committed_end: result.committed_end,
        range_hash: result.range_hash,
        status: AckStatus::Duplicate,
        message: Some("reconciled durable prefix after an ambiguous retry".into()),
    }))
}

fn range_status_timeout(interval: Interval) -> Duration {
    let hashing_seconds = interval
        .len()
        .div_ceil(RANGE_STATUS_HASH_BYTES_PER_SECOND)
        .max(1);
    ACK_TIMEOUT
        .saturating_add(Duration::from_secs(hashing_seconds))
        .min(RANGE_STATUS_MAX_TIMEOUT)
}

fn final_verification_timeout(total_bytes: u64) -> Duration {
    let hashing_seconds = total_bytes.div_ceil(FINAL_HASH_BYTES_PER_SECOND).max(1);
    ACK_TIMEOUT
        .saturating_add(Duration::from_secs(hashing_seconds))
        .min(FINAL_VERIFICATION_MAX_TIMEOUT)
}

async fn hash_source_range(
    snapshot: &SendSourceSnapshot,
    interval: Interval,
) -> Result<String, SenderError> {
    let reader = snapshot.open_reader()?;
    let mut offset = interval.start;
    let mut remaining = interval.len();
    let mut buffer = vec![0u8; DATA_PAYLOAD_BYTES];
    let mut hasher = Hasher::new();
    while remaining > 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        let read = reader.read_at(&mut buffer[..count], offset)?;
        if read == 0 {
            return Err(SenderError::UnexpectedSourceEof);
        }
        hasher.update(&buffer[..read]);
        offset += read as u64;
        remaining -= read as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn invalidate_range(stream: &mut TcpStream, task: &RangeTask) -> Result<(), SenderError> {
    invalidate_interval(stream, task, task.interval).await
}

async fn invalidate_interval(
    stream: &mut TcpStream,
    task: &RangeTask,
    interval: Interval,
) -> Result<(), SenderError> {
    let invalidate = Invalidate3 {
        range_id: task.range_id,
        start: interval.start,
        end: interval.end,
        generation: task.generation,
        reason: "range hash mismatch".into(),
    };
    protocol::write_control(stream, FrameKind::Invalidate, &invalidate).await?;
    let ack: Ack3 = timeout(ACK_TIMEOUT, protocol::read_control(stream, FrameKind::Ack))
        .await
        .map_err(|_| SenderError::AckTimeout)??;
    if ack.range_id != task.range_id
        || ack.committed_end != interval.start
        || ack.status != AckStatus::Committed
    {
        return Err(SenderError::RangeInvalidationFailed);
    }
    Ok(())
}

fn has_capability(hello: &Hello3, capability: &str) -> bool {
    hello.capabilities.iter().any(|value| value == capability)
}

fn supports_required_capabilities(hello: &Hello3) -> bool {
    hello.protocol_version == protocol::PROTOCOL_VERSION
        && protocol::REQUIRED_TRANSFER_CAPABILITIES
            .iter()
            .all(|capability| has_capability(hello, capability))
}

struct ValidatedResumePlan {
    completed: IntervalSet,
    segments: Vec<CompletedSegment3>,
}

fn validate_plan(plan: &Plan3, total_bytes: u64) -> Result<ValidatedResumePlan, SenderError> {
    let mut set = IntervalSet::new();
    for interval in &plan.completed_intervals {
        if interval.start >= interval.end || interval.end > total_bytes || set.overlaps(*interval) {
            return Err(SenderError::InvalidPlan);
        }
        set.insert(*interval);
    }
    let mut segment_set = IntervalSet::new();
    for segment in &plan.completed_segments {
        let interval = Interval {
            start: segment.start,
            end: segment.end,
        };
        if interval.start >= interval.end
            || interval.end > total_bytes
            || segment_set.overlaps(interval)
            || !is_blake3_hex(&segment.range_hash)
        {
            return Err(SenderError::InvalidPlan);
        }
        segment_set.insert(interval);
    }
    if segment_set != set {
        return Err(SenderError::InvalidPlan);
    }
    Ok(ValidatedResumePlan {
        completed: set,
        segments: plan.completed_segments.clone(),
    })
}

async fn validate_resumed_segments(
    request: &SendRequest,
    snapshot: &SendSourceSnapshot,
    session_token: &str,
    segments: Vec<CompletedSegment3>,
    cancellation: CancellationToken,
) -> Result<Vec<Interval>, SenderError> {
    let mut stream = None;
    let mut invalidated = Vec::new();
    for (index, segment) in segments.into_iter().enumerate() {
        if cancellation.is_cancelled() {
            return Err(SenderError::Cancelled);
        }
        let interval = Interval {
            start: segment.start,
            end: segment.end,
        };
        let actual = hash_source_range(snapshot, interval).await?;
        if actual == segment.range_hash {
            continue;
        }
        if stream.is_none() {
            stream = Some(connect_data(&request.target_ip, request.port, session_token).await?);
        }
        let task = RangeTask {
            range_id: u64::MAX.saturating_sub(index as u64),
            interval,
            generation: 0,
            attempts: 0,
            root_range_id: u64::MAX.saturating_sub(index as u64),
            created_at: Instant::now(),
        };
        invalidate_interval(stream.as_mut().unwrap(), &task, interval).await?;
        invalidated.push(interval);
        info!(
            target: "locsd::transfer",
            "transfer_id={} event=resume_segment_invalidated start={} end={}",
            request.transfer_id,
            interval.start,
            interval.end,
        );
    }
    Ok(invalidated)
}

fn is_blake3_hex(value: &str) -> bool {
    value.len() == blake3::OUT_LEN * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn is_safe_final_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename != "."
        && filename != ".."
        && !filename.contains(['/', '\\', '\0'])
}

enum HelloResponse {
    Hello(Hello3),
    Unsupported,
}

async fn read_hello_response(stream: &mut TcpStream) -> Result<HelloResponse, SenderError> {
    let (kind, payload) = protocol::read_payload(stream).await?;
    match kind {
        FrameKind::Hello => {
            let hello: Hello3 = serde_json::from_slice(&payload)?;
            Ok(HelloResponse::Hello(hello))
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
    allocated: usize,
    target: usize,
    segments: Vec<TransferSegment>,
    speed: f64,
    diagnostics: TransferDiagnostics,
    started: Instant,
    error_code: Option<String>,
    message: Option<String>,
) {
    let file_name = request.file_name.clone();
    let total = request.total_bytes;
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
    event.allocated_workers = allocated as u16;
    event.target_workers = target as u16;
    event.segments = segments;
    event.diagnostics = diagnostics;
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

struct SendRangeContext<'a> {
    scheduler: &'a Arc<Mutex<AdaptiveScheduler>>,
    controls: &'a mut mpsc::UnboundedReceiver<WorkerControl>,
    cancellation: &'a CancellationToken,
    retire_requested: &'a mut bool,
    data_payload_bytes: usize,
    supports_range_status: bool,
    wire_bytes: &'a AtomicU64,
    retransmitted_bytes: &'a AtomicU64,
}

struct RangeResult {
    ack: Ack3,
    elapsed: Duration,
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
    WorkerUnavailable,
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
            Self::WorkerUnavailable => "worker_unavailable",
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
            source: SendSource::from_path(source.clone()),
            file_name: "source.bin".into(),
            total_bytes: source.metadata().unwrap().len(),
            identity_cache: Arc::new(StdMutex::new(None)),
            source_key: Some("fallback-source".into()),
            shared_identity_cache: Arc::new(StdMutex::new(HashMap::new())),
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

    #[test]
    fn required_capability_probe_rejects_partial_v3_peers() {
        let mut hello = Hello3 {
            protocol_version: protocol::PROTOCOL_VERSION,
            role: ConnectionRole::Control,
            capabilities: protocol::REQUIRED_TRANSFER_CAPABILITIES
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            receiver_max_concurrency: 8,
            session_token: None,
        };
        assert!(supports_required_capabilities(&hello));
        hello
            .capabilities
            .retain(|value| value != protocol::CAP_PAUSE);
        assert!(!supports_required_capabilities(&hello));
    }

    #[tokio::test]
    async fn ambiguous_retry_reconciles_a_durable_prefix_before_resending() {
        let root = std::env::temp_dir().join(format!(
            "locsd-range-status-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bin");
        let content: Vec<u8> = (0..=255).cycle().take(1024).collect();
        std::fs::write(&source, &content).unwrap();
        let snapshot = SendSourceSnapshot::capture(SendSource::from_path(&source)).unwrap();
        let expected_hash = blake3::hash(&content[100..600]).to_hex().to_string();

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let first: RangeStatus3 = protocol::read_control(&mut stream, FrameKind::RangeStatus)
                .await
                .unwrap();
            assert_eq!((first.start, first.end), (100, 900));
            let busy = RangeStatusResult3 {
                range_id: first.range_id,
                committed_end: first.start,
                range_hash: String::new(),
                busy: true,
            };
            protocol::write_control(&mut stream, FrameKind::RangeStatusResult, &busy)
                .await
                .unwrap();
            let query: RangeStatus3 = protocol::read_control(&mut stream, FrameKind::RangeStatus)
                .await
                .unwrap();
            let result = RangeStatusResult3 {
                range_id: query.range_id,
                committed_end: 600,
                range_hash: expected_hash,
                busy: false,
            };
            protocol::write_control(&mut stream, FrameKind::RangeStatusResult, &result)
                .await
                .unwrap();
        });

        let task = RangeTask {
            range_id: 42,
            interval: Interval {
                start: 100,
                end: 900,
            },
            generation: 1,
            attempts: 1,
            root_range_id: 42,
            created_at: Instant::now(),
        };
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let ack = query_committed_prefix(&mut stream, &snapshot, &task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ack.status, AckStatus::Duplicate);
        assert_eq!(ack.committed_end, 600);
        server.await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn range_status_timeout_scales_for_large_durable_prefixes() {
        let small = range_status_timeout(Interval { start: 0, end: 1 });
        let large = range_status_timeout(Interval {
            start: 0,
            end: 8 * 1024 * 1024 * 1024,
        });
        assert!(small > ACK_TIMEOUT);
        assert!(large >= Duration::from_secs(30 * 60));
        assert!(large <= RANGE_STATUS_MAX_TIMEOUT);
    }

    #[test]
    fn final_verification_timeout_scales_with_file_size() {
        let small = final_verification_timeout(1);
        let large = final_verification_timeout(8 * 1024 * 1024 * 1024);
        assert!(small > ACK_TIMEOUT);
        assert!(large >= Duration::from_secs(4 * 60));
        assert!(large <= FINAL_VERIFICATION_MAX_TIMEOUT);
    }

    #[test]
    fn plans_reject_reversed_intervals_before_length_arithmetic() {
        let plan = Plan3 {
            accepted: true,
            session_token: "token".into(),
            completed_intervals: vec![Interval {
                start: 100,
                end: 50,
            }],
            completed_segments: Vec::new(),
            receiver_max_concurrency: 8,
            max_frame_bytes: protocol::MAX_DATA_FRAME as u32,
            reason: None,
        };
        assert!(matches!(
            validate_plan(&plan, 200),
            Err(SenderError::InvalidPlan)
        ));
    }

    #[test]
    fn plans_require_a_hash_for_every_resumed_segment() {
        let interval = Interval { start: 0, end: 100 };
        let mut plan = Plan3 {
            accepted: true,
            session_token: "token".into(),
            completed_intervals: vec![interval],
            completed_segments: Vec::new(),
            receiver_max_concurrency: 8,
            max_frame_bytes: protocol::MAX_DATA_FRAME as u32,
            reason: None,
        };
        assert!(matches!(
            validate_plan(&plan, 100),
            Err(SenderError::InvalidPlan)
        ));
        plan.completed_segments.push(CompletedSegment3 {
            start: interval.start,
            end: interval.end,
            range_hash: blake3::hash(b"resume").to_hex().to_string(),
        });
        assert!(validate_plan(&plan, 100).is_ok());
    }

    #[test]
    fn final_filename_validation_is_platform_independent() {
        assert!(is_safe_final_filename("file (1).txt"));
        assert!(!is_safe_final_filename("../file.txt"));
        assert!(!is_safe_final_filename("..\\file.txt"));
        assert!(!is_safe_final_filename(""));
    }
}
