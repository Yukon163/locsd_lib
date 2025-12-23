use jni::objects::{JClass, JString, JValue, GlobalRef};
use jni::sys::jstring;
use jni::{JavaVM, JNIEnv};
use std::net::UdpSocket;
use std::thread;
use std::sync::Arc;
use log::{info, error, debug, LevelFilter};
use android_logger::Config;
use crate::core::{self, DiscoveryCallback};

struct AndroidBridge {
    jvm: Arc<JavaVM>,
    class_ref: GlobalRef,
}

impl DiscoveryCallback for AndroidBridge {
    fn on_device_found(&self, device_info: String) {
        match self.jvm.attach_current_thread() {
            Ok(mut env) => {
                if let Ok(j_msg) = env.new_string(&device_info) {
                    let result = env.call_static_method(
                        &self.class_ref,
                        "onDeviceFound",
                        "(Ljava/lang/String;)V",
                        &[JValue::from(&j_msg)],
                    );

                    if let Err(e) = result {
                        error!("Android: 回调 Java 失败: {:?}", e);
                    } else {
                        debug!("Android: 回调成功 -> {}", device_info);
                    }
                }
            }
            Err(e) => error!("Android: 无法 Attach JVM 线程: {:?}", e),
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_yukon_localsend_RustSDK_startDiscovery(
    mut env: JNIEnv,
    _class: JClass
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

    core::start_listening(4060, Box::new(bridge));
}
