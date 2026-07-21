# LocalSend Core (Rust)

这是一个简单的局域网文件传输核心库，使用 Rust 编写。
它的主要作用是让不同的设备（比如手机和电脑）在同一个 Wi-Fi 下互相发现，并高速传输文件。

## 能做什么？

1.  **自动发现设备**：不用手动输入 IP，打开 App 就能看到局域网里的其他人，别人打开 App 也会同时更新该设备。
2.  **可靠、自适应传文件**：V3 使用 BLAKE3 文件身份、ACK 后 durable 进度、区间续传和 8–12 个持久 worker 自适应调度。
3.  **支持多平台**：
    *   **Android**: 提供了接口，可以直接给安卓 App 用。
    *   **电脑端**: 自带一个简单的界面，可以在 Windows/Mac/Linux 上运行。

## 文件夹说明

*   `src/core`: 设备发现及 V2/legacy 兼容实现。
*   `src/transfer`: V3 协议、身份、区间、Meta、收发会话、指标和自适应调度。
*   `src/platforms/android.rs`: **给安卓用的接口**。通过 JNI 技术，让 Java 代码能调用 Rust 代码。
*   `src/app/wlm.rs`: **电脑端演示程序**。一个简单的图形界面，用来测试发送和接收功能。

## V3 传输保证

V3 控制帧使用固定头和长度前缀 JSON，数据帧使用 offset 加二进制 payload；所有读取都使用 `read_exact` 并限制帧长。区间统一为半开区间 `[start, end)`。

接收端将临时文件保存为 `.localsend_v3/<transferKey>.part`。一个区间只有在校验、`sync_data()` 和原子 Meta 更新全部成功后才返回 ACK；发送端百分比默认只使用 ACK 对应的 durable bytes。全部区间完成后还会核对 BLAKE3 全文件哈希，再以不覆盖已有文件的方式重命名。

调度器每 200ms 调度、每 1s 计算 ACK 吞吐 EWMA。普通最小 range 为 512KiB，75%/90% 尾段分别降为 256KiB/128KiB；扩缩容、pause/split、SplitTrial rollback、10 秒 cooldown 和 tail rescue 都在 Rust 内完成。V2/legacy 仅在对端明确报告 V3 `unsupported` 后进入。

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
