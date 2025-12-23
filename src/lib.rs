use jni::objects::{JClass, JString, JValue, GlobalRef};
use jni::sys::jstring;
use jni::{JavaVM, JNIEnv};
use std::net::UdpSocket;
use std::thread;
use std::sync::Arc;
use log::{info, error, debug, LevelFilter};
use android_logger::Config;

#[unsafe(no_mangle)]
pub extern "C" fn Java_com_yukon_localsend_RustSDK_helloRromRust(
    mut env: JNIEnv,
    _class: JClass,
    path: JString,
) -> jstring {
    let file_path: String = env
        .get_string(&path)
        .expect("Couldn't get java string!")
        .into();

    let response = format!("Rust 接收到路径并准备开始传输: {}", file_path);

    let output = env
        .new_string(response)
        .expect("Couldn't create java string");

    output.into_raw()
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

    info!("startDiscovery 被调用");

    let jvm = env.get_java_vm().expect("无法获取 javaVM");
    let jvm_arc = Arc::new(jvm);

    let rust_sdk_class = env.find_class("com/yukon/localsend/RustSDK")
        .expect("在主线程无法找到 RustSDK 类");

    let class_global_ref = env.new_global_ref(rust_sdk_class)
        .expect("无法创建全局引用");

    thread::spawn(move || {
        info!("Rust UDP 线程启动");

        let socket = match UdpSocket::bind("0.0.0.0:4060") {
          Ok(s) => s,
          Err(e) => {
              error!("UDP 绑定失败: {:?}", e);
              return;
          }
        };

        if let Err(e) = socket.set_broadcast(true) {
            error!("设置广播博士失败: {:?}", e);
        }

        info!("UDP 绑定成功, 开始监听4060端口");

        let mut buf = [0u8; 1024];

        loop {
            match socket.recv_from(&mut buf) {
                Ok((size, addr)) => {
                    info!("收到数据来自: {} 的 {} 字节数据", addr, size);
                    let msg = String::from_utf8_lossy(&buf[..size]).to_string();
                    let device_info = format!("{}@{}", msg, addr.ip());

                    match jvm_arc.attach_current_thread() {
                        Ok(mut env) => {
                            // 创建 Java 字符串
                            if let Ok(j_msg) = env.new_string(&device_info) {
                                // 使用全局引用调用静态方法
                                // 注意：GlobalRef 实现了 AsRef<JObject>，可以直接传给 call_static_method
                                let result = env.call_static_method(
                                    &class_global_ref, // 使用传入的 GlobalRef
                                    "onDeviceFound",
                                    "(Ljava/lang/String;)V",
                                    &[JValue::from(&j_msg)],
                                );

                                if let Err(e) = result {
                                    error!("回调 Java 方法失败: {:?}", e);
                                } else {
                                    debug!("回调成功: {}", device_info);
                                }
                            }
                        }
                        Err(e) => error!("无法 Attach 线程: {:?}", e),
                    }
                }
                Err(e) => {
                    error!("接收数据出错: {:?}", e);
                }
            }
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_com_yukon_localsend_RustSDK_testCallback(
    mut env: JNIEnv,
    _class: JClass,
) {
    if let Ok(class) = env.find_class("com/yukon/localsend/RustSDK") {
        let j_msg = env.new_string("JNI 回调测试成功！").unwrap();
        let _ = env.call_static_method(
            class,
            "onDeviceFound",
            "(Ljava/lang/String;)V",
            &[jni::objects::JValue::from(&j_msg)],
        );
    }
}