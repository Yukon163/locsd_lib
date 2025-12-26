use std::net::{IpAddr, Ipv4Addr, UdpSocket, TcpListener, TcpStream};
use std::thread;
use std::sync::{Arc, Mutex};
use log::{info, error, debug, warn};
use std::time::Duration;
use if_addrs::{get_if_addrs, IfAddr};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::fs::{self, File, OpenOptions};
use std::path::Path;

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
    callback: Box<dyn DiscoveryCallback>
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

                let response = format!(
                    "HERE|{}|{}|{}",
                    device_id,
                    device_name,
                    port
                );

                let target_port = if parts.len() == 4 { parts[3].parse().unwrap_or(4060) } else { 4060 };
                let target_addr = format!("{}:{}", addr.ip(), target_port);

                if let Err(e) = socket.send_to(response.as_bytes(), &target_addr) {
                    error!("Core: 回复 HERE 失败 (至 {}): {:?}", target_addr, e);
                }
            }

            else if msg.starts_with("HERE|") {
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

pub fn start_discovery_broadcaster(
    port: u16,
    device_id: String,
    device_name: String,
) {
    thread::spawn(move || {
        let socket = UdpSocket::bind("0.0.0.0:0").expect("无法绑定发送套接字");  // 0就是随机端口，好强
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

pub fn send_discover_once(
    port: u16,
    device_id: String,
    device_name: String,
) {
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
                if iface.is_loopback() { continue; }
                if let IfAddr::V4(v4_addr) = iface.addr {
                    let ip = v4_addr.ip;
                    let mask = v4_addr.netmask;
                    let broadcast = caculate_broadcast(ip, mask);

                    if !broadcast.is_unspecified() {
                        broadcasts.push(broadcast.to_string());
                    }
                }
            }
        },
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

pub trait TransferCallback: Send + Sync {
    fn on_receive_request(&self, file_name: String, file_size: u64, sender_ip: String) -> bool;
    fn on_progress(&self, transferred: u64, total: u64);
    fn on_complete(&self, success: bool, msg: String);
}

pub fn start_file_server(
    port: u16,
    save_dir: String,
    callback: Box<dyn TransferCallback>,
) {
    let callback = Arc::new(callback);
    let save_dir = Arc::new(save_dir);

    thread::spawn(move || {
        info!("Core: 文件传输服务启动，监听 0.0.0.0:{}", port);
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)) {
            Ok(l) => l,
            Err(e) => {
                error!("Core: 无法绑定传输端口: {:?}", e);
                return;
            }
        };

        let progress_counter = Arc::new(Mutex::new(0u64));
        let current_file_size = Arc::new(Mutex::new(0u64));

        for stream in listener.incoming() {
            match stream {
                Ok(mut socket) => {
                    let callback = callback.clone();
                    let save_dir = save_dir.clone();
                    let progress = progress_counter.clone();
                    let total_size_store = current_file_size.clone();

                    thread::spawn(move || {
                        handle_incoming_connection(socket, save_dir, callback, progress, total_size_store);
                    });
                }
                Err(e) => error!("Core: 连接接收失败: {:?}", e),
            }
        }
    });
}

fn handle_incoming_connection(
    mut socket: TcpStream,
    save_dir: Arc<String>,
    callback: Arc<Box<dyn TransferCallback>>,
    progress_counter: Arc<Mutex<u64>>,
    total_size_store: Arc<Mutex<u64>>,
) {
    let mut buf = [0u8; 1024];
    let bytes_read = match socket.peek(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };

    let mut header_buf = Vec::new();
    let mut char_buf = [0u8; 1];
    loop {
        if let Ok(1) = socket.read(&mut char_buf) {
            if char_buf[0] == b'\n' { break; }
            header_buf.push(char_buf[0]);
        } else {
            return;
        }
    }

    let header_str = String::from_utf8_lossy(&header_buf);
    let parts: Vec<&str> = header_str.split('|').collect();

    if parts[0] == "REQ" && parts.len() >= 3 {
        let filename = parts[1];
        let size: u64 = parts[2].parse().unwrap_or(0);
        let sender_ip = socket.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();

        if callback.on_receive_request(filename.to_string(), size, sender_ip) {
            let path = Path::new(save_dir.as_str()).join(filename);
            if let Ok(file) = File::create(&path) {
                if let Err(e) = file.set_len(size) {
                    error!("无法预分配文件大小: {:?}", e);
                }
                if let Ok(mut t) = total_size_store.lock() { *t = size; }
                if let Ok(mut p) = progress_counter.lock() { *p = 0; }

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

        let path = Path::new(save_dir.as_str()).join(filename);

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

                    if current_total - last_progress_update > 1024 * 1024 || current_total == total {
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

pub fn send_file(
    target_ip: String,
    port: u16,
    file_path: String,
    parallel_cnt: u64, // 并行线程数，建议 4-8
    callback: Box<dyn TransferCallback> // 用于回传发送进度
) {
    thread::spawn(move || {
        let path = Path::new(&file_path);
        if !path.exists() {
            callback.on_complete(false, "文件不存在".into());
            return;
        }

        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        let file_len = path.metadata().unwrap().len();

        // 1. 发送握手请求 (REQ)
        let mut stream = match TcpStream::connect(format!("{}:{}", target_ip, port)) {
            Ok(s) => s,
            Err(e) => {
                callback.on_complete(false, format!("连接失败: {:?}", e));
                return;
            }
        };

        let req_msg = format!("REQ|{}|{}\n", file_name, file_len);
        let _ = stream.write_all(req_msg.as_bytes());

        // 等待响应
        let mut resp_buf = [0u8; 1024];
        let n = stream.read(&mut resp_buf).unwrap_or(0);
        let response = String::from_utf8_lossy(&resp_buf[..n]);

        if !response.starts_with("ACC") {
            callback.on_complete(false, "对方拒绝接收".into());
            return;
        }

        drop(stream); // 关闭握手连接

        // 2. 计算分片并并行发送
        let chunk_size = file_len / parallel_cnt;
        let mut handles = vec![];
        let progress = Arc::new(Mutex::new(0u64));
        // 使用原子布尔值标记是否有线程出错，任何一个线程出错则整体失败
        let error_occurred = Arc::new(std::sync::atomic::AtomicBool::new(false));

        info!("Core: 开始并行传输，线程数: {}", parallel_cnt);

        for i in 0..parallel_cnt {
            let ip = target_ip.clone();
            let fname = file_name.clone();
            let fpath = file_path.clone();
            let progress_ref = progress.clone();
            let error_flag = error_occurred.clone();
            
            // 计算当前线程负责的范围
            let start = i * chunk_size;
            let mut length = chunk_size;
            if i == parallel_cnt - 1 {
                length = file_len - start; // 最后一个线程处理剩余所有
            }

            let handle = thread::spawn(move || {
                if let Err(e) = send_chunk(&ip, port, &fpath, &fname, start, length, progress_ref) {
                    error!("线程 {} 传输失败: {:?}", i, e);
                    error_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            });
            handles.push(handle);
        }

        // 等待所有线程完成
        for h in handles {
            let _ = h.join();
        }

        if error_occurred.load(std::sync::atomic::Ordering::Relaxed) {
             callback.on_complete(false, "传输过程中发生错误，请检查日志".into());
        } else {
             callback.on_complete(true, "发送完成".into());
        }
    });
}

fn send_chunk(
    ip: &str,
    port: u16,
    path: &str,
    filename: &str,
    offset: u64,
    length: u64,
    progress: Arc<Mutex<u64>>
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
        if n == 0 { break; }
        stream.write_all(&buffer[..n])?;

        // 更新进度（这里太频繁锁可能会影响性能，实际可以用 atomic 或者每传 1MB 更新一次）
        if let Ok(mut p) = progress.lock() {
            *p += n as u64;
        }
    }
    Ok(())
}