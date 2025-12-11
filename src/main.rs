// src/main.rs
//
// DHJC ARC MONITOR - Rust GUI
//
// 顶部：两行
//   行1：LOGO + RUN
//   行2：右对齐： [TOP] [Logs] [Rate] [Connect/Disconnect] [Send R] [Mode + 参数]
// 中间：左侧 4 卡片，右侧 Total 曲线 + ARC 灯
// 底部：Live 一行 + Event Log (不含 Live 行)

mod dhjc_core;

use crate::dhjc_core::{Change, CoreState};
use chrono::Local;
use eframe::{egui, NativeOptions};
use egui::viewport::{ViewportCommand, WindowLevel};
use egui::{Align, Color32, FontFamily, FontId, Layout, TextStyle};
use egui_plot::{Legend, Line, Plot, PlotPoints};
use serde::Deserialize;
use std::fs::{self, create_dir_all, File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

// ================= 配置 =================

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    port_name: Option<String>,
    baud_rate: Option<u32>,
    log_folder: Option<String>,
    use_tcp: Option<bool>,
    tcp_host: Option<String>,
    tcp_port: Option<u16>,
}

#[derive(Debug, Clone)]
struct AppConfig {
    port_name: String,
    baud_rate: u32,
    log_folder: String,
    use_tcp: bool,
    tcp_host: String,
    tcp_port: u16,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port_name: "COM3".to_string(),
            baud_rate: 115_200,
            log_folder: "logs".to_string(),
            use_tcp: false,
            tcp_host: "127.0.0.1".to_string(),
            tcp_port: 5000,
        }
    }
}

impl AppConfig {
    fn load() -> Self {
        let default_cfg = AppConfig::default();
        let path = Path::new("dhjc_config.toml");

        if !path.exists() {
            let sample = r#"# DHJC Rust GUI 配置文件

# 串口模式
port_name = "COM3"
baud_rate = 115200

# TCP 模式
use_tcp  = false
tcp_host = "127.0.0.1"
tcp_port = 5000

# 日志目录
log_folder = "logs"
"#;
            let _ = fs::write(path, sample);
            println!("[CFG] 未找到 dhjc_config.toml，已生成示例配置文件，使用默认。");
            return default_cfg;
        }

        let content = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[CFG] 读取 dhjc_config.toml 失败: {:?}，使用默认。", e);
                return default_cfg;
            }
        };

        let raw: RawConfig = match toml::from_str(&content) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[CFG] 解析 dhjc_config.toml 失败: {:?}，使用默认。", e);
                return default_cfg;
            }
        };

        let mut cfg = default_cfg;
        if let Some(p) = raw.port_name {
            cfg.port_name = p;
        }
        if let Some(b) = raw.baud_rate {
            cfg.baud_rate = b;
        }
        if let Some(f) = raw.log_folder {
            cfg.log_folder = f;
        }
        if let Some(u) = raw.use_tcp {
            cfg.use_tcp = u;
        }
        if let Some(h) = raw.tcp_host {
            cfg.tcp_host = h;
        }
        if let Some(p) = raw.tcp_port {
            cfg.tcp_port = p;
        }

        println!(
            "[CFG] 使用配置: mode={} port={} baud={} tcp={} log_folder={}",
            if cfg.use_tcp { "TCP" } else { "Serial" },
            cfg.port_name,
            cfg.baud_rate,
            format!("{}:{}", cfg.tcp_host, cfg.tcp_port),
            cfg.log_folder
        );
        cfg
    }
}

// ================= 日志写入 =================

struct LogWriter {
    base_folder: String,
    current_date: String,
    file: Option<File>,
}

impl LogWriter {
    fn new(base_folder: &str) -> Self {
        let now = Local::now();
        let date = now.format("%Y-%m-%d").to_string();
        let file = Self::open_file_for_date(base_folder, &date);
        Self {
            base_folder: base_folder.to_string(),
            current_date: date,
            file,
        }
    }

    fn rotate_if_needed(&mut self) {
        let now = Local::now();
        let today = now.format("%Y-%m-%d").to_string();
        if today != self.current_date {
            self.current_date = today.clone();
            self.file = Self::open_file_for_date(&self.base_folder, &today);
        }
    }

    fn open_file_for_date(base_folder: &str, date_str: &str) -> Option<File> {
        let log_dir = Path::new(base_folder);
        if let Err(e) = create_dir_all(log_dir) {
            eprintln!("[LOG] 创建日志目录失败: {:?}", e);
            return None;
        }

        let filename = format!("{}.txt", date_str);
        let path = log_dir.join(filename);

        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                println!("[LOG] 当前日志文件: {}", path.to_string_lossy());
                Some(f)
            }
            Err(e) => {
                eprintln!("[LOG] 打开日志文件失败: {:?}", e);
                None
            }
        }
    }

    fn write_line(&mut self, content: &str) {
        self.rotate_if_needed();

        let file = match self.file.as_mut() {
            Some(f) => f,
            None => return,
        };

        // tiny_dhjc.cpp 的 noTs 逻辑
        let mut no_ts = false;
        if content.is_empty() {
            no_ts = true;
        } else {
            let mut chars = content.chars();
            if let Some(c0) = chars.next() {
                if c0 == '*' || c0 == '-' || c0 == '=' {
                    no_ts = true;
                }
            }
            if content.contains("SYSTEM") {
                no_ts = true;
            }
        }

        let line_to_write = if no_ts {
            format!("{}\r\n", content)
        } else {
            let now = Local::now();
            let ts = now.format("%H:%M:%S").to_string();
            format!("[{}] {}\r\n", ts, content)
        };

        if let Err(e) = file.write_all(line_to_write.as_bytes()) {
            eprintln!("[LOG] 写入日志失败: {:?}", e);
        } else {
            let _ = file.flush();
        }
    }
}

// ================= IO 线程 =================

fn spawn_serial_thread(
    port_name: String,
    baud_rate: u32,
    tx_line: Sender<String>,
    rx_cmd: Receiver<String>,
) {
    thread::spawn(move || {
        let mut port = match serialport::new(port_name.clone(), baud_rate)
            .timeout(Duration::from_millis(100))
            .open()
        {
            Ok(p) => p,
            Err(e) => {
                let _ = tx_line.send(format!("[ERROR] 打开串口失败: {:?}", e));
                return;
            }
        };

        let mut buf = [0u8; 1024];
        let mut line_buf = String::new();

        loop {
            // 发命令
            match rx_cmd.try_recv() {
                Ok(cmd) => {
                    if let Err(e) = port.write_all(cmd.as_bytes()) {
                        let _ = tx_line.send(format!("[ERROR] 串口发送失败: {:?}", e));
                        break;
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => break,
            }

            // 读串口
            match port.read(&mut buf) {
                Ok(n) if n > 0 => {
                    for &b in &buf[..n] {
                        if b == b'\r' || b == b'\n' {
                            let line = line_buf.trim_end().to_string();
                            line_buf.clear();
                            if !line.is_empty() {
                                if tx_line.send(line).is_err() {
                                    return;
                                }
                            }
                        } else {
                            line_buf.push(b as char);
                        }
                    }
                }
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    let _ = tx_line.send(format!("[ERROR] 串口读取失败: {:?}", e));
                    break;
                }
            }
        }
    });
}

fn spawn_tcp_thread(
    host: String,
    port: u16,
    tx_line: Sender<String>,
    rx_cmd: Receiver<String>,
) {
    thread::spawn(move || {
        let addr = format!("{}:{}", host, port);
        let mut stream = match TcpStream::connect(&addr) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx_line.send(format!("[ERROR] 连接 TCP {} 失败: {:?}", addr, e));
                return;
            }
        };

        if let Err(e) = stream.set_read_timeout(Some(Duration::from_millis(100))) {
            let _ = tx_line.send(format!("[WARN] 设置 TCP 读超时失败: {:?}", e));
        }

        let mut buf = [0u8; 1024];
        let mut line_buf = String::new();

        loop {
            match rx_cmd.try_recv() {
                Ok(cmd) => {
                    if let Err(e) = stream.write_all(cmd.as_bytes()) {
                        let _ = tx_line.send(format!("[ERROR] TCP 发送失败: {:?}", e));
                        break;
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => break,
            }

            match stream.read(&mut buf) {
                Ok(n) if n > 0 => {
                    for &b in &buf[..n] {
                        if b == b'\r' || b == b'\n' {
                            let line = line_buf.trim_end().to_string();
                            line_buf.clear();
                            if !line.is_empty() {
                                if tx_line.send(line).is_err() {
                                    return;
                                }
                            }
                        } else {
                            line_buf.push(b as char);
                        }
                    }
                }
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    let _ = tx_line.send(format!("[ERROR] TCP 读取失败: {:?}", e));
                    break;
                }
            }
        }
    });
}

// ================= GUI 状态 =================

enum ConnectionStatus {
    Disconnected,
    Connected,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionMode {
    Serial,
    Tcp,
}

struct DhjcApp {
    cfg: AppConfig,
    logger: LogWriter,
    core: CoreState,

    status: ConnectionStatus,
    mode: ConnectionMode,

    serial_port_text: String,
    serial_baud_text: String,
    tcp_host_text: String,
    tcp_port_text: String,

    line_rx: Option<Receiver<String>>,
    cmd_tx: Option<Sender<String>>,

    log_lines: Vec<String>,       // 不含 Live
    max_log_lines: usize,
    last_live_line: Option<String>, // 单独显示 Live

    last_error: Option<String>,

    start_time: Instant,
    plot_points: Vec<[f64; 2]>,
    max_plot_points: usize,

    last_pulse_time: Option<Instant>,
    always_on_top: bool,
}

impl DhjcApp {
    fn new(cc: &eframe::CreationContext<'_>, cfg: AppConfig) -> Self {
        let ctx = &cc.egui_ctx;
        ctx.set_visuals(egui::Visuals::dark());
        ctx.set_pixels_per_point(1.4);

        let mut style = (*ctx.style()).clone();
        style.text_styles.insert(
            TextStyle::Body,
            FontId::new(18.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(24.0, FontFamily::Proportional),
        );
        ctx.set_style(style);

        let mode = if cfg.use_tcp {
            ConnectionMode::Tcp
        } else {
            ConnectionMode::Serial
        };

        Self {
            cfg: cfg.clone(),
            logger: LogWriter::new(&cfg.log_folder),
            core: CoreState::new(),
            status: ConnectionStatus::Disconnected,
            mode,
            serial_port_text: cfg.port_name.clone(),
            serial_baud_text: cfg.baud_rate.to_string(),
            tcp_host_text: cfg.tcp_host.clone(),
            tcp_port_text: cfg.tcp_port.to_string(),
            line_rx: None,
            cmd_tx: None,
            log_lines: Vec::new(),
            max_log_lines: 1000,
            last_live_line: None,
            last_error: None,
            start_time: Instant::now(),
            plot_points: Vec::new(),
            max_plot_points: 2000,
            last_pulse_time: None,
            always_on_top: false,
        }
    }

    fn connect(&mut self) {
        if let ConnectionStatus::Connected = self.status {
            return;
        }

        let (tx_line, rx_line) = mpsc::channel::<String>();
        let (tx_cmd, rx_cmd) = mpsc::channel::<String>();

        match self.mode {
            ConnectionMode::Serial => {
                let port_name = self.serial_port_text.trim().to_string();
                if port_name.is_empty() {
                    self.last_error = Some("请先输入串口号".to_string());
                    return;
                }
                let baud = self
                    .serial_baud_text
                    .trim()
                    .parse::<u32>()
                    .unwrap_or(self.cfg.baud_rate);
                spawn_serial_thread(port_name, baud, tx_line, rx_cmd);
            }
            ConnectionMode::Tcp => {
                let host = self.tcp_host_text.trim().to_string();
                if host.is_empty() {
                    self.last_error = Some("请先输入 TCP 地址".to_string());
                    return;
                }
                let port = self
                    .tcp_port_text
                    .trim()
                    .parse::<u16>()
                    .unwrap_or(self.cfg.tcp_port);
                spawn_tcp_thread(host, port, tx_line, rx_cmd);
            }
        }

        self.line_rx = Some(rx_line);
        self.cmd_tx = Some(tx_cmd);
        self.status = ConnectionStatus::Connected;
        self.last_error = None;
    }

    fn disconnect(&mut self) {
        self.status = ConnectionStatus::Disconnected;
        self.line_rx = None;
        self.cmd_tx = None;
    }

    fn send_reset(&mut self) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send("R\n".to_string());
        }
    }

    fn open_logs_folder(&self) {
        let folder = self.cfg.log_folder.clone();

        #[cfg(target_os = "windows")]
        {
            let _ = Command::new("explorer").arg(folder).spawn();
        }
        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("xdg-open").arg(folder).spawn();
        }
        #[cfg(target_os = "macos")]
        {
            let _ = Command::new("open").arg(folder).spawn();
        }
    }

    fn rate_hz(&self) -> f64 {
        if self.core.active_time_s > 0.0 {
            self.core.current_total as f64 / self.core.active_time_s
        } else {
            0.0
        }
    }

    fn handle_incoming_line(&mut self, line: &str) {
        if line.starts_with("[ERROR]") {
            self.last_error = Some(line.to_string());
        }

        let is_live = line.contains("[Live]") || line.contains("[LIVE]");

        if is_live {
            // Live 单独显示，不写日志文件、不进 Event Log
            self.last_live_line = Some(line.to_string());
        } else {
            self.log_lines.push(line.to_string());
            if self.log_lines.len() > self.max_log_lines {
                let overflow = self.log_lines.len() - self.max_log_lines;
                self.log_lines.drain(0..overflow);
            }
            self.logger.write_line(line);
        }

        let change: Change = self.core.process_line(line);

        if change.total_changed {
            let t = self.start_time.elapsed().as_secs_f64();
            self.plot_points.push([t, self.core.current_total as f64]);
            if self.plot_points.len() > self.max_plot_points {
                let overflow = self.plot_points.len() - self.max_plot_points;
                self.plot_points.drain(0..overflow);
            }
            self.last_pulse_time = Some(Instant::now());
        }
    }

    fn ui_stats_panel(&self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("监控概览").strong());
        ui.add_space(8.0);

        self.stat_card(ui, "Stage", self.core.stage.to_string(), "");
        ui.add_space(8.0);

        self.stat_card(
            ui,
            "Total Count",
            self.core.current_total.to_string(),
            "pulses",
        );
        ui.add_space(8.0);

        self.stat_card(
            ui,
            "Active Time",
            format!("{:.3}", self.core.active_time_s),
            "s",
        );
        ui.add_space(8.0);

        let ts = self.core.last_timestamp.as_deref().unwrap_or("N/A");
        self.stat_card(ui, "Last Update", ts.to_string(), "");
    }

    fn stat_card(&self, ui: &mut egui::Ui, title: &str, value: String, unit: &str) {
        let bg = Color32::from_rgb(30, 34, 60);
        let accent = Color32::from_rgb(120, 200, 255);

        const CARD_W: f32 = 260.0;
        const CARD_H: f32 = 110.0;

        // 真·固定尺寸：先分配矩形，再在里面画 Frame
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(CARD_W, CARD_H), egui::Sense::hover());

        ui.allocate_ui_at_rect(rect, |ui| {
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                egui::Frame::none()
                    .fill(bg)
                    .rounding(egui::Rounding::same(10))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(title)
                                .size(20.0)
                                .color(accent),
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(value)
                                    .size(36.0)
                                    .strong(),
                            );
                            if !unit.is_empty() {
                                ui.label(
                                    egui::RichText::new(unit)
                                        .size(20.0)
                                        .weak(),
                                );
                            }
                        });
                    });
            });
        });
    }

    fn draw_led(&self, ui: &mut egui::Ui, label: &str, on: bool, color_on: Color32) {
        let color_off = Color32::from_gray(70);
        let color = if on { color_on } else { color_off };

        ui.vertical(|ui| {
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 8.0, color);
            ui.add_space(2.0);
            ui.label(egui::RichText::new(label).size(10.0));
        });
    }
}

impl eframe::App for DhjcApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 收后台数据
        if self.line_rx.is_some() {
            // take the receiver out so we don't hold an immutable borrow of self
            let mut rx = self.line_rx.take().unwrap();
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(line) => self.handle_incoming_line(&line),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.status = ConnectionStatus::Disconnected;
                        self.cmd_tx = None;
                        disconnected = true;
                        break;
                    }
                }
            }
            // put the receiver back if still connected
            if !disconnected {
                self.line_rx = Some(rx);
            } else {
                self.line_rx = None;
            }
        }

        ctx.request_repaint_after(Duration::from_millis(50));

        // 顶部两行
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            // 行1：LOGO + RUN
            ui.horizontal(|ui| {
                ui.heading("DHJC ARC MONITOR");
                ui.add_space(12.0);

                let blink_on = matches!(self.status, ConnectionStatus::Connected)
                    && (self.start_time.elapsed().as_millis() / 500) % 2 == 0;
                self.draw_led(ui, "RUN", blink_on, Color32::from_rgb(50, 220, 120));
            });

            ui.add_space(6.0);

            // 行2：右对齐
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // TOP 按钮
                let top_fill = if self.always_on_top {
                    Color32::from_rgb(255, 210, 80)
                } else {
                    Color32::from_gray(90)
                };
                let top_text = if self.always_on_top { "TOP" } else { "TOP" };
                let top_btn = egui::Button::new(
                    egui::RichText::new(top_text)
                        .strong()
                        .color(Color32::BLACK),
                )
                .fill(top_fill);
                if ui.add(top_btn).clicked() {
                    self.always_on_top = !self.always_on_top;
                    let level = if self.always_on_top {
                        WindowLevel::AlwaysOnTop
                    } else {
                        WindowLevel::Normal
                    };
                    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(level));
                }

                ui.add_space(6.0);
                if ui.button("Logs...").clicked() {
                    self.open_logs_folder();
                }

                ui.add_space(10.0);
                ui.label(format!("Rate: {:.2} Hz", self.rate_hz()));

                ui.add_space(10.0);

                // Connect / Disconnect 一个按钮搞定
                let (btn_text, btn_color) = match self.status {
                    ConnectionStatus::Disconnected => (
                        "Connect",
                        Color32::from_rgb(80, 200, 120),
                    ),
                    ConnectionStatus::Connected => (
                        "Disconnect",
                        Color32::from_rgb(220, 80, 80),
                    ),
                };
                let conn_btn = egui::Button::new(
                    egui::RichText::new(btn_text)
                        .strong()
                        .color(Color32::BLACK),
                )
                .fill(btn_color);

                if ui.add(conn_btn).clicked() {
                    match self.status {
                        ConnectionStatus::Disconnected => self.connect(),
                        ConnectionStatus::Connected => self.disconnect(),
                    }
                }

                ui.add_space(6.0);
                if ui.button("Send R").clicked() {
                    self.send_reset();
                }

                ui.add_space(12.0);

                // Mode + 参数放最左（因为 right_to_left）
                match self.mode {
                    ConnectionMode::Serial => {
                        ui.horizontal(|ui| {
                            ui.label("Baud:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.serial_baud_text)
                                    .desired_width(80.0),
                            );
                            ui.label("Port:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.serial_port_text)
                                    .desired_width(80.0),
                            );
                            ui.label("Mode:");
                            egui::ComboBox::from_id_source("mode_combo")
                                .selected_text("Serial")
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.mode,
                                        ConnectionMode::Serial,
                                        "Serial",
                                    );
                                    ui.selectable_value(
                                        &mut self.mode,
                                        ConnectionMode::Tcp,
                                        "TCP",
                                    );
                                });
                        });
                    }
                    ConnectionMode::Tcp => {
                        ui.horizontal(|ui| {
                            ui.label("Port:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.tcp_port_text)
                                    .desired_width(70.0),
                            );
                            ui.label("Host:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.tcp_host_text)
                                    .desired_width(130.0),
                            );
                            ui.label("Mode:");
                            egui::ComboBox::from_id_source("mode_combo")
                                .selected_text("TCP")
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.mode,
                                        ConnectionMode::Serial,
                                        "Serial",
                                    );
                                    ui.selectable_value(
                                        &mut self.mode,
                                        ConnectionMode::Tcp,
                                        "TCP",
                                    );
                                });
                        });
                    }
                }
            });

            if let Some(err) = &self.last_error {
                ui.add_space(4.0);
                ui.colored_label(Color32::RED, err);
            }
        });

        // 中间：统计 + 曲线
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                // 左：卡片列
                ui.vertical(|ui| {
                    ui.set_min_width(280.0);
                    self.ui_stats_panel(ui);
                });

                ui.separator();

                // 右：曲线 + ARC 灯
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Total Timeline").strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let arc_on = self
                                .last_pulse_time
                                .map(|t| t.elapsed() < Duration::from_millis(800))
                                .unwrap_or(false);
                            self.draw_led(
                                ui,
                                "ARC",
                                arc_on,
                                Color32::from_rgb(230, 80, 80),
                            );
                        });
                    });

                    ui.add_space(4.0);

                    Plot::new("pulse_plot")
                        .height(260.0)
                        .legend(Legend::default())
                        .show(ui, |plot_ui| {
                            if !self.plot_points.is_empty() {
                                let points: PlotPoints =
                                    PlotPoints::from(self.plot_points.clone());
                                let line = Line::new("Total", points)
                                    .color(Color32::from_rgb(120, 200, 255))
                                    .width(2.0);
                                plot_ui.line(line);
                            }
                        });
                });
            });
        });

        // 底部：Live + Event Log
        egui::TopBottomPanel::bottom("log_panel")
            .resizable(true)
            .default_height(220.0)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Live:").strong());
                let live_text = self
                    .last_live_line
                    .as_deref()
                    .unwrap_or("Waiting for signal pulses...");
                ui.label(egui::RichText::new(live_text).monospace());
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                ui.label(egui::RichText::new("Event Log").strong());
                ui.add_space(4.0);

                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log_lines {
                            ui.label(egui::RichText::new(line).monospace());
                        }
                    });
            });
    }
}

// ================= main =================

fn main() -> eframe::Result<()> {
    let cfg = AppConfig::load();

    let native_options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(1200.0, 750.0))
            .with_min_inner_size(egui::vec2(900.0, 600.0))
            .with_title("DHJC ARC MONITOR - Rust GUI"),
        ..Default::default()
    };

    eframe::run_native(
        "dhjc_rust_gui",
        native_options,
        Box::new(move |cc| Ok(Box::new(DhjcApp::new(cc, cfg.clone())))),
    )
}
