#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[path = "../core/mod.rs"]
mod core;

use eframe::egui;
use std::sync::{Arc, Mutex};
use std::thread;
use log::{info, error};
use std::time::Duration;

// ----------------------------------------------------------------------------
// 共享应用状态
// ----------------------------------------------------------------------------

struct AppState {
    devices: Vec<core::DeviceInfo>,
    status_msg: String,
    progress: f32,
    is_transferring: bool,
    current_filename: String,
    my_name: String,
    my_port: u16,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            devices: Vec::new(),
            status_msg: "就绪".to_string(),
            progress: 0.0,
            is_transferring: false,
            current_filename: String::new(),
            my_name: "Unknown".to_string(),
            my_port: 4061,
        }
    }
}

// ----------------------------------------------------------------------------
// 回调实现
// ----------------------------------------------------------------------------

// 我们让 Callback 结构体本身支持 Clone，因为它内部只包含轻量级的 Arc 和 Context
#[derive(Clone)]
struct DesktopDiscoveryCallback {
    state: Arc<Mutex<AppState>>,
    ctx: egui::Context,
}

impl core::DiscoveryCallback for DesktopDiscoveryCallback {
    fn on_device_found(&self, device_info: core::DeviceInfo) {
        let mut state = self.state.lock().unwrap();

        let mut found_index = None;

        for (i, d) in state.devices.iter().enumerate() {
            if d.ip == device_info.ip || d.device_id == device_info.device_id {
                found_index = Some(i);
                break;
            }
        }

        // 如果设备已存在则更新，不存在则添加
        if let Some(existing) = state.devices.iter_mut().find(|d| d.ip == device_info.ip) {
            existing.name = device_info.name;
            existing.control_port = device_info.control_port;
            existing.device_id = device_info.device_id;
        } else {
            state.devices.push(device_info);
        }
        self.ctx.request_repaint();
    }
}

#[derive(Clone)]
struct DesktopTransferCallback {
    state: Arc<Mutex<AppState>>,
    ctx: egui::Context,
}

impl core::TransferCallback for DesktopTransferCallback {
    fn on_receive_request(&self, file_name: String, file_size: u64, sender_ip: String) -> bool {
        let mut state = self.state.lock().unwrap();
        state.is_transferring = true;
        state.current_filename = file_name.clone();
        state.status_msg = format!("正在接收 {} ({} bytes) 来自 {}", file_name, file_size, sender_ip);
        state.progress = 0.0;
        self.ctx.request_repaint();

        info!("自动接收文件: {}", file_name);
        true // 自动同意接收
    }

    fn on_progress(&self, transferred: u64, total: u64) {
        let mut state = self.state.lock().unwrap();
        if total > 0 {
            state.progress = transferred as f32 / total as f32;
        }
        self.ctx.request_repaint();
    }

    fn on_complete(&self, success: bool, msg: String) {
        let mut state = self.state.lock().unwrap();
        state.is_transferring = false;
        state.progress = if success { 1.0 } else { 0.0 };
        state.status_msg = if success {
            format!("传输成功: {}", state.current_filename)
        } else {
            format!("传输失败: {}", msg)
        };
        self.ctx.request_repaint();
    }
}

// ----------------------------------------------------------------------------
// GUI 主程序
// ----------------------------------------------------------------------------

struct LocalSendApp {
    state: Arc<Mutex<AppState>>,
}

impl LocalSendApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        // 初始化日志
        env_logger::builder()
            .filter_level(log::LevelFilter::Debug)
            .init();

        configure_fonts(&cc.egui_ctx);

        // 使用时间戳生成简单的随机后缀
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() % 10000;

        let device_name = format!("Desktop-{}", suffix);
        let save_dir = "received_files".to_string();

        // 创建接收文件夹
        if !std::path::Path::new(&save_dir).exists() {
            let _ = std::fs::create_dir(&save_dir);
        }

        let state = Arc::new(Mutex::new(AppState::default()));
        {
            let mut s = state.lock().unwrap();
            s.my_name = device_name.clone();
            s.my_port = 4061;
        }

        // 准备回调对象 (直接创建结构体，不再套 Arc，因为结构体内部就是 Arc)
        let disc_cb = DesktopDiscoveryCallback {
            state: state.clone(),
            ctx: cc.egui_ctx.clone(),
        };

        let trans_cb = DesktopTransferCallback {
            state: state.clone(),
            ctx: cc.egui_ctx.clone(),
        };

        // 启动 UDP 发现 (注意：start_listening 需要 move 进去)
        let name_for_udp = device_name.clone();
        let id_for_udp = device_name.clone();

        core::start_listening(
            4060,
            id_for_udp,
            name_for_udp,
            Box::new(disc_cb) // 直接 Box 结构体，不 Box Arc
        );

        // 启动 TCP 文件服务
        core::start_file_server(
            4061,
            save_dir,
            Box::new(trans_cb)
        );

        // 发送上线广播
        core::send_discover_once(4060, device_name.clone(), device_name);

        Self { state }
    }

    fn send_file(&self, target_ip: String, ctx: egui::Context) {
        let state_ref = self.state.clone();

        // 使用 rfd 选择文件
        let file = rfd::FileDialog::new().pick_file();

        if let Some(path_buf) = file {
            let path_str = path_buf.to_string_lossy().to_string();
            let file_name = path_buf.file_name().unwrap().to_string_lossy().to_string();

            {
                let mut s = state_ref.lock().unwrap();
                s.status_msg = format!("准备发送: {}", file_name);
                s.current_filename = file_name;
                s.is_transferring = true;
                s.progress = 0.0;
            }

            // 发送专用的临时 Callback
            struct SenderCallback {
                state: Arc<Mutex<AppState>>,
                ctx: egui::Context,
            }
            impl core::TransferCallback for SenderCallback {
                fn on_receive_request(&self, _: String, _: u64, _: String) -> bool { true }
                fn on_progress(&self, transferred: u64, total: u64) {
                    let mut s = self.state.lock().unwrap();
                    if total > 0 {
                        s.progress = transferred as f32 / total as f32;
                    }
                    self.ctx.request_repaint();
                }
                fn on_complete(&self, success: bool, msg: String) {
                    let mut s = self.state.lock().unwrap();
                    s.is_transferring = false;
                    s.status_msg = if success { "发送成功".into() } else { format!("发送失败: {}", msg) };
                    s.progress = if success { 1.0 } else { 0.0 };
                    self.ctx.request_repaint();
                }
            }

            let cb = SenderCallback { state: state_ref, ctx };

            // 启动发送
            core::send_file(target_ip, 4061, path_str, 4, Box::new(cb));
        }
    }

    // 抽离 UI 渲染逻辑
    // src/app/wlm.rs -> impl LocalSendApp -> fn render_ui

    fn render_ui(&self, ctx: &egui::Context) {
        let mut target_ip_to_send: Option<String> = None;

        {
            let state = self.state.lock().unwrap();

            egui::CentralPanel::default().show(ctx, |ui| {
                // 标题栏
                ui.horizontal(|ui| {
                    ui.heading("LocalSend Desktop");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!("Name: {}", state.my_name));
                    });
                });

                ui.separator();

                // ============ 修复开始：状态栏与进度条 ============
                ui.group(|ui| {
                    ui.vertical(|ui| {
                        ui.label("当前状态:");

                        // 1. 允许文字换行，防止文件名过长撑爆窗口
                        ui.label(
                            egui::RichText::new(&state.status_msg)
                                .color(egui::Color32::LIGHT_BLUE)
                        );
                    });

                    if state.progress > 0.0 || state.is_transferring {
                        ui.add_space(5.0);

                        let progress_safe = state.progress.clamp(0.0, 1.0);
                        ui.add(
                            egui::ProgressBar::new(progress_safe)
                                .show_percentage()
                                .animate(state.is_transferring)
                        );
                    }
                });
                // ============ 修复结束 ============

                ui.add_space(20.0);

                ui.horizontal(|ui| {
                    ui.heading("设备列表");
                    if ui.button("⟳ 刷新").clicked() {
                        let name = state.my_name.clone();
                        thread::spawn(move || {
                            // 确保这里是 4060，对应上一轮修复
                            core::send_discover_once(4060, name.clone(), name);
                        });
                    }
                });

                egui::ScrollArea::vertical().id_source("dev_list").show(ui, |ui| {
                    for device in &state.devices {
                        ui.push_id(&device.ip, |ui| {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label(egui::RichText::new(&device.name).heading());
                                        ui.monospace(&device.ip);
                                    });
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button("📁 发送文件").clicked() {
                                            target_ip_to_send = Some(device.ip.clone());
                                        }
                                    });
                                });
                            });
                        });
                        ui.add_space(5.0);
                    }
                });

                if state.devices.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("暂无设备，请确保两端都在同一局域网并打开了APP");
                    });
                }
            });
        }

        if let Some(ip) = target_ip_to_send {
            self.send_file(ip, ctx.clone());
        }
    }
}

// 唯一的 App Trait 实现
impl eframe::App for LocalSendApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render_ui(ctx);
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "LocalSend Rust",
        options,
        Box::new(|cc| Box::new(LocalSendApp::new(cc))),
    )
}

fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // 尝试加载 Windows 系统自带的中文字体 (SimHei.ttf - 黑体)
    // 如果你在 Linux/Mac 上，需要改为对应的字体路径，或者将字体文件复制到项目根目录
    let font_path = "C:\\Windows\\Fonts\\simhei.ttf";

    // 如果读取系统字体失败，你可以把 .ttf 文件放到项目旁边，读取 "./my_font.ttf"
    match std::fs::read(font_path) {
        Ok(bytes) => {
            // 1. 将字体数据加载到 context
            fonts.font_data.insert(
                "my_chinese_font".to_owned(),
                egui::FontData::from_owned(bytes),
            );

            // 2. 将新字体插入到 Proportional (普通文本) 的首位
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.insert(0, "my_chinese_font".to_owned());
            }

            // 3. 将新字体插入到 Monospace (等宽文本) 的首位
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                family.insert(0, "my_chinese_font".to_owned());
            }

            // 4. 应用配置
            ctx.set_fonts(fonts);
            info!("中文字体加载成功: {}", font_path);
        },
        Err(e) => {
            error!("加载中文字体失败: {:?}。中文将显示为方框。", e);
            error!("请确保 {} 存在，或者修改代码指向有效的 .ttf 文件", font_path);
        }
    }
}