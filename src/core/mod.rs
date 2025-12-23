use std::net::UdpSocket;
use std::thread;
use std::sync::Arc;
use log::{info, error, debug};

pub trait DiscoveryCallback: Send + Sync {
    fn on_device_found(&self, device_info: String);
}

pub fn start_listening(port: u16, callback: Box<dyn DiscoveryCallback>) {
    let callback = Arc::new(callback);

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
            match socket.recv_from(&mut buf) {
                Ok((size, addr)) => {
                    info!("Core: 收到 {} 字节来自 {}", size, addr);
                    let msg = String::from_utf8_lossy(&buf[..size]).to_string();
                    let device_info = format!("{}@{}", msg, addr.ip());

                    callback.on_device_found(device_info);
                }
                Err(e) => {
                    error!("Core: 接收错误: {:?}", e);
                }
            }
        }
    });
}