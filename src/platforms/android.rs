use jni::objects::{JClass, JString, JValue, GlobalRef};
use jni::{JavaVM, JNIEnv};
use std::sync::Arc;
use log::{info, error, debug, LevelFilter};
use android_logger::Config;
use crate::core::{self, DeviceInfo, DiscoveryCallback, TransferCallback};

struct AndroidDiscoveryBridge {
    jvm: Arc<JavaVM>,
    class_ref: GlobalRef,
}

impl DiscoveryCallback for AndroidDiscoveryBridge {
    fn on_device_found(&self, device_info: DeviceInfo) {
        if let Ok(mut env) = self.jvm.attach_current_thread() {
            let msg = format!(
                "{}|{}|{}|{}",
                device_info.device_id,
                device_info.name,
                device_info.ip,
                device_info.control_port,
            );

            if let Ok(j_msg) = env.new_string(msg) {
                let result = env.call_static_method(
                    &self.class_ref,
                    "onDeviceFound",
                    "(Ljava/lang/String;)V",
                    &[JValue::from(&j_msg)],
                );

                if let Err(e) = result {
                    error!("Android 回调失败: {:?}", e);
                } else {
                    debug!("Android 回调成功");
                }
            }
        }
    }
}

struct AndroidTransferBridge {
    jvm: Arc<JavaVM>,
    class_ref: GlobalRef,
}

impl TransferCallback for AndroidTransferBridge {
    // 收到发送请求，返回 true 表示同意接收，false 拒绝
    // 这里是一个同步调用，但在 Android 上我们需要异步弹窗。
    // *简化方案*：暂时默认自动接收，或者通过 JNI 询问 Java 侧（需要 Java 侧阻塞等待用户点击，这很难实现）。
    // *进阶方案*：协议上应该分两步：1. 收到请求 -> 通知 UI -> UI 发送 "ACCEPT" 命令。
    //
    // 为了配合你目前的进度，这里我们先调用 Java 的 onReceiveRequest，
    // 并假设 Java 端如果返回 true 则 Rust 继续，否则拒绝。
    // 注意：这意味着 Java 端的 onReceiveRequest 不能是弹窗（因为会阻塞网络线程），
    // 除非我们在 Rust 这边做更复杂的异步等待。
    //
    // 现在的逻辑是：调用 Java 静态方法，获取返回值 (boolean)。
    fn on_receive_request(&self, file_name: String, file_size: u64, sender_ip: String) -> bool {
        if let Ok(mut env) = self.jvm.attach_current_thread() {
            let j_filename = env.new_string(file_name).unwrap();
            let j_sender_ip = env.new_string(sender_ip).unwrap();
            // Java long 对应 Rust i64 (JNI 中 jlong 是 i64)
            let j_size = file_size as i64;

            let result = env.call_static_method(
                &self.class_ref,
                "onReceiveRequest",
                "(Ljava/lang/String;JLjava/lang/String;)Z", // 签名: (String, long, String) -> boolean
                &[
                    JValue::from(&j_filename),
                    JValue::from(j_size),
                    JValue::from(&j_sender_ip)
                ],
            );

            match result {
                Ok(val) => val.z().unwrap_or(false), // .z() 获取 boolean 值
                Err(e) => {
                    error!("Android Transfer Request 回调失败: {:?}", e);
                    false // 出错默认拒绝
                }
            }
        } else {
            false
        }
    }

    fn on_progress(&self, transferred: u64, total: u64) {
        if let Ok(mut env) = self.jvm.attach_current_thread() {
            let _ = env.call_static_method(
                &self.class_ref,
                "onTransferProgress",
                "(JJ)V", // (long, long) -> void
                &[JValue::from(transferred as i64), JValue::from(total as i64)],
            );
        }
    }

    fn on_complete(&self, success: bool, msg: String) {
        if let Ok(mut env) = self.jvm.attach_current_thread() {
            let j_msg = env.new_string(msg).unwrap_or_else(|_| env.new_string("").unwrap());
            let _ = env.call_static_method(
                &self.class_ref,
                "onTransferComplete",
                "(ZLjava/lang/String;)V", // (boolean, String) -> void
                &[JValue::from(success), JValue::from(&j_msg)],
            );
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_yukon_localsend_RustSDK_startDiscovery(
    mut env: JNIEnv,
    _class: JClass,
    user_alias: JString,
) {
    android_logger::init_once(
        Config::default()
            .with_max_level(LevelFilter::Debug)
            .with_tag("YukonTestRustSDK"),
    );
    info!("Android: JNI startDiscovery 被调用");

    let jvm = env.get_java_vm().expect("无法获取 JavaVM");
    let rust_sdk_class = env.find_class("com/yukon/localsend/RustSDK")
        .expect("无法找到 RustSDK 类");
    let class_global_ref = env.new_global_ref(rust_sdk_class)
        .expect("无法创建全局引用");

    let bridge = AndroidDiscoveryBridge {
        jvm: Arc::new(jvm),
        class_ref: class_global_ref,
    };

    let device_name: String = env
        .get_string(&user_alias)
        .expect("Couldn't get java string!")
        .into();

    core::start_listening(
        4060,
        device_name.clone(),
        device_name,
        Box::new(bridge)
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_yukon_localsend_RustSDK_discoverOnce(
    mut env: JNIEnv,
    _class: JClass,
    user_alias: JString,
) {
    let device_name: String = env
        .get_string(&user_alias)
        .expect("Couldn't get java string!")
        .into();
    core::send_discover_once(
    4060,
         device_name.clone(),
         device_name,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_yukon_localsend_RustSDK_startFileServer(
    mut env: JNIEnv,
    _class: JClass,
    save_dir: JString,
) {
    let jvm = env.get_java_vm().expect("无法获取 JavaVM");
    let rust_sdk_class = env.find_class("com/yukon/localsend/RustSDK")
        .expect("无法找到 RustSDK 类");
    let class_global_ref = env.new_global_ref(rust_sdk_class)
        .expect("无法创建全局引用");

    let bridge = AndroidTransferBridge {
        jvm: Arc::new(jvm),
        class_ref: class_global_ref,
    };

    let save_path: String = env
        .get_string(&save_dir)
        .expect("无法获取保存路径字符串")
        .into();

    core::start_file_server(
        4061,
        save_path,
        Box::new(bridge)
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_yukon_localsend_RustSDK_sendFile(
    mut env: JNIEnv,
    _class: JClass,
    target_ip: JString,
    file_path: JString,
) {
    let jvm = env.get_java_vm().expect("无法获取 JavaVM");
    let rust_sdk_class = env.find_class("com/yukon/localsend/RustSDK")
        .expect("无法找到 RustSDK 类");
    let class_global_ref = env.new_global_ref(rust_sdk_class)
        .expect("无法创建全局引用");

    let bridge = AndroidTransferBridge {
        jvm: Arc::new(jvm),
        class_ref: class_global_ref,
    };

    let ip: String = env.get_string(&target_ip).unwrap().into();
    let path: String = env.get_string(&file_path).unwrap().into();

    // 假设 4 并行线程
    core::send_file(
        ip,
        4061,
        path,
        8,
        Box::new(bridge)
    );
}
