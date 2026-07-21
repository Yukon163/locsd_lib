use super::events::{
    EventSubscriber, OfferDisposition, OfferId, TransferCallback, TransferDirection, TransferEvent,
    TransferId, TransferState,
};
use super::protocol::FRAME_MAGIC;
use super::receiver::{OfferRegistry, ReceiverContext};
use super::sender::{self, SendRequest, SenderError, V3SendOutcome};
use log::{error, info, warn};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

static ENGINE: OnceLock<TransferEngine> = OnceLock::new();
static NEXT_TRANSFER: AtomicU64 = AtomicU64::new(1);

struct TransferEngine {
    runtime: Runtime,
    tasks: Arc<Mutex<HashMap<TransferId, TaskEntry>>>,
    server: Mutex<Option<CancellationToken>>,
    subscribers: Arc<RwLock<Vec<EventSubscriber>>>,
    offers: Arc<OfferRegistry>,
    device_identity: RwLock<DeviceIdentity>,
}

#[derive(Clone)]
struct TaskEntry {
    request: StoredSendRequest,
    cancellation: CancellationToken,
    finished: watch::Receiver<bool>,
    retry_pending: Arc<AtomicBool>,
}

#[derive(Clone)]
struct DeviceIdentity {
    id: String,
    name: String,
}

#[derive(Clone)]
struct StoredSendRequest {
    transfer_id: TransferId,
    target_ip: String,
    port: u16,
    file_path: PathBuf,
    callback: Arc<dyn TransferCallback>,
}

fn engine() -> &'static TransferEngine {
    ENGINE.get_or_init(|| TransferEngine {
        runtime: Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("locsd-transfer")
            .enable_all()
            .build()
            .expect("failed to create transfer runtime"),
        tasks: Arc::new(Mutex::new(HashMap::new())),
        server: Mutex::new(None),
        subscribers: Arc::new(RwLock::new(Vec::new())),
        offers: Arc::new(OfferRegistry::default()),
        device_identity: RwLock::new(default_device_identity()),
    })
}

pub fn start_server(port: u16, save_dir: String, callback: Box<dyn TransferCallback>) {
    let engine = engine();
    let cancellation = CancellationToken::new();
    if let Some(previous) = engine.server.lock().unwrap().replace(cancellation.clone()) {
        previous.cancel();
    }
    let callback: Arc<dyn TransferCallback> = Arc::from(callback);
    let callback = Arc::new(FanoutCallback {
        inner: callback,
        subscribers: engine.subscribers.clone(),
    });
    let legacy_callback: Arc<dyn TransferCallback> = callback.clone();
    let receiver = Arc::new(ReceiverContext::new(
        PathBuf::from(&save_dir),
        callback,
        engine.offers.clone(),
    ));
    engine.runtime.spawn(async move {
        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => listener,
            Err(bind_error) => {
                error!("Core: cannot bind transfer port {port}: {bind_error}");
                return;
            }
        };
        info!("Core: V3/V2/legacy transfer server listening on 0.0.0.0:{port}");
        let progress = Arc::new(Mutex::new(0u64));
        let total = Arc::new(Mutex::new(0u64));
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue; };
                    let receiver = receiver.clone();
                    let legacy_callback = legacy_callback.clone();
                    let save_dir = Arc::new(save_dir.clone());
                    let progress = progress.clone();
                    let total = total.clone();
                    tokio::spawn(async move {
                        let mut prefix = [0u8; 4];
                        let detected = tokio::time::timeout(Duration::from_secs(3), async {
                            loop {
                                let count = stream.peek(&mut prefix).await?;
                                if count == 0 || count >= prefix.len() { return Ok::<usize, std::io::Error>(count); }
                                tokio::task::yield_now().await;
                            }
                        }).await;
                        match detected {
                            Ok(Ok(count)) if count == prefix.len() && prefix == FRAME_MAGIC => receiver.handle_connection(stream).await,
                            Ok(Ok(count)) if count > 0 => {
                                match stream.into_std() {
                                    Ok(socket) => {
                                        let _ = socket.set_nonblocking(false);
                                        let _ = tokio::task::spawn_blocking(move || {
                                            super::legacy::handle_connection(socket, save_dir, legacy_callback, progress, total);
                                        }).await;
                                    }
                                    Err(conversion_error) => warn!("legacy socket conversion failed: {conversion_error}"),
                                }
                            }
                            _ => warn!("connection did not provide a protocol header before timeout"),
                        }
                    });
                }
            }
        }
        info!("Core: transfer server stopped on port {port}");
    });
}

pub fn stop_server() {
    if let Some(token) = engine().server.lock().unwrap().take() {
        token.cancel();
    }
}

pub fn send_file(
    target_ip: String,
    port: u16,
    file_path: String,
    callback: Box<dyn TransferCallback>,
) -> TransferId {
    let transfer_id = new_transfer_id();
    let inner: Arc<dyn TransferCallback> = Arc::from(callback);
    let total = Path::new(&file_path)
        .metadata()
        .map(|value| value.len())
        .unwrap_or(0);
    let file_name = Path::new(&file_path)
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let callback: Arc<dyn TransferCallback> = Arc::new(TaskCallback {
        inner,
        subscribers: engine().subscribers.clone(),
        transfer_id: transfer_id.clone(),
        file_name,
        total,
        started: Instant::now(),
    });
    let stored = StoredSendRequest {
        transfer_id: transfer_id.clone(),
        target_ip,
        port,
        file_path: PathBuf::from(file_path),
        callback,
    };
    spawn_send(stored);
    transfer_id
}

fn spawn_send(stored: StoredSendRequest) {
    let cancellation = CancellationToken::new();
    let (finished_tx, finished_rx) = watch::channel(false);
    engine().tasks.lock().unwrap().insert(
        stored.transfer_id.clone(),
        TaskEntry {
            request: stored.clone(),
            cancellation: cancellation.clone(),
            finished: finished_rx,
            retry_pending: Arc::new(AtomicBool::new(false)),
        },
    );
    let identity = engine().device_identity.read().unwrap().clone();
    engine().runtime.spawn(async move {
        let request = SendRequest {
            transfer_id: stored.transfer_id.clone(),
            target_ip: stored.target_ip.clone(),
            port: stored.port,
            file_path: stored.file_path.clone(),
            sender_device_id: identity.id,
            sender_name: identity.name,
            callback: stored.callback.clone(),
        };
        match sender::send_v3(request, cancellation.clone()).await {
            Ok(V3SendOutcome::Completed(_)) => {}
            Ok(V3SendOutcome::CompatibilityRequired) => {
                info!("peer did not complete the V3 pre-offer handshake; entering V2 compatibility layer");
                if let Err(message) = run_compatibility_fallback(&stored).await {
                    stored.callback.on_complete(false, message);
                }
            }
            Err(SenderError::Cancelled) => {
                let mut event = task_event(&stored, TransferState::Cancelled);
                event.error_code = Some("cancelled".into());
                event.message = Some("传输已取消".into());
                stored.callback.on_event(event);
            }
            Err(send_error) => {
                let mut event = task_event(&stored, TransferState::Failed);
                event.error_code = Some(send_error.code().into());
                event.message = Some(send_error.to_string());
                stored.callback.on_event(event);
            }
        }
        finished_tx.send_replace(true);
    });
}

async fn run_compatibility_fallback(stored: &StoredSendRequest) -> Result<(), String> {
    let stored = stored.clone();
    tokio::task::spawn_blocking(move || {
        let metadata = stored
            .file_path
            .metadata()
            .map_err(|error| format!("读取文件信息失败: {error}"))?;
        let file_len = metadata.len();
        let file_name = stored
            .file_path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .ok_or_else(|| "无效文件名".to_string())?;
        let path = stored.file_path.to_string_lossy().into_owned();
        super::legacy::send_file(
            &stored.target_ip,
            stored.port,
            &path,
            &file_name,
            file_len,
            stored.callback.clone(),
        )
    })
    .await
    .map_err(|error| error.to_string())?
}

pub fn cancel_transfer(transfer_id: &str) -> bool {
    let entry = engine().tasks.lock().unwrap().get(transfer_id).cloned();
    if let Some(entry) = entry {
        entry.cancellation.cancel();
        true
    } else {
        false
    }
}

pub fn retry_transfer(transfer_id: &str) -> bool {
    let entry = engine().tasks.lock().unwrap().get(transfer_id).cloned();
    if let Some(entry) = entry {
        if entry
            .retry_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        entry.cancellation.cancel();
        engine().runtime.spawn(async move {
            let mut finished = entry.finished.clone();
            if !*finished.borrow() {
                let _ = finished.changed().await;
            }
            spawn_send(entry.request);
        });
        true
    } else {
        false
    }
}

pub fn subscribe_events(subscriber: EventSubscriber) {
    engine().subscribers.write().unwrap().push(subscriber);
}

pub fn accept_transfer(offer_id: &str, destination: String) -> bool {
    engine().offers.accept(offer_id, PathBuf::from(destination))
}

pub fn reject_transfer(offer_id: &str) -> bool {
    engine().offers.reject(offer_id)
}

pub fn configure_device_identity(device_id: String, device_name: String) -> bool {
    let device_id = device_id.trim();
    if device_id.is_empty() {
        return false;
    }
    let mut identity = engine().device_identity.write().unwrap();
    identity.id = device_id.to_string();
    if !device_name.trim().is_empty() {
        identity.name = device_name;
    }
    true
}

fn default_device_identity() -> DeviceIdentity {
    let machine = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".into());
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    let seed = format!(
        "{}|{}|{}|{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        machine,
        user
    );
    let digest = blake3::hash(seed.as_bytes()).to_hex();
    DeviceIdentity {
        id: format!("device-{}", &digest[..32]),
        name: if machine == "unknown-host" {
            "LocalSend".into()
        } else {
            machine
        },
    }
}

fn new_transfer_id() -> TransferId {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = NEXT_TRANSFER.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp:032x}-{sequence:016x}")
}

fn task_event(stored: &StoredSendRequest, state: TransferState) -> TransferEvent {
    let total = stored
        .file_path
        .metadata()
        .map(|value| value.len())
        .unwrap_or(0);
    let file_name = stored
        .file_path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    TransferEvent::new(
        stored.transfer_id.clone(),
        TransferDirection::Send,
        state,
        file_name,
        total,
        Duration::ZERO,
    )
}

struct FanoutCallback {
    inner: Arc<dyn TransferCallback>,
    subscribers: Arc<RwLock<Vec<EventSubscriber>>>,
}

impl TransferCallback for FanoutCallback {
    fn on_receive_request(&self, file_name: String, file_size: u64, sender_ip: String) -> bool {
        self.inner
            .on_receive_request(file_name, file_size, sender_ip)
    }
    fn on_progress(&self, transferred: u64, total: u64) {
        self.inner.on_progress(transferred, total);
    }
    fn on_offer(
        &self,
        offer_id: OfferId,
        file_name: String,
        file_size: u64,
        sender_ip: String,
    ) -> OfferDisposition {
        self.inner
            .on_offer(offer_id, file_name, file_size, sender_ip)
    }
    fn on_complete(&self, success: bool, msg: String) {
        self.inner.on_complete(success, msg);
    }
    fn on_event(&self, event: TransferEvent) {
        self.inner.on_event(event.clone());
        for subscriber in self.subscribers.read().unwrap().iter() {
            subscriber(event.clone());
        }
    }
}

struct TaskCallback {
    inner: Arc<dyn TransferCallback>,
    subscribers: Arc<RwLock<Vec<EventSubscriber>>>,
    transfer_id: TransferId,
    file_name: String,
    total: u64,
    started: Instant,
}

impl TaskCallback {
    fn fanout(&self, event: TransferEvent) {
        self.inner.on_event(event.clone());
        for subscriber in self.subscribers.read().unwrap().iter() {
            subscriber(event.clone());
        }
    }
}

impl TransferCallback for TaskCallback {
    fn on_receive_request(&self, file_name: String, file_size: u64, sender_ip: String) -> bool {
        self.inner
            .on_receive_request(file_name, file_size, sender_ip)
    }
    fn on_progress(&self, transferred: u64, total: u64) {
        let mut event = TransferEvent::new(
            self.transfer_id.clone(),
            TransferDirection::Send,
            TransferState::Transferring,
            self.file_name.clone(),
            total,
            self.started.elapsed(),
        );
        event.durable_bytes = transferred.min(total);
        self.fanout(event);
    }
    fn on_offer(
        &self,
        offer_id: OfferId,
        file_name: String,
        file_size: u64,
        sender_ip: String,
    ) -> OfferDisposition {
        self.inner
            .on_offer(offer_id, file_name, file_size, sender_ip)
    }
    fn on_complete(&self, success: bool, msg: String) {
        let state = if success {
            TransferState::Completed
        } else {
            TransferState::Failed
        };
        let mut event = TransferEvent::new(
            self.transfer_id.clone(),
            TransferDirection::Send,
            state,
            self.file_name.clone(),
            self.total,
            self.started.elapsed(),
        );
        event.durable_bytes = if success { self.total } else { 0 };
        event.message = Some(msg);
        self.fanout(event);
    }
    fn on_event(&self, event: TransferEvent) {
        self.fanout(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    struct TestCallback {
        completed: Mutex<mpsc::Sender<bool>>,
    }

    #[test]
    fn fallback_device_identity_is_stable_and_process_independent() {
        let first = default_device_identity();
        let second = default_device_identity();
        assert_eq!(first.id, second.id);
        assert!(first.id.starts_with("device-"));
        assert_eq!(first.id.len(), "device-".len() + 32);
    }

    #[test]
    fn new_sender_falls_back_to_an_old_v2_only_receiver() {
        let root = std::env::temp_dir().join(format!("locsd-v2-e2e-{}", new_transfer_id()));
        let source_dir = root.join("source");
        let destination = root.join("destination");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        let content = vec![0x5a; 100 * 1024 + 13];
        let source = source_dir.join("old-peer.bin");
        std::fs::write(&source, &content).unwrap();

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (receive_tx, receive_rx) = mpsc::channel();
        let receiver_callback: Arc<dyn TransferCallback> = Arc::new(TestCallback {
            completed: Mutex::new(receive_tx),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let save_dir = Arc::new(destination.to_string_lossy().into_owned());
        let server = std::thread::spawn(move || {
            let progress = Arc::new(Mutex::new(0u64));
            let total = Arc::new(Mutex::new(0u64));
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let save_dir = save_dir.clone();
                        let callback = receiver_callback.clone();
                        let progress = progress.clone();
                        let total = total.clone();
                        std::thread::spawn(move || {
                            crate::core::handle_incoming_connection(
                                socket, save_dir, callback, progress, total,
                            );
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("old receiver accept failed: {error}"),
                }
            }
        });

        let (send_tx, send_rx) = mpsc::channel();
        send_file(
            "127.0.0.1".into(),
            port,
            source.to_string_lossy().into_owned(),
            Box::new(TestCallback {
                completed: Mutex::new(send_tx),
            }),
        );
        assert!(send_rx.recv_timeout(Duration::from_secs(30)).unwrap());
        assert!(receive_rx.recv_timeout(Duration::from_secs(30)).unwrap());
        assert_eq!(
            std::fs::read(destination.join("old-peer.bin")).unwrap(),
            content
        );

        stop.store(true, Ordering::Release);
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retry_waits_for_the_previous_task_and_coalesces_duplicate_requests() {
        let root = std::env::temp_dir().join(format!("locsd-retry-{}", new_transfer_id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("retry.bin");
        std::fs::write(&source, b"retry").unwrap();
        let transfer_id = new_transfer_id();
        let (completed, completion) = mpsc::channel();
        let request = StoredSendRequest {
            transfer_id: transfer_id.clone(),
            target_ip: "127.0.0.1".into(),
            port: 1,
            file_path: source,
            callback: Arc::new(TestCallback {
                completed: Mutex::new(completed),
            }),
        };
        let cancellation = CancellationToken::new();
        let (finished_tx, finished_rx) = watch::channel(false);
        let original_retry_flag = Arc::new(AtomicBool::new(false));
        engine().tasks.lock().unwrap().insert(
            transfer_id.clone(),
            TaskEntry {
                request,
                cancellation,
                finished: finished_rx,
                retry_pending: original_retry_flag.clone(),
            },
        );

        assert!(retry_transfer(&transfer_id));
        assert!(!retry_transfer(&transfer_id));
        std::thread::sleep(Duration::from_millis(50));
        let before_finish = engine()
            .tasks
            .lock()
            .unwrap()
            .get(&transfer_id)
            .unwrap()
            .retry_pending
            .clone();
        assert!(Arc::ptr_eq(&before_finish, &original_retry_flag));

        finished_tx.send_replace(true);
        for _ in 0..100 {
            let current = engine()
                .tasks
                .lock()
                .unwrap()
                .get(&transfer_id)
                .unwrap()
                .retry_pending
                .clone();
            if !Arc::ptr_eq(&current, &original_retry_flag) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let after_finish = engine()
            .tasks
            .lock()
            .unwrap()
            .get(&transfer_id)
            .unwrap()
            .retry_pending
            .clone();
        assert!(!Arc::ptr_eq(&after_finish, &original_retry_flag));
        assert!(!completion.recv_timeout(Duration::from_secs(5)).unwrap());
        engine().tasks.lock().unwrap().remove(&transfer_id);
        std::fs::remove_dir_all(root).unwrap();
    }

    impl TransferCallback for TestCallback {
        fn on_receive_request(
            &self,
            _file_name: String,
            _file_size: u64,
            _sender_ip: String,
        ) -> bool {
            true
        }
        fn on_progress(&self, _transferred: u64, _total: u64) {}
        fn on_complete(&self, success: bool, _msg: String) {
            let _ = self.completed.lock().unwrap().send(success);
        }
    }

    #[test]
    fn loopback_v3_transfers_zero_small_and_13_mib_with_final_hash() {
        let root = std::env::temp_dir().join(format!("locsd-e2e-{}", new_transfer_id()));
        let source_dir = root.join("source");
        let destination = root.join("destination");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&destination).unwrap();

        let port = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.local_addr().unwrap().port()
        };
        let (receive_tx, receive_rx) = mpsc::channel();
        start_server(
            port,
            destination.to_string_lossy().into_owned(),
            Box::new(TestCallback {
                completed: Mutex::new(receive_tx),
            }),
        );
        std::thread::sleep(Duration::from_millis(150));

        let fixtures = [
            ("zero.bin", Vec::new()),
            ("small.bin", b"special|filename-safe-content".to_vec()),
            (
                "thirteen.bin",
                (0..(13 * 1024 * 1024 + 37))
                    .map(|index| (index % 251) as u8)
                    .collect(),
            ),
        ];
        for (name, content) in fixtures {
            let source = source_dir.join(name);
            std::fs::write(&source, &content).unwrap();
            let (send_tx, send_rx) = mpsc::channel();
            send_file(
                "127.0.0.1".into(),
                port,
                source.to_string_lossy().into_owned(),
                Box::new(TestCallback {
                    completed: Mutex::new(send_tx),
                }),
            );
            assert!(send_rx.recv_timeout(Duration::from_secs(30)).unwrap());
            assert!(receive_rx.recv_timeout(Duration::from_secs(30)).unwrap());
            assert_eq!(std::fs::read(destination.join(name)).unwrap(), content);
        }

        let concurrent = [
            ("parallel-a.bin", vec![0xA5; 2 * 1024 * 1024 + 3]),
            ("parallel-b.bin", vec![0x5A; 3 * 1024 * 1024 + 7]),
        ];
        let mut send_receivers = Vec::new();
        for (name, content) in &concurrent {
            let source = source_dir.join(name);
            std::fs::write(&source, content).unwrap();
            let (send_tx, send_rx) = mpsc::channel();
            send_file(
                "127.0.0.1".into(),
                port,
                source.to_string_lossy().into_owned(),
                Box::new(TestCallback {
                    completed: Mutex::new(send_tx),
                }),
            );
            send_receivers.push(send_rx);
        }
        for send_rx in send_receivers {
            assert!(send_rx.recv_timeout(Duration::from_secs(30)).unwrap());
        }
        assert!(receive_rx.recv_timeout(Duration::from_secs(30)).unwrap());
        assert!(receive_rx.recv_timeout(Duration::from_secs(30)).unwrap());
        for (name, content) in concurrent {
            assert_eq!(std::fs::read(destination.join(name)).unwrap(), content);
        }

        stop_server();
        let _ = std::fs::remove_dir_all(root);
    }
}
