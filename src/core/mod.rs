use std::net::UdpSocket;
use std::thread;
use std::sync::Arc;
use log::{info, error, debug};
use std::time::Duration;

pub struct DeviceInfo {
    pub device_id: String,
    pub name: String,
    pub ip: String,
    pub control_port: u16,
}

pub trait DiscoveryCallback: Send + Sync {
    fn on_device_found(&self, device_info: DeviceInfo);
}

pub fn start_listening(
    port: u16,
    device_id: String,
    device_name: String,
    callback: Box<dyn DiscoveryCallback>
) {
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
            let (size, addr) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) => {
                    error!("Core: UDP 接收失败: {:?}", e);
                    continue;
                }
            };

            let msg = String::from_utf8_lossy(&buf[..size]);

            if msg.starts_with("DISCOVER|") {
                let parts: Vec<&str> = msg.split('|').collect();
                if parts.len() == 4 {
                    let device = DeviceInfo {
                        device_id: parts[1].to_string(),
                        name: parts[2].to_string(),
                        ip: addr.ip().to_string(),
                        control_port: parts[3].parse().unwrap_or(4061),
                    };
                    callback.on_device_found(device);
                }

                let response = format!(
                    "HERE|{}|{}|4061",
                    device_id,
                    device_name
                );

                let _ = socket.send_to(response.as_bytes(), addr);
            }

            else if msg.starts_with("HERE|") {
                let parts: Vec<&str> = msg.split('|').collect();
                if parts.len() == 4 {
                    let device = DeviceInfo {
                        device_id: parts[1].to_string(),
                        name: parts[2].to_string(),
                        ip: addr.ip().to_string(),
                        control_port: parts[3].parse().unwrap_or(4061),
                    };

                    callback.on_device_found(device);
                }
            }
        }
    });
}

pub fn start_discovery_broadcaster(port: u16) {
    thread::spawn(move || {
        let socket = UdpSocket::bind("0.0.0.0:0").expect("无法绑定发送套接字");  // 0就是随机端口，好强
        socket.set_broadcast(true).expect("无法设置广播权限");

        let broadcast_addr = format!("255.255.255.255:{}", port);
        let msg = "DISCOVER";

        loop {
            if let Err(e) = socket.send_to(msg.as_bytes(), &broadcast_addr) {
                error!("发现广播失败: {:?}", e);
            } else {
                debug!("已发送 DISCOVER 广播");
            }
            thread::sleep(Duration::from_secs(5));
        }
    });
}

pub fn send_discover_once(port: u16) {
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        socket.set_broadcast(true).ok();
        let _ = socket.send_to(b"DISCOVER", format!("192.168.0.255:{}", port));
    }
}
