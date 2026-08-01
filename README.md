# LocalSend Core (Rust)

这是一个简单的局域网文件传输核心库，使用 Rust 编写。
它的主要作用是让不同的设备（比如手机和电脑）在同一个 Wi-Fi 下互相发现，并高速传输文件。

## 能做什么？

1.  **自动发现设备**：不用手动输入 IP，打开 App 就能看到局域网里的其他人，别人打开 App 也会同时更新该设备。
2.  **可靠、自适应传文件**：V4 使用快速会话身份、后台 BLAKE3、ACK 后 durable 进度、可信区间续传和 8–20 个持久 worker 自适应调度。
3.  **支持多平台**：
    *   **Android**: 提供了接口，可以直接给安卓 App 用。
    *   **电脑端**: 自带一个简单的界面，可以在 Windows/Mac/Linux 上运行。

## 文件夹说明

*   `src/core`: 设备发现及 V2/legacy 兼容实现。
*   `src/transfer`: V4 协议、身份、区间、Meta、收发会话、指标和自适应调度。
*   `src/platforms/android.rs`: **给安卓用的接口**。通过 JNI 技术，让 Java 代码能调用 Rust 代码。
*   `src/app/wlm.rs`: **电脑端演示程序**。一个简单的图形界面，用来测试发送和接收功能。

## V4 传输保证

V4 控制帧使用固定头和长度前缀 JSON，数据帧使用 offset 加二进制 payload；所有读取都使用 `read_exact` 并限制帧长。区间统一为半开区间 `[start, end)`。发送端先用稳定源属性生成快速会话键，接收方接受后立即启动 range worker，完整 BLAKE3 与传输并行计算。

接收端将临时文件保存为 `.localsend_v3/<transferKey>.part`。每个 worker 使用独立文件句柄和 seek 写入互不重叠的区间；一个区间只有在校验、`sync_data()` 和原子 Meta 更新全部成功后才返回 ACK。Meta v6 记录 Receiving/Finalizing/Completed 阶段与逐段 BLAKE3；恢复时收发双方并行复核 durable segment，不匹配的区间会被单独作废和补传。接收端沿连续 durable 前缀后台计算最终哈希，通常无需在传完后重新读取整个文件。

调度器每 200ms 只做 pause/split/tail rescue 等结构动作，每 1s 用扣除重传后的 useful-wire 吞吐和 EWMA 诊断是否加线程。普通最小 range 为 512KiB，75%/90% 尾段分别降为 256KiB/128KiB；SplitTrial 连续观察后才接受，无收益、负收益或错误会恢复原 target、回收新增 worker，并把已 ACK 前缀保留为 durable 进度。

V4 Hello 会校验 deferred-hash 等必要 capability 并协商实际帧大小。ACK 丢失后的 retry 先发 RangeStatus：接收端若仍在 fsync 会返回 busy，完成后返回 durable prefix 及 BLAKE3；发送端对账后只调度缺失后缀。最终发布优先使用原子 no-replace move，在 Android FUSE 等不支持 hard-link 的文件系统上使用 O_EXCL + copy + fsync + hash 校验兜底。旧端在 V4 探测明确不兼容后进入 V2/legacy 兼容层。

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
