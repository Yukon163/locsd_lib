use crate::core::{self, DeviceInfo, DiscoveryCallback, TransferCallback};
use log::{info, error, debug};
use std::ffi::{CStr, CString, c_char};
use std::sync::Arc;

pub type OnDeviceFoundCallback = extern "C" fn(*const c_char);

struct WindowsDiscoveryBridge {
    // 这里保存的是外部（Dart/UI）传入的函数指针
    callback_ptr: OnDeviceFoundCallback,
}

unsafe impl Send for WindowsDiscoveryBridge {}
unsafe impl Sync for WindowsDiscoveryBridge {}


impl DiscoveryCallback for WindowsDiscoveryBridge {
    fn on_device_found(&self, device_info: DeviceInfo) {
        let msg = format!(
            "{}|{}|{}|{}",
            device_info.device_id,
            device_info.name,
            device_info.ip,
            device_info.control_port
        );

        if let Ok(c_msg) = CString::new(msg) {
            debug!("Windows 回调触发: {:?}", c_msg);
            (self.callback_ptr)(c_msg.as_ptr());
        }
    }
}

pub type OnReceiveRequestCallback =
extern "C" fn(file_name: *const c_char, file_size: u64, sender_ip: *const c_char) -> bool;

pub type OnProgressCallback =
extern "C" fn(transferred: u64, total: u64);

pub type OnTransferCompleteCallback =
extern "C" fn(success: bool, msg: *const c_char);

struct WindowsTransferBridge {
    on_request: OnReceiveRequestCallback,
    on_progress: OnProgressCallback,
    on_complete: OnTransferCompleteCallback,
}

unsafe impl Send for WindowsTransferBridge {}
unsafe impl Sync for WindowsTransferBridge {}

impl TransferCallback for WindowsTransferBridge {
    fn on_receive_request(&self, file_name: String, file_size: u64, sender_ip: String) -> bool {
        let fname = CString::new(file_name).unwrap();
        let ip = CString::new(sender_ip).unwrap();

        (self.on_request)(fname.as_ptr(), file_size, ip.as_ptr())
    }

    fn on_progress(&self, transferred: u64, total: u64) {
        (self.on_progress)(transferred, total);
    }

    fn on_complete(&self, success: bool, msg: String) {
        let c_msg = CString::new(msg).unwrap_or_else(|_| CString::new("").unwrap());
        (self.on_complete)(success, c_msg.as_ptr());
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_start_discovery(
    port: u16,
    user_alias: *const c_char,
    callback: OnDeviceFoundCallback
) {
    let _ = env_logger::try_init();

    info!("Windows: FFI startDiscovery 被调用");

    let device_name = if user_alias.is_null() {
        "Unknown Windows PC".to_string()
    } else {
        unsafe {
            CStr::from_ptr(user_alias)
                .to_string_lossy()
                .into_owned()
        }
    };

    let bridge = WindowsDiscoveryBridge {
        callback_ptr: callback,
    };

    core::start_listening(
        port,
        "windows_pc".into(),
        device_name,
        Box::new(bridge)
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_discover_once(port: u16, user_alias: *const c_char,) {
    debug!("Windows: FFI discoverOnce 被调用");
    let device_name = if user_alias.is_null() {
        "Unknown Windows PC".to_string()
    } else {
        unsafe {
            CStr::from_ptr(user_alias)
                .to_string_lossy()
                .into_owned()
        }
    };
    core::send_discover_once(port, "windows_pc".into(), device_name);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_start_file_server(
    port: u16,
    save_dir: *const c_char,
    on_request: OnReceiveRequestCallback,
    on_progress: OnProgressCallback,
    on_complete: OnTransferCompleteCallback,
) {
    let save_path = unsafe {
        if save_dir.is_null() {
            ".".into()
        } else {
            CStr::from_ptr(save_dir).to_string_lossy().into_owned()
        }
    };

    info!("Windows: startFileServer, save_dir={}", save_path);

    let bridge = WindowsTransferBridge {
        on_request,
        on_progress,
        on_complete,
    };

    core::start_file_server(
        port,
        save_path,
        Box::new(bridge),
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_send_file(
    target_ip: *const c_char,
    port: u16,
    file_path: *const c_char,
    parallel_cnt: u64,
    on_request: OnReceiveRequestCallback,
    on_progress: OnProgressCallback,
    on_complete: OnTransferCompleteCallback,
) {
    let ip = unsafe { CStr::from_ptr(target_ip).to_string_lossy().into_owned() };
    let path = unsafe { CStr::from_ptr(file_path).to_string_lossy().into_owned() };

    info!("Windows: sendFile {} -> {}", path, ip);

    let bridge = WindowsTransferBridge {
        on_request,
        on_progress,
        on_complete,
    };

    core::send_file(
        ip,
        port,
        path,
        parallel_cnt,
        Box::new(bridge),
    );
}