use crate::core::{self, DeviceInfo, DiscoveryCallback};
use log::{info, error, debug};
use std::ffi::{CStr, CString, c_char};

pub type OnDeviceFoundCallback = extern "C" fn(*const c_char);

struct WindowsBridge {
    // 这里保存的是外部（Dart/UI）传入的函数指针
    callback_ptr: OnDeviceFoundCallback,
}

unsafe impl Send for WindowsBridge {}
unsafe impl Sync for WindowsBridge {}


impl DiscoveryCallback for WindowsBridge {
    fn on_device_found(&self, device_info: DeviceInfo) {
        let msg = format!(
            "{}|{}|{}|{}",
            device_info.device_id,
            device_info.name,
            device_info.ip,
            device_info.control_port
        );

        let c_msg = match CString::new(msg) {
            Ok(s) => s,
            Err(e) => {
                error!("字符串转换失败: {:?}", e);
                return;
            }
        };

        debug!("Windows 回调触发: {:?}", c_msg);
        (self.callback_ptr)(c_msg.as_ptr());
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

    let bridge = WindowsBridge {
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
pub extern "C" fn rust_discover_once(port: u16) {
    debug!("Windows: FFI discoverOnce 被调用");
    core::send_discover_once(port);
}