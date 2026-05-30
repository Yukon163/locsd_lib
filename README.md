# LocalSend Core (Rust)

这是一个简单的局域网文件传输核心库，使用 Rust 编写。
它的主要作用是让不同的设备（比如手机和电脑）在同一个 Wi-Fi 下互相发现，并高速传输文件。

## 能做什么？

1.  **自动发现设备**：不用手动输入 IP，打开 App 就能看到局域网里的其他人，别人打开 App 也会同时更新该设备。
2.  **高速传文件**：支持多线程并行发送，速度很快。
3.  **支持多平台**：
    *   **Android**: 提供了接口，可以直接给安卓 App 用。
    *   **电脑端**: 自带一个简单的界面，可以在 Windows/Mac/Linux 上运行。

## 文件夹说明

*   `src/core`: **最核心的代码**。规定了设备怎么打招呼（发现），文件怎么切片发送（传输）。
*   `src/platforms/android.rs`: **给安卓用的接口**。通过 JNI 技术，让 Java 代码能调用 Rust 代码。
*   `src/app/wlm.rs`: **电脑端演示程序**。一个简单的图形界面，用来测试发送和接收功能。

## 怎么运行？

### 1. 编译/运行电脑端 App

电脑端图形界面在 `src/app/wlm.rs`，对应 Cargo bin 名称是 `localsend_app`，需要启用 `bin` feature。

直接运行调试版：

```bash
cargo run --features bin --bin localsend_app
```

编译调试版：

```bash
cargo build --features bin --bin localsend_app
```

产物路径：

```text
target/debug/localsend_app.exe
```

编译发布版：

```bash
cargo build --release --features bin --bin localsend_app
```

产物路径：

```text
target/release/localsend_app.exe
```

### 2. 编译给 Android 用

如果你是安卓开发者，想把这个库集成进去：

1.  需要配置好 Rust 的安卓编译环境 (NDK)。
2.  运行编译命令：
    ```bash
    cargo build --target aarch64-linux-android --release --lib
    ```
3.  把生成的 `.so` 文件放到你的安卓工程里。
