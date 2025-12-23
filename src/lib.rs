use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::JNIEnv;

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