// src/main.rs
//
// DHJC ARC MONITOR - Rust GUI
//
// 顶部：两行
//   行1：LOGO (左) | 📌TOP + Logs... (右，Logs 在最右)
//   行2：左：Mode/Serial/TCP/Port/... + Connect/Reset
//        右：RUN 小圆灯
// 中间：左侧 SidePanel：DATA TEMPLATE + 四个卡片（可滚动）
//       右侧 CentralPanel：Live + Total Timeline（右上角 Rate）+ 曲线
// 底部：Event Log（不可拖动分隔线）

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

        // 标题行 / 分隔线不加时间戳
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

#[derive(Clone, Copy, PartialEq, Eq)]
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

    log_lines: Vec<String>,        // 不含 Live
    max_log_lines: usize,
    last_live_line: Option<String>, // 单独显示 Live

    last_error: Option<String>,

    start_time: Instant,
    plot_points: Vec<[f64; 2]>,
    max_plot_points: usize,

    last_pulse_time: Option<Instant>,
    last_stage_for_plot: i32,
    always_on_top: bool,
    last_wait_ms: Option<f64>,
    prev_wait_ms: Option<f64>,
    log_filter: String,
}

impl DhjcApp {

    fn full_reset(&mut self) {
         // ✅ 保留日志内容，不清空 log_lines
        // ✅ 写入分隔线作为提示
        self.logger.write_line("===== SYSTEM RESET =====");
        self.log_lines.push("===== SYSTEM RESET =====".to_string());

        // ✅ 重置内部计数与绘图
        self.core = CoreState::new();
        self.plot_points.clear();

        // ✅ 清除速率状态
        self.last_wait_ms = None;
        self.prev_wait_ms = None;
        self.last_pulse_time = None;
        self.last_stage_for_plot = -1;

        // ✅ 重置时间基准
        self.start_time = Instant::now();

        // ✅ 清空仅 UI 层的状态
        self.last_live_line = None;
        self.last_error = None;

        // ✅ 日志写分隔线（视觉提示）
        self.logger.write_line("===== SYSTEM RESET =====");
        self.log_lines.push("===== SYSTEM RESET =====".to_string());

        //
        self.prev_wait_ms = None;

    }

    fn parse_wait_ms_from_live(line: &str) -> Option<f64> {
        // 在字符串中找 "Wait:"
        let tag = "Wait:";
        let idx = line.find(tag)?;
        let rest = &line[idx + tag.len()..];

        // 从 "xxxxx ms" 里抠出数字
        let end = rest.find("ms")?;
        let num_str = rest[..end].trim();
        num_str.parse::<f64>().ok()
    }

    fn new(cc: &eframe::CreationContext<'_>, cfg: AppConfig) -> Self {
        let ctx = &cc.egui_ctx;
        ctx.set_visuals(egui::Visuals::light());
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
            prev_wait_ms: None,
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
            last_stage_for_plot: -1,
            always_on_top: false,
            last_wait_ms: None,

            log_filter: String::new(),
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
        if let Some(wait_ms) = self.last_wait_ms {
            if wait_ms.is_finite() && wait_ms > 0.0 {
                let rate = 1000.0 / wait_ms;
                // 限制到合理范围
                if (1.0..=1000.0).contains(&rate) {
                    rate
                } else {
                    0.0
                }
            } else {
                0.0
            }
        } else {
            0.0
        }
    }

    fn handle_incoming_line(&mut self, line: &str) {
        // 处理错误行
        if line.starts_with("[ERROR]") {
            self.last_error = Some(line.to_string());
        }

        let is_live = line.contains("[Live]") || line.contains("[LIVE]");

        if is_live {
            // ✅ Live 行：实时显示 + 更新频率
            self.last_live_line = Some(line.to_string());

            if let Some(wait_ms) = Self::parse_wait_ms_from_live(line) {
                // 第一次解析，先存，不立刻显示
                if self.prev_wait_ms.is_none() {
                    self.prev_wait_ms = Some(wait_ms);
                    self.last_wait_ms = None;
                } else {
                    let avg = (self.prev_wait_ms.unwrap() + wait_ms) / 2.0;
                    self.last_wait_ms = Some(avg);
                    self.prev_wait_ms = Some(wait_ms);
                }
            }
        } else {
            // ✅ 非 Live 行：推送到日志
            self.log_lines.push(line.to_string());
            if self.log_lines.len() > self.max_log_lines {
                let overflow = self.log_lines.len() - self.max_log_lines;
                self.log_lines.drain(0..overflow);
            }
            self.logger.write_line(line);
        }

        // ✅ 更新总数与绘图逻辑
        let prev_total = self.core.current_total;
        let change: Change = self.core.process_line(line);

        if self.core.current_total > prev_total {
            self.last_pulse_time = Some(Instant::now());
        }

        if change.total_changed {
            let t = self.start_time.elapsed().as_secs_f64();

            if self.core.current_total < prev_total {
                return;
            }

            const GAP_THRESHOLD: f64 = 5.0;
            let mut gap_too_long = false;

            if let Some(last_time) = self.last_pulse_time {
                let gap = last_time.elapsed().as_secs_f64();
                if gap > GAP_THRESHOLD {
                    gap_too_long = true;
                }
            }

            if gap_too_long {
                self.plot_points.push([t, f64::NAN]);
            }

            self.plot_points.push([t, self.core.current_total as f64]);

            if self.plot_points.len() > self.max_plot_points {
                let overflow = self.plot_points.len() - self.max_plot_points;
                self.plot_points.drain(0..overflow);
            }

            self.last_pulse_time = Some(Instant::now());
        }
    }


    // 顶部 RUN 小灯
    fn draw_run_led(&self, ui: &mut egui::Ui, on: bool) {
        let size = 15.0;
        let color_on = Color32::from_rgb(50, 220, 120);
        let color_off = Color32::from_gray(70);
        let color = if on { color_on } else { color_off };

        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), size / 2.0, color);
    }

    // 左侧卡片区域
        fn ui_stats_panel(&self, ui: &mut egui::Ui) {
        // 左侧区域大致宽度
        ui.set_width(220.0);

        // 顶部 DATA TEMPLATE：在左侧区域内水平居中 + 加粗
        ui.add_space(4.0);
        ui.columns(3, |cols| {
            cols[1].label(
                egui::RichText::new("DATA TEMPLATE")
                    .size(13.0)
                    .strong(),
            );
        });
        ui.add_space(2.0);
        ui.add(egui::Separator::default());
        ui.add_space(4.0);

        // 下面四张卡片保持不变
        self.stat_card(ui, "Stage", self.core.stage.to_string(), "");
        ui.add_space(6.0);

        self.stat_card(
            ui,
            "Total Count",
            self.core.current_total.to_string(),
            "pulses",
        );
        ui.add_space(6.0);

        self.stat_card(
            ui,
            "Active Time",
            format!("{:.3}", self.core.active_time_s),
            "s",
        );
        ui.add_space(6.0);

        let ts = self.core.last_timestamp.as_deref().unwrap_or("N/A");
        self.stat_card(ui, "Last Update", ts.to_string(), "");
    }






    fn stat_card(&self, ui: &mut egui::Ui, title: &str, value: String, unit: &str) {
        let bg = Color32::from_rgb(235, 239, 245);
        let title_color = Color32::from_rgb(40, 40, 60);
        let value_color = Color32::from_rgb(10, 10, 30);
        let unit_color = Color32::from_rgb(90, 90, 120);

        // 不用 Margin::symmetric，直接手写结构体，避免编译器提示 arguments incorrect
        let margin = egui::Margin {
            left: 8,
            right: 8,
            top: 4,     // 上边距很小
            bottom: 4,
        };

        egui::Frame::none()
            .fill(bg)
            .rounding(egui::Rounding::same(10))
            .inner_margin(margin)
            .show(ui, |ui| {
                // 卡片本身的“有效宽度”，再瘦一点
                ui.set_width(200.0);

                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(title)
                            .size(16.0)
                            .color(title_color)
                            .strong(),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(value.clone())
                            .size(26.0)
                            .color(value_color)
                            .strong(),
                    );
                    if !unit.is_empty() {
                        ui.add_space(1.0);
                        ui.label(
                            egui::RichText::new(unit)
                                .size(14.0)
                                .color(unit_color),
                        );
                    }
                });
            });
    }

}


impl eframe::App for DhjcApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. 先收后台数据
        // 临时取出
        let mut temp_rx = None;
        if let Some(rx) = self.line_rx.take() {
            temp_rx = Some(rx);
        }

        if let Some(rx) = &mut temp_rx {
            loop {
                match rx.try_recv() {
                    Ok(line) => {
                        self.handle_incoming_line(&line); // 可安全使用 &mut self
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.status = ConnectionStatus::Disconnected;
                        self.cmd_tx = None;
                        break;
                    }
                }
            }
        }

        // 放回去
        if self.status == ConnectionStatus::Connected {
        self.line_rx = temp_rx;
        }
        ctx.request_repaint_after(Duration::from_millis(50));

        // 2. 顶部两行
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            // 行1：LOGO + TOP + Logs
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("DHJC ARC MONITOR")
                        .size(26.0)
                        .strong(),
                );

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Logs...").clicked() {
                        self.open_logs_folder();
                    }

                    ui.add_space(6.0);

                    let top_fill = if self.always_on_top {
                        Color32::from_rgb(255, 210, 80)
                    } else {
                        Color32::from_gray(90)
                    };
                    let top_label = egui::RichText::new("📌 TOP")
                        .strong()
                        .color(Color32::BLACK);
                    let top_btn = egui::Button::new(top_label).fill(top_fill);
                    if ui.add(top_btn).clicked() {
                        self.always_on_top = !self.always_on_top;
                        let level = if self.always_on_top {
                            WindowLevel::AlwaysOnTop
                        } else {
                            WindowLevel::Normal
                        };
                        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(level));
                    }
                });
            });

            ui.add_space(6.0);

            // 行2：左参数 + Connect/Reset，右 RUN 灯
            let blink_on = matches!(self.status, ConnectionStatus::Connected)
                && (self.start_time.elapsed().as_millis() / 500) % 2 == 0;

            ui.columns(2, |cols| {
            let is_connected = matches!(self.status, ConnectionStatus::Connected);

            // 左列：配置 + Connect/Reset
            cols[0].horizontal(|ui| {
                // 这一块配置在连接后变灰，不可编辑
                ui.add_enabled_ui(!is_connected, |ui| {
                    // Mode: 加粗
                    ui.label(egui::RichText::new("Mode:").strong());
                    egui::ComboBox::from_id_source("mode_combo")
                        .selected_text(match self.mode {
                            ConnectionMode::Serial => "Serial",
                            ConnectionMode::Tcp => "TCP",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.mode, ConnectionMode::Serial, "Serial");
                            ui.selectable_value(&mut self.mode, ConnectionMode::Tcp, "TCP");
                        });

                    ui.add_space(8.0);

                    match self.mode {
                        ConnectionMode::Serial => {
                            // 串口：Port / Baud
                            ui.label(egui::RichText::new("Port:").strong());
                            ui.add(
                                egui::TextEdit::singleline(&mut self.serial_port_text)
                                    .desired_width(80.0)
                                    .horizontal_align(egui::Align::Center),
                            );

                            ui.add_space(8.0);

                            ui.label(egui::RichText::new("Baud:").strong());
                            ui.add(
                                egui::TextEdit::singleline(&mut self.serial_baud_text)
                                    .desired_width(80.0)
                                    .horizontal_align(egui::Align::Center),
                            );
                        }
                        ConnectionMode::Tcp => {
                            // TCP：Host / Port
                            ui.label(egui::RichText::new("Host:").strong());
                            ui.add(
                                egui::TextEdit::singleline(&mut self.tcp_host_text)
                                    .desired_width(110.0)
                                    .horizontal_align(egui::Align::Center),
                            );

                            ui.add_space(8.0);

                            ui.label(egui::RichText::new("Port:").strong());
                            ui.add(
                                egui::TextEdit::singleline(&mut self.tcp_port_text)
                                    .desired_width(80.0)
                                    .horizontal_align(egui::Align::Center),
                            );
                        }
                    }
                });

                ui.add_space(16.0);

                // Connect / Disconnect 按钮始终可点
                let (btn_text, btn_color) = match self.status {
                    ConnectionStatus::Disconnected => ("Connect", Color32::from_rgb(80, 200, 120)),
                    ConnectionStatus::Connected => ("Disconnect", Color32::from_rgb(220, 80, 80)),
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

                ui.add_space(8.0);
                if ui.button("Reset").clicked() {
                    self.send_reset();
                    self.full_reset(); 
                }
            });

                // 右列：RUN 灯不动
                cols[1].with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let blink_on = matches!(self.status, ConnectionStatus::Connected)
                        && (self.start_time.elapsed().as_millis() / 500) % 2 == 0;
                    self.draw_run_led(ui, blink_on);
                });
            });

            if let Some(err) = &self.last_error {
                ui.add_space(4.0);
                ui.colored_label(Color32::RED, err);
            }
        });

        // 3. 底部 Event Log（固定）
        egui::TopBottomPanel::bottom("log_panel")
        .resizable(false)
        .default_height(200.0)
        .min_height(140.0)
        .show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Event Log").strong());

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // ✅ 搜索框
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.log_filter)
                        .hint_text("🔍 Search logs...")
                        .desired_width(200.0),
                );
                if resp.changed() {
                    // 重新过滤立即生效
                    ctx.request_repaint();
                }
            });
        });

        ui.add_space(4.0);

        egui::Frame::none()
            .fill(Color32::from_rgb(245, 247, 250))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let full_width = ui.available_width();

                        // ✅ 按过滤条件显示
                        for line in &self.log_lines {
                            if self.log_filter.is_empty()
                                || line
                                    .to_lowercase()
                                    .contains(&self.log_filter.to_lowercase())
                            {
                                // 自动染色：ERROR 红，WARN 橙，其他灰
                                let color = if line.contains("ERROR") {
                                    Color32::from_rgb(220, 60, 60)
                                } else if line.contains("WARN") {
                                    Color32::from_rgb(230, 180, 70)
                                } else {
                                    Color32::from_gray(30)
                                };

                                ui.add_sized(
                                    [full_width, 18.0],
                                    egui::Label::new(
                                        egui::RichText::new(line)
                                            .monospace()
                                            .color(color),
                                    ),
                                );
                            }
                        }
                    });
            });
    });


        // 4. 左侧 SidePanel：DATA TEMPLATE + 四个卡片（可以滚动）
        egui::SidePanel::left("stats_panel")
        .resizable(false)
        .min_width(220.0)
        .max_width(240.0)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    self.ui_stats_panel(ui);
                });
        });

        // 5. 中间 CentralPanel：Live + Total Timeline + Rate + 曲线
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical(|ui| {
                // Live 行
                ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Message:").strong());

                if let Some(live_text) = self.last_live_line.as_deref() {
                    ui.label(egui::RichText::new(live_text).monospace());
                }
                // 如果还没收到数据，就只剩一个 "Live:"，后面是空
            });


                ui.add_space(6.0);

                // 标题 + Rate
                ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Total Timeline").strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("Rate: {:.2} pulses/s", self.rate_hz()))
                            .monospace(),
                    );
                });
            });

                ui.add_space(4.0);

                Plot::new("pulse_plot")
                    .height(260.0)
                    .legend(Legend::default())
                    .show(ui, |plot_ui| {
                        if !self.plot_points.is_empty() {
                            let points: PlotPoints = self
                                .plot_points
                                .iter()
                                .copied()
                                .collect();
                            let line = Line::new("Total", points)
                                .color(Color32::from_rgb(120, 180, 255))
                                .width(2.0);
                            plot_ui.line(line);
                        }
                    });
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
            .with_min_inner_size(egui::vec2(800.0, 620.0))
            .with_title("DHJC ARC MONITOR - Rust GUI"),
        ..Default::default()
    };

    eframe::run_native(
        "dhjc_rust_gui",
        native_options,
        Box::new(move |cc| Ok(Box::new(DhjcApp::new(cc, cfg.clone())))),
    )
}
