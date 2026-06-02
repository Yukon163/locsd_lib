use crate::core::{self, DeviceInfo, DiscoveryCallback, TransferCallback};
use log::info;
use serde_json::json;
use std::ffi::{CStr, CString, c_char};

pub type FlutterEventCallback = extern "C" fn(*mut c_char);

struct FlutterDiscoveryBridge {
    on_event: FlutterEventCallback,
}

unsafe impl Send for FlutterDiscoveryBridge {}
unsafe impl Sync for FlutterDiscoveryBridge {}

struct FlutterTransferBridge {
    on_event: FlutterEventCallback,
    direction: &'static str,
}

unsafe impl Send for FlutterTransferBridge {}
unsafe impl Sync for FlutterTransferBridge {}

fn c_string_arg(value: *const c_char, fallback: &str) -> String {
    if value.is_null() {
        fallback.to_string()
    } else {
        unsafe { CStr::from_ptr(value).to_string_lossy().into_owned() }
    }
}

fn emit_json(on_event: FlutterEventCallback, value: serde_json::Value) {
    if let Ok(payload) = CString::new(value.to_string()) {
        on_event(payload.into_raw());
    }
}

impl DiscoveryCallback for FlutterDiscoveryBridge {
    fn on_device_found(&self, device_info: DeviceInfo) {
        emit_json(
            self.on_event,
            json!({
                "type": "deviceFound",
                "deviceId": device_info.device_id,
                "name": device_info.name,
                "ip": device_info.ip,
                "controlPort": device_info.control_port,
            }),
        );
    }
}

impl TransferCallback for FlutterTransferBridge {
    fn on_receive_request(&self, file_name: String, file_size: u64, sender_ip: String) -> bool {
        emit_json(
            self.on_event,
            json!({
                "type": "receiveRequest",
                "direction": self.direction,
                "fileName": file_name,
                "fileSize": file_size,
                "senderIp": sender_ip,
            }),
        );
        true
    }

    fn on_progress(&self, transferred: u64, total: u64) {
        emit_json(
            self.on_event,
            json!({
                "type": "progress",
                "direction": self.direction,
                "transferred": transferred,
                "total": total,
            }),
        );
    }

    fn on_complete(&self, success: bool, msg: String) {
        emit_json(
            self.on_event,
            json!({
                "type": "complete",
                "direction": self.direction,
                "success": success,
                "message": msg,
            }),
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_flutter_free_string(value: *mut c_char) {
    if !value.is_null() {
        unsafe {
            let _ = CString::from_raw(value);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_flutter_start_discovery(
    port: u16,
    user_alias: *const c_char,
    on_event: FlutterEventCallback,
) {
    let _ = env_logger::try_init();
    let device_name = c_string_arg(user_alias, "Flutter");
    info!("Flutter FFI: start discovery, port={}", port);

    core::start_listening(
        port,
        device_name.clone(),
        device_name,
        Box::new(FlutterDiscoveryBridge { on_event }),
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_flutter_discover_once(port: u16, user_alias: *const c_char) {
    let device_name = c_string_arg(user_alias, "Flutter");
    core::send_discover_once(port, device_name.clone(), device_name);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_flutter_start_file_server(
    port: u16,
    save_dir: *const c_char,
    on_event: FlutterEventCallback,
) {
    let save_path = c_string_arg(save_dir, ".");
    info!(
        "Flutter FFI: start file server, port={}, save_dir={}",
        port, save_path
    );

    core::start_file_server(
        port,
        save_path,
        Box::new(FlutterTransferBridge {
            on_event,
            direction: "receive",
        }),
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_flutter_send_file(
    target_ip: *const c_char,
    port: u16,
    file_path: *const c_char,
    parallel_cnt: u64,
    on_event: FlutterEventCallback,
) {
    let ip = c_string_arg(target_ip, "");
    let path = c_string_arg(file_path, "");
    info!("Flutter FFI: send file {} -> {}", path, ip);

    core::send_file(
        ip,
        port,
        path,
        parallel_cnt,
        Box::new(FlutterTransferBridge {
            on_event,
            direction: "send",
        }),
    );
}
