use if_addrs::{IfAddr, get_if_addrs};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub use crate::transfer::events::TransferCallback;

const RESUME_CHUNK_SIZE: u64 = 1024 * 1024;
const RESUME_DIR_NAME: &str = ".localsend_resume";
static RESUME_META_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub device_id: String,
    pub name: String,
    pub ip: String,
    pub control_port: u16,
}

pub trait DiscoveryCallback: Send + Sync {
    fn on_device_found(&self, device_info: DeviceInfo);
}

fn caculate_broadcast(ip: Ipv4Addr, mask: Ipv4Addr) -> Ipv4Addr {
    let ip_u32 = u32::from(ip);
    let mask_u32 = u32::from(mask);
    let broadcast_u32 = ip_u32 | (!mask_u32);
    Ipv4Addr::from(broadcast_u32)
}

pub fn start_listening(
    port: u16,
    device_id: String,
    device_name: String,
    callback: Box<dyn DiscoveryCallback>,
) {
    let callback = Arc::new(callback);

    let self_id_check = device_id.clone();

    thread::spawn(move || {
        info!("Core: UDP 线程启动，正在监听 0.0.0.0:{}", port);

        let socket = match UdpSocket::bind(format!("0.0.0.0:{}", port)) {
            Ok(s) => s,
            Err(e) => {
                error!("Core: UDP 绑定失败: {:?}", e);
                return;
            }
        };

        if let Err(e) = socket.set_broadcast(true) {
            error!("Core: 设置广播失败: {:?}", e);
        }

        let mut buf = [0u8; 1024];

        loop {
            let (size, addr) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) => {
                    error!("Core: UDP 接收失败: {:?}", e);
                    continue;
                }
            };

            let msg = String::from_utf8_lossy(&buf[..size]);
            let parts: Vec<&str> = msg.split('|').collect();

            if parts.len() > 1 && parts[1] == self_id_check {
                continue;
            }

            if msg.starts_with("DISCOVER|") {
                let parts: Vec<&str> = msg.split('|').collect();
                if parts.len() == 4 {
                    let device = DeviceInfo {
                        device_id: parts[1].to_string(),
                        name: parts[2].to_string(),
                        ip: addr.ip().to_string(),
                        control_port: parts[3].parse().unwrap_or(4060),
                    };
                    callback.on_device_found(device);
                }

                let response = format!("HERE|{}|{}|{}", device_id, device_name, port);

                let target_port = if parts.len() == 4 {
                    parts[3].parse().unwrap_or(4060)
                } else {
                    4060
                };
                let target_addr = format!("{}:{}", addr.ip(), target_port);

                if let Err(e) = socket.send_to(response.as_bytes(), &target_addr) {
                    error!("Core: 回复 HERE 失败 (至 {}): {:?}", target_addr, e);
                }
            } else if msg.starts_with("HERE|") {
                let parts: Vec<&str> = msg.split('|').collect();
                if parts.len() == 4 {
                    let device = DeviceInfo {
                        device_id: parts[1].to_string(),
                        name: parts[2].to_string(),
                        ip: addr.ip().to_string(),
                        control_port: parts[3].parse().unwrap_or(4060),
                    };

                    callback.on_device_found(device);
                }
            }
        }
    });
}

pub fn start_discovery_broadcaster(port: u16, device_id: String, device_name: String) {
    thread::spawn(move || {
        let socket = UdpSocket::bind("0.0.0.0:0").expect("无法绑定发送套接字"); // 0就是随机端口，好强
        socket.set_broadcast(true).expect("无法设置广播权限");

        let msg = format!("DISCOVER|{}|{}|{}", device_id, device_name, port);

        loop {
            let target_ips = get_target_broadcats();

            for target_ip in target_ips {
                let broadcast_addr = format!("{}:{}", target_ip, port);

                if let Err(e) = socket.send_to(msg.as_bytes(), &broadcast_addr) {
                    error!("发现广播失败: {:?}", e);
                } else {
                    debug!("已向 {} 发送 DISCOVER 广播", target_ip);
                }
            }

            thread::sleep(Duration::from_secs(5));
        }
    });
}

pub fn send_discover_once(port: u16, device_id: String, device_name: String) {
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        socket.set_broadcast(true).ok();
        let targets = get_target_broadcats();
        for target_ip in targets {
            let msg = format!("DISCOVER|{}|{}|{}", device_id, device_name, port);
            let _ = socket.send_to(msg.as_bytes(), format!("{}:{}", target_ip, port));
        }
    }
}

fn get_target_broadcats() -> Vec<String> {
    let mut broadcasts = Vec::new();

    match get_if_addrs() {
        Ok(ifaces) => {
            for iface in ifaces {
                if iface.is_loopback() {
                    continue;
                }
                if let IfAddr::V4(v4_addr) = iface.addr {
                    let ip = v4_addr.ip;
                    let mask = v4_addr.netmask;
                    let broadcast = caculate_broadcast(ip, mask);

                    if !broadcast.is_unspecified() {
                        broadcasts.push(broadcast.to_string());
                    }
                }
            }
        }
        Err(e) => {
            error!("无法获取网络接口信息: {:?}", e);
        }
    }
    if broadcasts.is_empty() {
        warn!("未找到有效网卡，回退到全局广播 255.255.255.255");
        broadcasts.push("255.255.255.255".to_string());
    }

    broadcasts
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResumeMeta {
    file_id: String,
    filename: String,
    size: u64,
    chunk_size: u64,
    received: Vec<bool>,
}

impl ResumeMeta {
    fn new(file_id: String, filename: String, size: u64, chunk_size: u64) -> Self {
        Self {
            file_id,
            filename,
            size,
            chunk_size,
            received: vec![false; total_chunks(size, chunk_size)],
        }
    }

    fn received_bytes(&self) -> u64 {
        self.received
            .iter()
            .enumerate()
            .filter(|(_, done)| **done)
            .map(|(idx, _)| chunk_len(self.size, self.chunk_size, idx))
            .sum()
    }

    fn missing_chunks(&self) -> Vec<usize> {
        self.received
            .iter()
            .enumerate()
            .filter_map(|(idx, done)| if *done { None } else { Some(idx) })
            .collect()
    }

    fn is_complete(&self) -> bool {
        !self.received.is_empty() && self.received.iter().all(|done| *done)
    }
}

fn total_chunks(size: u64, chunk_size: u64) -> usize {
    if size == 0 {
        1
    } else {
        size.div_ceil(chunk_size) as usize
    }
}

fn chunk_len(size: u64, chunk_size: u64, chunk_index: usize) -> u64 {
    let offset = chunk_index as u64 * chunk_size;
    if offset >= size {
        0
    } else {
        (size - offset).min(chunk_size)
    }
}

fn safe_filename(filename: &str) -> String {
    Path::new(filename)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "received_file".to_string())
}

fn resume_dir(save_dir: &str) -> PathBuf {
    Path::new(save_dir).join(RESUME_DIR_NAME)
}

fn meta_path(save_dir: &str, file_id: &str) -> PathBuf {
    resume_dir(save_dir).join(format!("{}.json", file_id))
}

fn part_path(save_dir: &str, filename: &str) -> PathBuf {
    Path::new(save_dir).join(format!("{}.part", safe_filename(filename)))
}

fn final_path(save_dir: &str, filename: &str) -> PathBuf {
    Path::new(save_dir).join(safe_filename(filename))
}

fn available_final_path(save_dir: &str, filename: &str) -> (PathBuf, String) {
    let filename = safe_filename(filename);
    let direct = final_path(save_dir, &filename);
    if !direct.exists() {
        return (direct, filename);
    }

    let dot = filename.rfind('.');
    let (base, ext) = match dot {
        Some(idx) if idx > 0 && idx < filename.len() - 1 => (&filename[..idx], &filename[idx..]),
        _ => (filename.as_str(), ""),
    };

    for idx in 1..=9999 {
        let candidate = format!("{}({}){}", base, idx, ext);
        let path = final_path(save_dir, &candidate);
        if !path.exists() {
            return (path, candidate);
        }
    }

    let candidate = format!("{}({}){}", base, current_timestamp_ms(), ext);
    (final_path(save_dir, &candidate), candidate)
}

fn current_timestamp_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn load_resume_meta(save_dir: &str, file_id: &str) -> Option<ResumeMeta> {
    let path = meta_path(save_dir, file_id);
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_resume_meta(save_dir: &str, meta: &ResumeMeta) -> std::io::Result<()> {
    std::fs::create_dir_all(resume_dir(save_dir))?;
    let path = meta_path(save_dir, &meta.file_id);
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(meta)?;
    std::fs::write(&tmp, data)?;
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    std::fs::rename(tmp, path)
}

fn remove_resume_meta(save_dir: &str, file_id: &str) {
    let _ = std::fs::remove_file(meta_path(save_dir, file_id));
}

fn format_chunk_ranges(chunks: &[usize]) -> String {
    if chunks.is_empty() {
        return String::new();
    }

    let mut ranges = Vec::new();
    let mut start = chunks[0];
    let mut prev = chunks[0];

    for &chunk in &chunks[1..] {
        if chunk == prev + 1 {
            prev = chunk;
        } else {
            if start == prev {
                ranges.push(start.to_string());
            } else {
                ranges.push(format!("{}-{}", start, prev));
            }
            start = chunk;
            prev = chunk;
        }
    }

    if start == prev {
        ranges.push(start.to_string());
    } else {
        ranges.push(format!("{}-{}", start, prev));
    }

    ranges.join(",")
}

fn parse_chunk_ranges(input: &str) -> Vec<usize> {
    let mut chunks = Vec::new();
    for part in input.split(',').filter(|part| !part.is_empty()) {
        if let Some((start, end)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>())
                && start <= end
            {
                chunks.extend(start..=end);
            }
        } else if let Ok(idx) = part.parse::<usize>() {
            chunks.push(idx);
        }
    }
    chunks
}

fn make_file_id(filename: &str, size: u64) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for b in filename
        .as_bytes()
        .iter()
        .chain(size.to_string().as_bytes())
    {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

pub fn start_file_server(port: u16, save_dir: String, callback: Box<dyn TransferCallback>) {
    crate::transfer::engine::start_server(port, save_dir, callback);
}

pub(crate) fn handle_incoming_connection(
    mut socket: TcpStream,
    save_dir: Arc<String>,
    callback: Arc<dyn TransferCallback>,
    progress_counter: Arc<Mutex<u64>>,
    total_size_store: Arc<Mutex<u64>>,
) {
    let mut header_buf = Vec::new();
    let mut char_buf = [0u8; 1];
    loop {
        if let Ok(1) = socket.read(&mut char_buf) {
            if char_buf[0] == b'\n' {
                break;
            }
            header_buf.push(char_buf[0]);
        } else {
            return;
        }
    }

    let header_str = String::from_utf8_lossy(&header_buf);
    let parts: Vec<&str> = header_str.split('|').collect();

    if parts[0] == "REQ2" && parts.len() >= 5 {
        handle_resume_request(
            socket,
            &save_dir,
            callback,
            progress_counter,
            total_size_store,
            &parts,
        );
    } else if parts[0] == "DATA2" && parts.len() >= 5 {
        handle_resume_data(
            socket,
            &save_dir,
            callback,
            progress_counter,
            total_size_store,
            &parts,
        );
    } else if parts[0] == "REQ" && parts.len() >= 3 {
        let filename = parts[1];
        let size: u64 = parts[2].parse().unwrap_or(0);
        let sender_ip = socket
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_default();

        if callback.on_receive_request(filename.to_string(), size, sender_ip) {
            let path = Path::new(save_dir.as_str()).join(safe_filename(filename));
            if let Ok(file) = File::create(&path) {
                if let Err(e) = file.set_len(size) {
                    error!("无法预分配文件大小: {:?}", e);
                }
                if let Ok(mut t) = total_size_store.lock() {
                    *t = size;
                }
                if let Ok(mut p) = progress_counter.lock() {
                    *p = 0;
                }

                let _ = socket.write_all(b"ACC\n"); // Accept
            } else {
                let _ = socket.write_all(b"REJ|CreateFileErr\n");
            }
        } else {
            let _ = socket.write_all(b"REJ\n"); // Reject
        }
    } else if parts[0] == "DATA" && parts.len() >= 3 {
        let filename = parts[1];
        let offset: u64 = parts[2].parse().unwrap_or(0);

        let path = Path::new(save_dir.as_str()).join(safe_filename(filename));

        let mut file = match OpenOptions::new().write(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                error!("无法打开文件写入数据: {:?}", e);
                return;
            }
        };

        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            error!("Seek失败: {:?}", e);
            return;
        }

        let mut buffer = [0u8; 64 * 1024];
        let mut last_progress_update = 0u64;
        loop {
            match socket.read(&mut buffer) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if let Err(e) = file.write_all(&buffer[..n]) {
                        error!("写入文件失败: {:?}", e);
                        break;
                    }

                    let mut p = progress_counter.lock().unwrap();
                    *p += n as u64;
                    let current_total = *p;
                    drop(p);

                    let total = *total_size_store.lock().unwrap();

                    if current_total - last_progress_update > 1024 * 1024 || current_total == total
                    {
                        callback.on_progress(current_total, total);
                        last_progress_update = current_total;
                    }

                    if current_total >= total && total > 0 {
                        // 注意：这里可能会被多个线程触发，实际应该加状态判断
                        // 但为了简单，多调一次 on_complete 问题不大，Java端防抖即可
                        callback.on_complete(true, filename.to_string());
                    }
                }
                Err(_) => break,
            }
        }
    }
}

fn handle_resume_request(
    mut socket: TcpStream,
    save_dir: &str,
    callback: Arc<dyn TransferCallback>,
    progress_counter: Arc<Mutex<u64>>,
    total_size_store: Arc<Mutex<u64>>,
    parts: &[&str],
) {
    let file_id = parts[1].to_string();
    let filename = safe_filename(parts[2]);
    let size: u64 = parts[3].parse().unwrap_or(0);
    let chunk_size: u64 = parts[4].parse().unwrap_or(RESUME_CHUNK_SIZE).max(1);
    let sender_ip = socket
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();

    if !callback.on_receive_request(filename.clone(), size, sender_ip) {
        let _ = socket.write_all(b"REJ\n");
        return;
    }

    let _meta_guard = RESUME_META_LOCK.lock().unwrap();
    let mut meta = load_resume_meta(save_dir, &file_id)
        .filter(|meta| {
            meta.size == size && meta.chunk_size == chunk_size && meta.filename == filename
        })
        .unwrap_or_else(|| ResumeMeta::new(file_id.clone(), filename.clone(), size, chunk_size));

    let chunks = total_chunks(size, chunk_size);
    if meta.received.len() != chunks {
        meta.received.resize(chunks, false);
    }

    let part_file_path = part_path(save_dir, &filename);
    let part_file_valid = part_file_path
        .metadata()
        .map(|metadata| metadata.len() == size)
        .unwrap_or(false);
    if !part_file_valid && meta.received.iter().any(|done| *done) {
        meta = ResumeMeta::new(file_id.clone(), filename.clone(), size, chunk_size);
    }

    match OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&part_file_path)
    {
        Ok(file) => {
            if let Err(e) = file.set_len(size) {
                error!("无法预分配续传临时文件大小: {:?}", e);
                let _ = socket.write_all(b"REJ|CreatePartErr\n");
                return;
            }
        }
        Err(e) => {
            error!("无法创建续传临时文件: {:?}", e);
            let _ = socket.write_all(b"REJ|CreatePartErr\n");
            return;
        }
    }

    if let Err(e) = save_resume_meta(save_dir, &meta) {
        error!("无法保存续传元数据: {:?}", e);
        let _ = socket.write_all(b"REJ|MetaErr\n");
        return;
    }

    let mut received = meta.received_bytes();
    if let Ok(mut total) = total_size_store.lock() {
        *total = size;
    }
    if let Ok(mut progress) = progress_counter.lock() {
        *progress = received;
    }
    if received > 0 {
        callback.on_progress(received, size);
    }

    let mut missing = meta.missing_chunks();
    if missing.is_empty() {
        let part = part_path(save_dir, &filename);
        if part.metadata().map(|m| m.len() == size).unwrap_or(false) {
            let (final_file, final_name) = available_final_path(save_dir, &filename);
            match std::fs::rename(&part, &final_file) {
                Ok(_) => {
                    remove_resume_meta(save_dir, &file_id);
                    callback.on_progress(size, size);
                    callback.on_complete(true, final_name);
                    let _ = socket.write_all(b"DONE\n");
                    return;
                }
                Err(e) => {
                    error!("续传完成重命名失败: {:?}", e);
                }
            }
        }

        meta = ResumeMeta::new(file_id.clone(), filename.clone(), size, chunk_size);
        if let Err(e) = save_resume_meta(save_dir, &meta) {
            error!("无法重置续传元数据: {:?}", e);
            let _ = socket.write_all(b"REJ|MetaErr\n");
            return;
        }
        received = 0;
        missing = meta.missing_chunks();
    }

    let response = format!("ACC2|{}|{}\n", received, format_chunk_ranges(&missing));
    let _ = socket.write_all(response.as_bytes());
}

fn handle_resume_data(
    mut socket: TcpStream,
    save_dir: &str,
    callback: Arc<dyn TransferCallback>,
    progress_counter: Arc<Mutex<u64>>,
    total_size_store: Arc<Mutex<u64>>,
    parts: &[&str],
) {
    let file_id = parts[1];
    let chunk_index: usize = parts[2].parse().unwrap_or(0);
    let offset: u64 = parts[3].parse().unwrap_or(0);
    let expected_len: u64 = parts[4].parse().unwrap_or(0);

    let meta = match load_resume_meta(save_dir, file_id) {
        Some(meta) => meta,
        None => {
            error!("未找到续传元数据: {}", file_id);
            return;
        }
    };

    if chunk_index >= meta.received.len() {
        error!("续传块索引越界: {}", chunk_index);
        return;
    }

    if meta.received[chunk_index] {
        return;
    }

    let expected_offset = chunk_index as u64 * meta.chunk_size;
    let expected_chunk_len = chunk_len(meta.size, meta.chunk_size, chunk_index);
    if offset != expected_offset || expected_len != expected_chunk_len {
        error!(
            "续传块信息不匹配: idx={}, offset={} expected={}, len={} expected={}",
            chunk_index, offset, expected_offset, expected_len, expected_chunk_len
        );
        return;
    }

    let path = part_path(save_dir, &meta.filename);
    let mut file = match OpenOptions::new().write(true).open(&path) {
        Ok(file) => file,
        Err(e) => {
            error!("无法打开续传临时文件: {:?}", e);
            return;
        }
    };

    if let Err(e) = file.seek(SeekFrom::Start(offset)) {
        error!("续传 Seek 失败: {:?}", e);
        return;
    }

    let mut remaining = expected_len;
    let mut received_this_chunk = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    let mut last_progress_update = 0u64;

    while remaining > 0 {
        let read_len = (remaining as usize).min(buffer.len());
        match socket.read(&mut buffer[..read_len]) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = file.write_all(&buffer[..n]) {
                    error!("续传写入文件失败: {:?}", e);
                    return;
                }

                remaining -= n as u64;
                received_this_chunk += n as u64;

                let mut progress = progress_counter.lock().unwrap();
                *progress += n as u64;
                let current_total = *progress;
                drop(progress);

                let total = *total_size_store.lock().unwrap();
                if current_total.saturating_sub(last_progress_update) >= RESUME_CHUNK_SIZE
                    || current_total == total
                {
                    callback.on_progress(current_total, total);
                    last_progress_update = current_total;
                }
            }
            Err(e) => {
                error!("续传读取失败: {:?}", e);
                return;
            }
        }
    }

    if received_this_chunk != expected_len {
        warn!(
            "续传块未完整接收: idx={}, got={}, expected={}",
            chunk_index, received_this_chunk, expected_len
        );
        return;
    }

    let _meta_guard = RESUME_META_LOCK.lock().unwrap();
    let mut meta = match load_resume_meta(save_dir, file_id) {
        Some(meta) => meta,
        None => return,
    };

    if chunk_index >= meta.received.len() || meta.received[chunk_index] {
        return;
    }

    meta.received[chunk_index] = true;
    if let Err(e) = save_resume_meta(save_dir, &meta) {
        error!("续传元数据更新失败: {:?}", e);
        return;
    }

    if meta.is_complete() {
        let part = part_path(save_dir, &meta.filename);
        let (final_file, final_name) = available_final_path(save_dir, &meta.filename);
        match std::fs::rename(&part, &final_file) {
            Ok(_) => {
                remove_resume_meta(save_dir, &meta.file_id);
                callback.on_progress(meta.size, meta.size);
                callback.on_complete(true, final_name);
            }
            Err(e) => {
                error!("续传完成重命名失败: {:?}", e);
                callback.on_complete(false, format!("保存文件失败: {:?}", e));
            }
        }
    }
}

pub fn send_file(
    target_ip: String,
    port: u16,
    file_path: String,
    parallel_cnt: u64,                   // 并行线程数，建议 4-8
    callback: Box<dyn TransferCallback>, // 用于回传发送进度
) -> crate::transfer::TransferId {
    let _ = parallel_cnt;
    crate::transfer::engine::send_file(target_ip, port, file_path, callback)
}

pub(crate) fn send_file_resume_inner(
    target_ip: &str,
    port: u16,
    file_path: &str,
    file_name: &str,
    file_len: u64,
    workers: usize,
    callback: Arc<dyn TransferCallback>,
) -> std::io::Result<bool> {
    let file_id = make_file_id(file_name, file_len);
    let mut stream = TcpStream::connect(format!("{}:{}", target_ip, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(3))).ok();

    let req_msg = format!(
        "REQ2|{}|{}|{}|{}\n",
        file_id, file_name, file_len, RESUME_CHUNK_SIZE
    );
    stream.write_all(req_msg.as_bytes())?;

    let mut resp_buf = [0u8; 64 * 1024];
    let n = stream.read(&mut resp_buf)?;
    let response = String::from_utf8_lossy(&resp_buf[..n]).trim().to_string();

    if response.starts_with("DONE") {
        callback.on_progress(file_len, file_len);
        callback.on_complete(true, "发送完成".into());
        return Ok(true);
    }

    if response.starts_with("REJ") {
        callback.on_complete(false, "对方拒绝接收".into());
        return Ok(true);
    }

    if !response.starts_with("ACC2|") {
        return Ok(false);
    }

    let parts: Vec<&str> = response.split('|').collect();
    if parts.len() < 3 {
        return Ok(false);
    }

    let already_received = parts[1].parse::<u64>().unwrap_or(0).min(file_len);
    let missing_chunks = parse_chunk_ranges(parts[2]);
    if missing_chunks.is_empty() {
        callback.on_progress(file_len, file_len);
        callback.on_complete(true, "发送完成".into());
        return Ok(true);
    }

    callback.on_progress(already_received, file_len);

    let queue = Arc::new(Mutex::new(VecDeque::from(missing_chunks)));
    let progress = Arc::new(Mutex::new(already_received));
    let last_report = Arc::new(Mutex::new(already_received));
    let error_occurred = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles = Vec::new();

    info!(
        "Core: 开始续传/传输，缺失块队列: {}，线程数: {}",
        queue.lock().map(|q| q.len()).unwrap_or(0),
        workers
    );

    for worker_idx in 0..workers {
        let ip = target_ip.to_string();
        let fpath = file_path.to_string();
        let fname = file_name.to_string();
        let fid = file_id.clone();
        let queue_ref = queue.clone();
        let progress_ref = progress.clone();
        let last_report_ref = last_report.clone();
        let callback_ref = callback.clone();
        let error_flag = error_occurred.clone();

        let handle = thread::spawn(move || {
            loop {
                let chunk_index = {
                    let mut queue = queue_ref.lock().unwrap();
                    queue.pop_front()
                };

                let Some(chunk_index) = chunk_index else {
                    break;
                };

                let offset = chunk_index as u64 * RESUME_CHUNK_SIZE;
                let length = chunk_len(file_len, RESUME_CHUNK_SIZE, chunk_index);
                if let Err(e) = send_chunk_v2(
                    &ip,
                    port,
                    &fpath,
                    &fname,
                    &fid,
                    chunk_index,
                    offset,
                    length,
                    progress_ref.clone(),
                    last_report_ref.clone(),
                    file_len,
                    callback_ref.clone(),
                ) {
                    error!(
                        "续传线程 {} 传输块 {} 失败: {:?}",
                        worker_idx, chunk_index, e
                    );
                    error_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.join();
    }

    if error_occurred.load(std::sync::atomic::Ordering::Relaxed) {
        callback.on_complete(false, "传输过程中发生错误，可重新发送继续补传".into());
    } else {
        callback.on_progress(file_len, file_len);
        callback.on_complete(true, "发送完成".into());
    }

    Ok(true)
}

pub(crate) fn send_file_legacy_inner(
    target_ip: &str,
    port: u16,
    file_path: &str,
    file_name: &str,
    file_len: u64,
    workers: usize,
    callback: Arc<dyn TransferCallback>,
) -> Result<(), String> {
    let mut stream = TcpStream::connect(format!("{}:{}", target_ip, port))
        .map_err(|e| format!("连接失败: {:?}", e))?;

    let req_msg = format!("REQ|{}|{}\n", file_name, file_len);
    stream
        .write_all(req_msg.as_bytes())
        .map_err(|e| format!("发送请求失败: {:?}", e))?;

    let mut resp_buf = [0u8; 1024];
    let n = stream.read(&mut resp_buf).unwrap_or(0);
    let response = String::from_utf8_lossy(&resp_buf[..n]);

    if !response.starts_with("ACC") {
        return Err("对方拒绝接收".into());
    }

    drop(stream);

    let chunk_size = if workers as u64 > file_len && file_len > 0 {
        1
    } else {
        (file_len / workers as u64).max(1)
    };
    let progress = Arc::new(Mutex::new(0u64));
    let last_report = Arc::new(Mutex::new(0u64));
    let error_occurred = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles = vec![];

    info!("Core: 开始旧协议并行传输，线程数: {}", workers);

    for i in 0..workers {
        let ip = target_ip.to_string();
        let fname = file_name.to_string();
        let fpath = file_path.to_string();
        let progress_ref = progress.clone();
        let last_report_ref = last_report.clone();
        let callback_ref = callback.clone();
        let error_flag = error_occurred.clone();

        let start = i as u64 * chunk_size;
        if start >= file_len && file_len > 0 {
            continue;
        }
        let mut length = chunk_size;
        if i == workers - 1 || start + length > file_len {
            length = file_len.saturating_sub(start);
        }

        let handle = thread::spawn(move || {
            if let Err(e) = send_chunk(
                &ip,
                port,
                &fpath,
                &fname,
                start,
                length,
                progress_ref,
                last_report_ref,
                file_len,
                callback_ref,
            ) {
                error!("旧协议线程 {} 传输失败: {:?}", i, e);
                error_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.join();
    }

    if error_occurred.load(std::sync::atomic::Ordering::Relaxed) {
        Err("传输过程中发生错误，请检查日志".into())
    } else {
        callback.on_progress(file_len, file_len);
        callback.on_complete(true, "发送完成".into());
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn send_chunk(
    ip: &str,
    port: u16,
    path: &str,
    filename: &str,
    offset: u64,
    length: u64,
    progress: Arc<Mutex<u64>>,
    last_report: Arc<Mutex<u64>>,
    total: u64,
    callback: Arc<dyn TransferCallback>,
) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;

    let mut stream = TcpStream::connect(format!("{}:{}", ip, port))?;
    stream.set_nodelay(true).ok();

    // 发送数据头: DATA|filename|offset\n
    let header = format!("DATA|{}|{}\n", filename, offset);
    stream.write_all(header.as_bytes())?;

    // 使用 take 限制读取长度，防止读过界
    let mut handle = file.take(length);
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let n = handle.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        stream.write_all(&buffer[..n])?;

        // 更新进度（这里太频繁锁可能会影响性能，实际可以用 atomic 或者每传 1MB 更新一次）
        if let Ok(mut p) = progress.lock() {
            *p += n as u64;
            maybe_report_progress(*p, total, &last_report, &callback);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_chunk_v2(
    ip: &str,
    port: u16,
    path: &str,
    filename: &str,
    file_id: &str,
    chunk_index: usize,
    offset: u64,
    length: u64,
    progress: Arc<Mutex<u64>>,
    last_report: Arc<Mutex<u64>>,
    total: u64,
    callback: Arc<dyn TransferCallback>,
) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;

    let mut stream = TcpStream::connect(format!("{}:{}", ip, port))?;
    stream.set_nodelay(true).ok();

    let header = format!("DATA2|{}|{}|{}|{}\n", file_id, chunk_index, offset, length);
    stream.write_all(header.as_bytes())?;

    let mut handle = file.take(length);
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let n = handle.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        stream.write_all(&buffer[..n])?;

        if let Ok(mut p) = progress.lock() {
            *p += n as u64;
            maybe_report_progress(*p, total, &last_report, &callback);
        }
    }

    debug!("已发送续传块: {} ({})", chunk_index, filename);
    Ok(())
}

fn maybe_report_progress(
    current: u64,
    total: u64,
    last_report: &Arc<Mutex<u64>>,
    callback: &Arc<dyn TransferCallback>,
) {
    let should_report = {
        let mut last = last_report.lock().unwrap();
        if current >= total || current.saturating_sub(*last) >= RESUME_CHUNK_SIZE {
            *last = current;
            true
        } else {
            false
        }
    };

    if should_report {
        callback.on_progress(current.min(total), total);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_ranges_round_trip() {
        let chunks = vec![0, 1, 2, 5, 7, 8, 9];
        let encoded = format_chunk_ranges(&chunks);
        assert_eq!(encoded, "0-2,5,7-9");
        assert_eq!(parse_chunk_ranges(&encoded), chunks);
    }

    #[test]
    fn chunk_lengths_include_last_partial_chunk() {
        assert_eq!(total_chunks(0, RESUME_CHUNK_SIZE), 1);
        assert_eq!(total_chunks(RESUME_CHUNK_SIZE + 10, RESUME_CHUNK_SIZE), 2);
        assert_eq!(
            chunk_len(RESUME_CHUNK_SIZE + 10, RESUME_CHUNK_SIZE, 0),
            RESUME_CHUNK_SIZE
        );
        assert_eq!(chunk_len(RESUME_CHUNK_SIZE + 10, RESUME_CHUNK_SIZE, 1), 10);
    }
}
