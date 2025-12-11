// src/dhjc_core.rs
//
// 从 tiny_dhjc.cpp 提出来的协议解析核心：
// - 维护 Stage / Total / Active Time
// - 解析 MCU 输出的 [Live]、[STAGE REPORT]、[TOTAL SUMMARY]

use chrono::Local;

#[derive(Debug, Clone)]
pub struct CoreState {
    pub stage: i32,
    pub current_total: i32,
    pub active_time_s: f64,
    pub last_timestamp: Option<String>,

    active_from_mcu: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Change {
    pub system_reset: bool,
    pub stage_changed: bool,
    pub active_changed: bool,
    pub total_changed: bool,
    pub session_reset: bool,
}

impl CoreState {
    pub fn new() -> Self {
        Self {
            stage: 0,
            current_total: 0,
            active_time_s: 0.0,
            last_timestamp: None,
            active_from_mcu: false,
        }
    }

    fn reset_session(&mut self) {
        self.current_total = 0;
        self.stage = 0;
        self.active_time_s = 0.0;
        self.active_from_mcu = false;
        self.last_timestamp = None;
    }

    /// 解析一行 MCU 输出
    pub fn process_line(&mut self, raw: &str) -> Change {
        let mut change = Change::default();

        // 去掉控制字符，只保留 TAB 和可见字符
        let mut clean = String::with_capacity(raw.len());
        for b in raw.bytes() {
            if b == b'\t' || b >= 0x20 {
                clean.push(b as char);
            }
        }
        let clean = clean.trim();
        if clean.is_empty() {
            return change;
        }

        // SYSTEM RESET OK -> 整体重置
        if clean.contains("SYSTEM RESET OK") {
            self.reset_session();
            change.system_reset = true;
            change.session_reset = true;
            return change;
        }

        // [Live] 行：更新 Stage + Total
        if clean.contains("[Live]") || clean.contains("[LIVE]") {
            if let Some(stg) = find_int_after(clean, "Stage:") {
                if stg != self.stage {
                    self.stage = stg;
                    change.stage_changed = true;
                }
            }

            if let Some(total) = find_int_after(clean, "Total:") {
                self.update_total_from_live(total, &mut change);
            }

            return change;
        }

        // ======== 非 Live 行：阶段报告 / 总报告 ========

        // 1) 分阶段 Duration（ms）—— 用这个累计 Active Time
        let mut active_changed = false;
        if let Some(dur_ms) = find_double_after(clean, "Duration") {
            self.active_time_s += dur_ms / 1000.0;
            self.active_from_mcu = false;
            active_changed = true;
        }

        // 2) MCU 在 TOTAL SUMMARY 里给的 Active Time（秒）—— 覆盖
        if !active_changed {
            if let Some(at_s) = find_double_after(clean, "Active Time") {
                self.active_time_s = at_s;
                self.active_from_mcu = true;
                active_changed = true;
            }
        }

        if active_changed {
            change.active_changed = true;
        }

        // 3) Grand Total（总脉冲数）
        if let Some(gt) = find_int_after(clean, "Grand Total") {
            self.update_total_from_live(gt, &mut change);
        }

        change
    }

    fn update_total_from_live(&mut self, new_total: i32, change: &mut Change) {
        if new_total < self.current_total {
            // 计数回退，认为是新 session
            self.reset_session();
            self.current_total = new_total;
            change.session_reset = true;
            change.total_changed = true;
        } else if new_total > self.current_total {
            self.current_total = new_total;
            self.last_timestamp =
                Some(Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
            change.total_changed = true;
        }
    }
}

// ----------------- 工具函数 -----------------

fn find_int_after(src: &str, key: &str) -> Option<i32> {
    let start = src.find(key)? + key.len();
    let mut s = String::new();
    let mut started = false;

    for ch in src[start..].chars() {
        if !started {
            if ch.is_ascii_digit() || ch == '+' || ch == '-' {
                s.push(ch);
                started = true;
            } else {
                continue;
            }
        } else {
            if ch.is_ascii_digit() {
                s.push(ch);
            } else {
                break;
            }
        }
    }

    if s.is_empty() {
        None
    } else {
        s.parse().ok()
    }
}

fn find_double_after(src: &str, key: &str) -> Option<f64> {
    let start = src.find(key)? + key.len();
    let mut s = String::new();
    let mut started = false;

    for ch in src[start..].chars() {
        if !started {
            if ch.is_ascii_digit() || ch == '.' || ch == '+' || ch == '-' {
                s.push(ch);
                started = true;
            } else {
                continue;
            }
        } else {
            if ch.is_ascii_digit() || ch == '.' {
                s.push(ch);
            } else {
                break;
            }
        }
    }

    if s.is_empty() {
        None
    } else {
        s.parse().ok()
    }
}
