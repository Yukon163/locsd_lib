use jni::objects::{JClass, JString, JValue, GlobalRef};
use jni::{JavaVM, JNIEnv};
use std::sync::Arc;
use log::{info, error, debug, LevelFilter};
use android_logger::Config;
use crate::core::{self, DeviceInfo, DiscoveryCallback};

struct AndroidBridge {
    jvm: Arc<JavaVM>,
    class_ref: GlobalRef,
}

impl DiscoveryCallback for AndroidBridge {
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

    let bridge = AndroidBridge {
        jvm: Arc::new(jvm),
        class_ref: class_global_ref,
    };

    let device_name: String = env
        .get_string(&user_alias)
        .expect("Couldn't get java string!")
        .into();

    core::start_listening(
        4060,
        "android phone".into(),
        device_name,
        Box::new(bridge)
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_yukon_localsend_RustSDK_discoverOnce(
    _env: JNIEnv,
    _class: JClass,
) {
    core::send_discover_once(4060);
}
