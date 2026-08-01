#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use localsend_core::core;

use eframe::egui::{self, Color32, Frame, Margin, RichText, Rounding, Stroke, Vec2};
use log::{error, info};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ----------------------------------------------------------------------------
// 颜色主题定义
// ----------------------------------------------------------------------------

struct Theme {
    bg_primary: Color32,
    bg_secondary: Color32,
    bg_tertiary: Color32,
    accent: Color32,
    success: Color32,
    text_primary: Color32,
    text_secondary: Color32,
    text_muted: Color32,
    border: Color32,
    overlay: Color32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg_primary: Color32::from_rgb(26, 26, 46),   // #1a1a2e
            bg_secondary: Color32::from_rgb(22, 33, 62), // #16213e
            bg_tertiary: Color32::from_rgb(15, 52, 96),  // #0f3460
            accent: Color32::from_rgb(0, 217, 255),      // #00d9ff
            success: Color32::from_rgb(74, 222, 128),    // #4ade80
            text_primary: Color32::from_rgb(255, 255, 255),
            text_secondary: Color32::from_rgb(200, 200, 220),
            text_muted: Color32::from_rgb(140, 140, 160),
            border: Color32::from_rgb(60, 60, 90),
            overlay: Color32::from_rgba_unmultiplied(0, 0, 0, 180),
        }
    }
}

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
    // 拖拽状态
    is_file_hovering: bool,
    // 设备选择对话框
    show_device_picker: bool,
    pending_files: Vec<PathBuf>,
    // 存储位置
    save_dir: String,
    // 下载完成状态
    last_received_file: Option<String>,
    show_download_complete: bool,
    // 设置对话框
    show_settings: bool,
    // 状态重置时间
    status_reset_time: Option<Instant>,
    // 速度计算
    transferred_bytes: u64,
    total_bytes: u64,
    last_speed_update: Option<Instant>,
    last_transferred: u64,
    current_speed: f64,                   // bytes per second
    transfer_start_time: Option<Instant>, // 传输开始时间
    average_speed: f64,                   // 平均速度
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
            is_file_hovering: false,
            show_device_picker: false,
            pending_files: Vec::new(),
            save_dir: "received_files".to_string(),
            last_received_file: None,
            show_download_complete: false,
            show_settings: false,
            status_reset_time: None,
            transferred_bytes: 0,
            total_bytes: 0,
            last_speed_update: None,
            last_transferred: 0,
            current_speed: 0.0,
            transfer_start_time: None,
            average_speed: 0.0,
        }
    }
}

// ----------------------------------------------------------------------------
// 回调实现
// ----------------------------------------------------------------------------

#[derive(Clone)]
struct DesktopDiscoveryCallback {
    state: Arc<Mutex<AppState>>,
    ctx: egui::Context,
}

impl core::DiscoveryCallback for DesktopDiscoveryCallback {
    fn on_device_found(&self, device_info: core::DeviceInfo) {
        let mut state = self.state.lock().unwrap();

        // 基于 IP 地址去重：同一 IP 只保留一个设备
        if let Some(existing) = state.devices.iter_mut().find(|d| d.ip == device_info.ip) {
            // 更新已有设备信息
            existing.name = device_info.name;
            existing.control_port = device_info.control_port;
            existing.device_id = device_info.device_id;
        } else {
            // 新设备，添加到列表
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
        state.status_msg = format!("正在接收 {} 来自 {}", file_name, sender_ip);
        state.progress = 0.0;
        state.show_download_complete = false;
        // 初始化速度追踪
        state.transferred_bytes = 0;
        state.total_bytes = file_size;
        state.last_speed_update = Some(Instant::now());
        state.last_transferred = 0;
        state.current_speed = 0.0;
        state.transfer_start_time = Some(Instant::now());
        state.average_speed = 0.0;
        self.ctx.request_repaint();

        info!("自动接收文件: {}", file_name);
        true
    }

    fn on_progress(&self, transferred: u64, total: u64) {
        let mut state = self.state.lock().unwrap();
        if total > 0 {
            state.progress = transferred as f32 / total as f32;
        }
        state.transferred_bytes = transferred;
        state.total_bytes = total;

        // 计算速度（每 500ms 更新一次）
        if let Some(last_update) = state.last_speed_update {
            let elapsed = last_update.elapsed();
            if elapsed >= Duration::from_millis(500) {
                let bytes_delta = transferred.saturating_sub(state.last_transferred);
                state.current_speed = bytes_delta as f64 / elapsed.as_secs_f64();
                state.last_transferred = transferred;
                state.last_speed_update = Some(Instant::now());
            }
        } else {
            state.last_speed_update = Some(Instant::now());
            state.last_transferred = transferred;
        }

        self.ctx.request_repaint();
    }

    fn on_complete(&self, success: bool, msg: String) {
        let mut state = self.state.lock().unwrap();
        state.is_transferring = false;
        state.progress = if success { 1.0 } else { 0.0 };

        // 计算平均速度
        if let Some(start_time) = state.transfer_start_time {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                state.average_speed = state.total_bytes as f64 / elapsed;
            }
        }

        if success {
            // 构建完整文件路径
            let file_path = std::path::Path::new(&state.save_dir)
                .join(&state.current_filename)
                .to_string_lossy()
                .to_string();
            state.last_received_file = Some(file_path);
            state.show_download_complete = true;
            state.status_msg = format!("✓ 接收成功: {}", state.current_filename);
        } else {
            state.status_msg = format!("✗ 传输失败: {}", msg);
        }
        state.status_reset_time = Some(Instant::now());
        self.ctx.request_repaint();
    }
}

// ----------------------------------------------------------------------------
// GUI 主程序
// ----------------------------------------------------------------------------

struct LocalSendApp {
    state: Arc<Mutex<AppState>>,
    theme: Theme,
}

impl LocalSendApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        // 初始化日志
        env_logger::builder()
            .filter_level(log::LevelFilter::Debug)
            .init();

        configure_fonts(&cc.egui_ctx);
        configure_theme(&cc.egui_ctx);

        // 使用时间戳生成简单的随机后缀
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            % 10000;

        let device_name = format!("Desktop-{}", suffix);

        // 获取用户 Downloads 文件夹
        let save_dir = dirs::download_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "received_files".to_string());

        // 创建接收文件夹（如果不存在）
        if !std::path::Path::new(&save_dir).exists() {
            let _ = std::fs::create_dir_all(&save_dir);
        }

        let state = Arc::new(Mutex::new(AppState::default()));
        {
            let mut s = state.lock().unwrap();
            s.my_name = device_name.clone();
            s.my_port = 4061;
            s.save_dir = save_dir.clone();
        }

        let disc_cb = DesktopDiscoveryCallback {
            state: state.clone(),
            ctx: cc.egui_ctx.clone(),
        };

        let trans_cb = DesktopTransferCallback {
            state: state.clone(),
            ctx: cc.egui_ctx.clone(),
        };

        core::start_listening(
            4060,
            device_name.clone(),
            device_name.clone(),
            Box::new(disc_cb),
        );

        core::start_file_server(4061, save_dir, Box::new(trans_cb));

        core::send_discover_once(4060, device_name.clone(), device_name);

        Self {
            state,
            theme: Theme::default(),
        }
    }

    fn send_file(&self, target_ip: String, file_path: PathBuf, ctx: egui::Context) {
        let state_ref = self.state.clone();
        let path_str = file_path.to_string_lossy().to_string();
        let file_name = file_path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();

        {
            let mut s = state_ref.lock().unwrap();
            s.status_msg = format!("准备发送: {}", file_name);
            s.current_filename = file_name;
            s.is_transferring = true;
            s.progress = 0.0;
        }

        struct SenderCallback {
            state: Arc<Mutex<AppState>>,
            ctx: egui::Context,
        }
        impl core::TransferCallback for SenderCallback {
            fn on_receive_request(&self, _: String, _: u64, _: String) -> bool {
                true
            }
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
                s.status_msg = if success {
                    "✓ 发送成功".into()
                } else {
                    format!("✗ 发送失败: {}", msg)
                };
                s.progress = if success { 1.0 } else { 0.0 };
                s.status_reset_time = Some(Instant::now());
                self.ctx.request_repaint();
            }
        }

        let cb = SenderCallback {
            state: state_ref,
            ctx,
        };
        core::send_file(target_ip, 4061, path_str, 4, Box::new(cb));
    }

    fn render_ui(&self, ctx: &egui::Context) {
        let theme = &self.theme;

        // 检查是否需要重置状态（3秒后自动清除）
        {
            let mut state = self.state.lock().unwrap();
            if let Some(reset_time) = state.status_reset_time {
                if reset_time.elapsed() >= Duration::from_secs(3) {
                    state.status_msg = "就绪".to_string();
                    state.progress = 0.0;
                    state.status_reset_time = None;
                } else {
                    // 继续请求重绘直到时间到
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
            }
        }

        // 处理拖拽事件
        self.handle_drag_drop(ctx);

        // 主面板
        egui::CentralPanel::default()
            .frame(Frame::none().fill(theme.bg_primary))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(0.0, 12.0);

                // 顶部标题栏
                self.render_header(ui);

                ui.add_space(8.0);

                // 状态区域
                self.render_status(ui);

                ui.add_space(16.0);

                // 设备列表
                self.render_device_list(ui, ctx);
            });

        // 渲染覆盖层和对话框
        self.render_overlays(ctx);
    }

    fn handle_drag_drop(&self, ctx: &egui::Context) {
        let hovered_files = ctx.input(|i| i.raw.hovered_files.clone());
        let dropped_files = ctx.input(|i| i.raw.dropped_files.clone());

        let mut state = self.state.lock().unwrap();

        // 更新悬浮状态
        state.is_file_hovering = !hovered_files.is_empty();

        // 处理释放的文件
        if !dropped_files.is_empty() {
            let paths: Vec<PathBuf> = dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect();

            if !paths.is_empty() {
                if state.devices.is_empty() {
                    state.status_msg = "⚠ 当前无可用设备，请确保其他设备在线".to_string();
                } else {
                    state.pending_files = paths;
                    state.show_device_picker = true;
                }
            }
        }
    }

    fn render_header(&self, ui: &mut egui::Ui) {
        let theme = &self.theme;
        let my_name = {
            let state = self.state.lock().unwrap();
            state.my_name.clone()
        };

        let mut open_settings = false;
        let mut do_refresh = false;

        Frame::none()
            .fill(theme.bg_secondary)
            .inner_margin(Margin::symmetric(16.0, 12.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // 标题和图标
                    ui.label(
                        RichText::new("📡 LocalSend")
                            .size(20.0)
                            .color(theme.text_primary)
                            .strong(),
                    );

                    ui.add_space(8.0);

                    ui.label(RichText::new(&my_name).size(14.0).color(theme.accent));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // 设置按钮
                        let settings_btn = ui.add(
                            egui::Button::new(
                                RichText::new("⚙").size(18.0).color(theme.text_secondary),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE),
                        );
                        if settings_btn.clicked() {
                            open_settings = true;
                        }

                        // 刷新按钮
                        let refresh_btn = ui.add(
                            egui::Button::new(
                                RichText::new("⟳").size(18.0).color(theme.text_secondary),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE),
                        );
                        if refresh_btn.clicked() {
                            do_refresh = true;
                        }
                    });
                });
            });

        // 处理按钮点击（在闭包外部）
        if open_settings {
            self.state.lock().unwrap().show_settings = true;
        }
        if do_refresh {
            let name = my_name.clone();
            thread::spawn(move || {
                core::send_discover_once(4060, name.clone(), name);
            });
        }
    }

    fn render_status(&self, ui: &mut egui::Ui) {
        let theme = &self.theme;
        let state = self.state.lock().unwrap();

        Frame::none()
            .fill(theme.bg_secondary)
            .rounding(Rounding::same(8.0))
            .inner_margin(Margin::symmetric(16.0, 12.0))
            .outer_margin(Margin::symmetric(16.0, 0.0))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    // 状态消息
                    let status_color = if state.status_msg.starts_with("✓") {
                        theme.success
                    } else if state.status_msg.starts_with("✗") || state.status_msg.starts_with("⚠")
                    {
                        Color32::from_rgb(255, 100, 100)
                    } else {
                        theme.text_secondary
                    };

                    ui.label(
                        RichText::new(&state.status_msg)
                            .size(14.0)
                            .color(status_color),
                    );

                    // 进度条和速度
                    if state.progress > 0.0 || state.is_transferring {
                        ui.add_space(8.0);

                        let progress_safe = state.progress.clamp(0.0, 1.0);
                        let progress_bar = egui::ProgressBar::new(progress_safe)
                            .show_percentage()
                            .animate(state.is_transferring);
                        ui.add(progress_bar);

                        // 显示传输速度
                        if state.is_transferring && state.current_speed > 0.0 {
                            ui.add_space(4.0);
                            let speed_str = format_speed(state.current_speed);
                            let transferred_str = format_bytes(state.transferred_bytes);
                            let total_str = format_bytes(state.total_bytes);
                            ui.label(
                                RichText::new(format!(
                                    "⚡ {} | {} / {}",
                                    speed_str, transferred_str, total_str
                                ))
                                .size(12.0)
                                .color(theme.accent),
                            );
                        }
                    }

                    // 保存位置
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("📁 保存位置: {}", state.save_dir))
                            .size(12.0)
                            .color(theme.text_muted),
                    );
                });
            });
    }

    fn render_device_list(&self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let theme = &self.theme;
        let state = self.state.lock().unwrap();

        // 标题
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(
                RichText::new("在线设备")
                    .size(16.0)
                    .color(theme.text_primary)
                    .strong(),
            );

            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("({})", state.devices.len()))
                    .size(14.0)
                    .color(theme.text_muted),
            );
        });

        ui.add_space(8.0);

        // 设备列表滚动区域
        egui::ScrollArea::vertical()
            .id_source("device_list")
            .show(ui, |ui| {
                if state.devices.is_empty() {
                    // 空状态
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.label(RichText::new("🔍").size(48.0).color(theme.text_muted));
                        ui.add_space(16.0);
                        ui.label(
                            RichText::new("暂无设备")
                                .size(16.0)
                                .color(theme.text_secondary),
                        );
                        ui.label(
                            RichText::new("请确保其他设备与此电脑在同一局域网")
                                .size(12.0)
                                .color(theme.text_muted),
                        );
                        ui.add_space(16.0);
                        ui.label(
                            RichText::new("💡 提示：拖拽文件到此窗口可快速发送")
                                .size(12.0)
                                .color(theme.accent),
                        );
                    });
                } else {
                    // 设备卡片
                    for device in &state.devices {
                        self.render_device_card(ui, device, ctx.clone());
                        ui.add_space(8.0);
                    }
                }
            });
    }

    fn render_device_card(&self, ui: &mut egui::Ui, device: &core::DeviceInfo, ctx: egui::Context) {
        let theme = &self.theme;

        Frame::none()
            .fill(theme.bg_secondary)
            .rounding(Rounding::same(8.0))
            .stroke(Stroke::new(1.0, theme.border))
            .inner_margin(Margin::symmetric(16.0, 12.0))
            .outer_margin(Margin::symmetric(16.0, 0.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // 设备图标
                    let icon = if device.name.to_lowercase().contains("android")
                        || device.name.to_lowercase().contains("phone")
                    {
                        "📱"
                    } else if device.name.to_lowercase().contains("desktop")
                        || device.name.to_lowercase().contains("pc")
                    {
                        "💻"
                    } else {
                        "📟"
                    };

                    ui.label(RichText::new(icon).size(28.0));

                    ui.add_space(12.0);

                    // 设备信息
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&device.name)
                                .size(15.0)
                                .color(theme.text_primary)
                                .strong(),
                        );
                        ui.label(
                            RichText::new(&device.ip)
                                .size(12.0)
                                .color(theme.text_muted)
                                .monospace(),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let send_btn = ui.add(
                            egui::Button::new(
                                RichText::new("📤 发送文件")
                                    .size(13.0)
                                    .color(theme.bg_primary),
                            )
                            .fill(theme.accent)
                            .rounding(Rounding::same(6.0))
                            .min_size(Vec2::new(90.0, 32.0)),
                        );

                        if send_btn.clicked() {
                            let ip = device.ip.clone();
                            let ctx_clone = ctx.clone();
                            // 使用文件选择器
                            if let Some(file) = rfd::FileDialog::new().pick_file() {
                                let state_ref = self.state.clone();
                                let file_name = file
                                    .file_name()
                                    .map(|f| f.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                let path_str = file.to_string_lossy().to_string();

                                {
                                    let mut s = state_ref.lock().unwrap();
                                    s.status_msg = format!("准备发送: {}", file_name);
                                    s.current_filename = file_name;
                                    s.is_transferring = true;
                                    s.progress = 0.0;
                                }

                                struct SenderCallback {
                                    state: Arc<Mutex<AppState>>,
                                    ctx: egui::Context,
                                }
                                impl core::TransferCallback for SenderCallback {
                                    fn on_receive_request(
                                        &self,
                                        _: String,
                                        _: u64,
                                        _: String,
                                    ) -> bool {
                                        true
                                    }
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
                                        s.status_msg = if success {
                                            "✓ 发送成功".into()
                                        } else {
                                            format!("✗ 发送失败: {}", msg)
                                        };
                                        s.progress = if success { 1.0 } else { 0.0 };
                                        s.status_reset_time = Some(Instant::now());
                                        self.ctx.request_repaint();
                                    }
                                }

                                let cb = SenderCallback {
                                    state: state_ref,
                                    ctx: ctx_clone,
                                };
                                core::send_file(ip, 4061, path_str, 4, Box::new(cb));
                            }
                        }
                    });
                });
            });
    }

    fn render_overlays(&self, ctx: &egui::Context) {
        // 拖拽悬浮窗
        {
            let state = self.state.lock().unwrap();
            if state.is_file_hovering {
                drop(state);
                self.render_drag_overlay(ctx);
            }
        }

        // 设备选择对话框
        {
            let state = self.state.lock().unwrap();
            if state.show_device_picker {
                drop(state);
                self.render_device_picker(ctx);
            }
        }

        // 下载完成对话框
        {
            let state = self.state.lock().unwrap();
            if state.show_download_complete {
                drop(state);
                self.render_download_complete(ctx);
            }
        }

        // 设置对话框
        {
            let state = self.state.lock().unwrap();
            if state.show_settings {
                drop(state);
                self.render_settings(ctx);
            }
        }
    }

    fn render_drag_overlay(&self, ctx: &egui::Context) {
        let theme = &self.theme;

        egui::Area::new(egui::Id::new("drag_overlay"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::LEFT_TOP, Vec2::ZERO)
            .show(ctx, |ui| {
                let screen_rect = ctx.screen_rect();

                // 半透明背景
                ui.painter().rect_filled(screen_rect, 0.0, theme.overlay);

                // 中心悬浮窗
                let center = screen_rect.center();
                let card_size = Vec2::new(280.0, 140.0);
                let card_rect = egui::Rect::from_center_size(center, card_size);

                // 卡片背景
                ui.painter()
                    .rect_filled(card_rect, 16.0, theme.bg_secondary);
                ui.painter()
                    .rect_stroke(card_rect, 16.0, Stroke::new(2.0, theme.accent));

                // 图标和文字
                let icon_pos = center - Vec2::new(0.0, 25.0);
                ui.painter().text(
                    icon_pos,
                    egui::Align2::CENTER_CENTER,
                    "📁",
                    egui::FontId::proportional(48.0),
                    theme.text_primary,
                );

                let text_pos = center + Vec2::new(0.0, 30.0);
                ui.painter().text(
                    text_pos,
                    egui::Align2::CENTER_CENTER,
                    "拖拽到此处发送文件",
                    egui::FontId::proportional(16.0),
                    theme.accent,
                );
            });
    }

    fn render_device_picker(&self, ctx: &egui::Context) {
        let theme = &self.theme;

        egui::Window::new("选择目标设备")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                Frame::none()
                    .fill(theme.bg_secondary)
                    .rounding(Rounding::same(12.0))
                    .stroke(Stroke::new(1.0, theme.border))
                    .inner_margin(Margin::same(20.0)),
            )
            .show(ctx, |ui| {
                ui.set_min_width(300.0);

                let state = self.state.lock().unwrap();
                let pending_count = state.pending_files.len();
                let devices = state.devices.clone();
                let pending = state.pending_files.clone();
                drop(state);

                ui.label(
                    RichText::new(format!("即将发送 {} 个文件", pending_count))
                        .size(14.0)
                        .color(theme.text_secondary),
                );

                ui.add_space(16.0);

                if devices.is_empty() {
                    ui.label(
                        RichText::new("⚠ 当前无可用设备")
                            .size(14.0)
                            .color(Color32::from_rgb(255, 180, 100)),
                    );
                } else {
                    for device in &devices {
                        let btn = ui.add(
                            egui::Button::new(
                                RichText::new(format!("📱 {} ({})", device.name, device.ip))
                                    .size(14.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.bg_tertiary)
                            .rounding(Rounding::same(6.0))
                            .min_size(Vec2::new(260.0, 40.0)),
                        );

                        if btn.clicked() {
                            let ip = device.ip.clone();
                            let ctx_clone = ctx.clone();

                            // 发送所有待发送文件
                            for file_path in &pending {
                                self.send_file(ip.clone(), file_path.clone(), ctx_clone.clone());
                            }

                            let mut state = self.state.lock().unwrap();
                            state.show_device_picker = false;
                            state.pending_files.clear();
                        }

                        ui.add_space(8.0);
                    }
                }

                ui.add_space(12.0);

                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let cancel_btn = ui.add(
                            egui::Button::new(
                                RichText::new("取消").size(13.0).color(theme.text_secondary),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0, theme.border))
                            .rounding(Rounding::same(6.0))
                            .min_size(Vec2::new(80.0, 32.0)),
                        );

                        if cancel_btn.clicked() {
                            let mut state = self.state.lock().unwrap();
                            state.show_device_picker = false;
                            state.pending_files.clear();
                        }
                    });
                });
            });
    }

    fn render_download_complete(&self, ctx: &egui::Context) {
        let theme = &self.theme;

        egui::Window::new("下载完成")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                Frame::none()
                    .fill(theme.bg_secondary)
                    .rounding(Rounding::same(12.0))
                    .stroke(Stroke::new(1.0, theme.success))
                    .inner_margin(Margin::same(20.0)),
            )
            .show(ctx, |ui| {
                ui.set_min_width(320.0);

                let state = self.state.lock().unwrap();
                let file_path = state.last_received_file.clone();
                let filename = state.current_filename.clone();
                let avg_speed = state.average_speed;
                drop(state);

                ui.horizontal(|ui| {
                    ui.label(RichText::new("✓").size(24.0).color(theme.success));
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("文件接收成功")
                                .size(16.0)
                                .color(theme.text_primary)
                                .strong(),
                        );
                        ui.label(
                            RichText::new(&filename)
                                .size(13.0)
                                .color(theme.text_secondary),
                        );

                        // 显示平均速度
                        if avg_speed > 0.0 {
                            let speed_str = format_speed(avg_speed);
                            ui.label(
                                RichText::new(format!("平均速度: {}", speed_str))
                                    .size(11.0)
                                    .color(theme.text_muted),
                            );
                        }
                    });
                });

                ui.add_space(16.0);

                ui.horizontal(|ui| {
                    // 打开文件按钮
                    let open_file_btn = ui.add(
                        egui::Button::new(
                            RichText::new("📄 打开文件")
                                .size(13.0)
                                .color(theme.bg_primary),
                        )
                        .fill(theme.accent)
                        .rounding(Rounding::same(6.0))
                        .min_size(Vec2::new(100.0, 32.0)),
                    );

                    if open_file_btn.clicked() {
                        if let Some(ref path) = file_path {
                            #[cfg(target_os = "windows")]
                            {
                                let _ = std::process::Command::new("cmd")
                                    .args(["/c", "start", "", path])
                                    .spawn();
                            }
                        }
                    }

                    ui.add_space(8.0);

                    // 打开文件夹按钮
                    let open_folder_btn = ui.add(
                        egui::Button::new(
                            RichText::new("📁 打开文件夹")
                                .size(13.0)
                                .color(theme.text_primary),
                        )
                        .fill(theme.bg_tertiary)
                        .rounding(Rounding::same(6.0))
                        .min_size(Vec2::new(100.0, 32.0)),
                    );

                    if open_folder_btn.clicked() {
                        if let Some(ref path) = file_path {
                            #[cfg(target_os = "windows")]
                            {
                                let _ = std::process::Command::new("explorer")
                                    .args(["/select,", path])
                                    .spawn();
                            }
                        }
                    }

                    ui.add_space(8.0);

                    // 关闭按钮
                    let close_btn = ui.add(
                        egui::Button::new(
                            RichText::new("✕").size(13.0).color(theme.text_secondary),
                        )
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::new(1.0, theme.border))
                        .rounding(Rounding::same(6.0))
                        .min_size(Vec2::new(32.0, 32.0)),
                    );

                    if close_btn.clicked() {
                        let mut state = self.state.lock().unwrap();
                        state.show_download_complete = false;
                    }
                });
            });
    }

    fn render_settings(&self, ctx: &egui::Context) {
        let theme = &self.theme;

        egui::Window::new("设置")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                Frame::none()
                    .fill(theme.bg_secondary)
                    .rounding(Rounding::same(12.0))
                    .stroke(Stroke::new(1.0, theme.border))
                    .inner_margin(Margin::same(20.0)),
            )
            .show(ctx, |ui| {
                ui.set_min_width(350.0);

                let state = self.state.lock().unwrap();
                let current_save_dir = state.save_dir.clone();
                drop(state);

                ui.label(
                    RichText::new("保存位置")
                        .size(14.0)
                        .color(theme.text_primary)
                        .strong(),
                );

                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    // 当前路径显示
                    Frame::none()
                        .fill(theme.bg_primary)
                        .rounding(Rounding::same(4.0))
                        .inner_margin(Margin::symmetric(8.0, 6.0))
                        .show(ui, |ui| {
                            ui.set_min_width(220.0);
                            ui.label(
                                RichText::new(&current_save_dir)
                                    .size(12.0)
                                    .color(theme.text_secondary)
                                    .monospace(),
                            );
                        });

                    ui.add_space(8.0);

                    // 选择文件夹按钮
                    let choose_btn = ui.add(
                        egui::Button::new(
                            RichText::new("📂 选择")
                                .size(13.0)
                                .color(theme.text_primary),
                        )
                        .fill(theme.bg_tertiary)
                        .rounding(Rounding::same(6.0))
                        .min_size(Vec2::new(70.0, 28.0)),
                    );

                    if choose_btn.clicked() {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            let new_path = folder.to_string_lossy().to_string();
                            let mut state = self.state.lock().unwrap();
                            state.save_dir = new_path;
                        }
                    }
                });

                ui.add_space(20.0);

                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let close_btn = ui.add(
                            egui::Button::new(
                                RichText::new("完成").size(13.0).color(theme.bg_primary),
                            )
                            .fill(theme.accent)
                            .rounding(Rounding::same(6.0))
                            .min_size(Vec2::new(80.0, 32.0)),
                        );

                        if close_btn.clicked() {
                            let mut state = self.state.lock().unwrap();
                            state.show_settings = false;
                        }
                    });
                });
            });
    }
}

impl eframe::App for LocalSendApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render_ui(ctx);
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 600.0])
            .with_min_inner_size([360.0, 400.0]),
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

    // 加载中文字体
    let chinese_font_path = "C:\\Windows\\Fonts\\simhei.ttf";
    if let Ok(bytes) = std::fs::read(chinese_font_path) {
        fonts
            .font_data
            .insert("chinese_font".to_owned(), egui::FontData::from_owned(bytes));
        info!("中文字体加载成功: {}", chinese_font_path);
    } else {
        error!("加载中文字体失败: {}", chinese_font_path);
    }

    // 加载 Emoji 字体
    let emoji_font_path = "C:\\Windows\\Fonts\\seguiemj.ttf";
    if let Ok(bytes) = std::fs::read(emoji_font_path) {
        fonts
            .font_data
            .insert("emoji_font".to_owned(), egui::FontData::from_owned(bytes));
        info!("Emoji 字体加载成功: {}", emoji_font_path);
    } else {
        error!("加载 Emoji 字体失败: {}", emoji_font_path);
    }

    // 设置字体优先级：中文字体 -> Emoji 字体 -> 默认字体
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, "emoji_font".to_owned());
        family.insert(0, "chinese_font".to_owned());
    }

    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        family.insert(0, "emoji_font".to_owned());
        family.insert(0, "chinese_font".to_owned());
    }

    ctx.set_fonts(fonts);
}

fn configure_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();

    // 自定义深色主题
    visuals.window_fill = Color32::from_rgb(22, 33, 62);
    visuals.panel_fill = Color32::from_rgb(26, 26, 46);
    visuals.faint_bg_color = Color32::from_rgb(15, 52, 96);
    visuals.extreme_bg_color = Color32::from_rgb(10, 10, 20);

    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(22, 33, 62);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(15, 52, 96);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0, 180, 220);
    visuals.widgets.active.bg_fill = Color32::from_rgb(0, 217, 255);

    visuals.selection.bg_fill = Color32::from_rgb(0, 217, 255);
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);

    ctx.set_visuals(visuals);
}

/// 格式化速度为人类可读的字符串
fn format_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1_000_000_000.0 {
        format!("{:.1} GB/s", bytes_per_sec / 1_000_000_000.0)
    } else if bytes_per_sec >= 1_000_000.0 {
        format!("{:.1} MB/s", bytes_per_sec / 1_000_000.0)
    } else if bytes_per_sec >= 1_000.0 {
        format!("{:.1} KB/s", bytes_per_sec / 1_000.0)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

/// 格式化字节数为人类可读的字符串
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{} B", bytes)
    }
}
