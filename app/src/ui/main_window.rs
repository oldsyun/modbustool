//! Main workbench window — toolbar, poll config panel, data table,
//! trace row, log pane, status bar, and in-window connection / write dialogs.
//!
//! Native port of the old web UI (`index.html` + `src/main.ts`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::StyledExt;
use gpui_kit::gpui::{
    div, px, App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Window,
};

use crate::core::service::{Event, Service};
use crate::ui::common::{bridge_events, theme};

// ── row decode helpers ───────────────────────────────────────────

fn is_32bit(fmt: &str) -> bool {
    matches!(fmt, "u32" | "i32" | "f32")
}

/// u16 → "HH LL" (high byte first, Modbus wire order).
fn format_raw_word(reg: u16) -> String {
    format!("{:02X} {:02X}", (reg >> 8) & 0xff, reg & 0xff)
}

/// Recombine the two words of a 32-bit value into bytes per byte order.
fn reg_bytes(w1: u16, w2: u16, order: &str) -> [u8; 4] {
    let b1hi = (w1 >> 8) as u8;
    let b1lo = (w1 & 0xff) as u8;
    let b2hi = (w2 >> 8) as u8;
    let b2lo = (w2 & 0xff) as u8;
    match order {
        "cdab" => [b2hi, b2lo, b1hi, b1lo],
        "badc" => [b1lo, b1hi, b2lo, b2hi],
        "dcba" => [b2lo, b2hi, b1lo, b1hi],
        _ => [b1hi, b1lo, b2hi, b2lo], // abcd
    }
}

/// Decode a register row with the given format and byte order.
/// `func` — only FC01/FC02 force boolean display.
fn parse_reg(regs: &[u16], offset: usize, fmt: &str, order: &str, func: &str) -> String {
    if func == "01" || func == "02" {
        return if regs.get(offset).copied().unwrap_or(0) != 0 {
            "true".into()
        } else {
            "false".into()
        };
    }
    let v = regs.get(offset).copied().unwrap_or(0);
    match fmt {
        "i16" => {
            let s = if v >= 0x8000 {
                v as i32 - 0x10000
            } else {
                v as i32
            };
            s.to_string()
        }
        "u32" | "i32" | "f32" => {
            if regs.len() < offset + 2 {
                return "—".into();
            }
            let w2 = regs.get(offset + 1).copied().unwrap_or(0);
            let b = reg_bytes(v, w2, order);
            let u32v = u32::from_be_bytes(b);
            match fmt {
                "u32" => u32v.to_string(),
                "i32" => (u32v as i32).to_string(),
                _ => f32::from_bits(u32v).to_string(),
            }
        }
        _ => v.to_string(), // u16
    }
}

/// Binary bit-pattern display for a row (16 or 32 bit).
fn parsed_binary(regs: &[u16], offset: usize, fmt: &str, order: &str, func: &str) -> String {
    if func == "01" || func == "02" {
        return regs.get(offset).copied().unwrap_or(0).to_string();
    }
    if is_32bit(fmt) {
        if regs.len() < offset + 2 {
            return "—".into();
        }
        let w1 = regs.get(offset).copied().unwrap_or(0);
        let w2 = regs.get(offset + 1).copied().unwrap_or(0);
        let b = reg_bytes(w1, w2, order);
        let u32v = u32::from_be_bytes(b);
        let s = format!("{:032b}", u32v);
        return s
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" ");
    }
    let v = regs.get(offset).copied().unwrap_or(0);
    let s = format!("{:016b}", v);
    s.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_poll_start_addr(text: &str) -> u16 {
    let t = text.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).unwrap_or(0)
    } else if !t.is_empty() {
        t.parse::<u32>().ok().filter(|v| *v <= 0xffff).map(|v| v as u16).unwrap_or(0)
    } else {
        0
    }
}

/// Result of write-value parsing (mirrors `parseWriteValue` in main.ts).
struct ParsedWrite {
    ok: bool,
    value: f64,
    msg: Option<String>,
}

fn range_label(fmt: &str) -> (&'static str, f64, f64, bool) {
    // (label, min, max, int_only)
    match fmt {
        "i16" => ("-32768 ~ 32767", -32768.0, 32767.0, true),
        "u32" => ("0 ~ 4294967295", 0.0, 4294967295.0, true),
        "i32" => (
            "-2147483648 ~ 2147483647",
            -2147483648.0,
            2147483647.0,
            true,
        ),
        "f32" => ("约 ±3.4e38（IEEE754 单精度）", -3.4028235e38, 3.4028235e38, false),
        _ => ("0 ~ 65535", 0.0, 65535.0, true),
    }
}

fn format_name(fmt: &str) -> &'static str {
    match fmt {
        "i16" => "有符号 16 位",
        "u32" => "无符号 32 位",
        "i32" => "有符号 32 位",
        "f32" => "浮点 32 位",
        _ => "无符号 16 位",
    }
}

fn parse_write_value(raw: &str, fmt: &str, func: &str) -> ParsedWrite {
    let s = raw.trim();
    if s.is_empty() {
        return ParsedWrite { ok: false, value: 0.0, msg: Some("请输入要写入的数值".into()) };
    }
    let (is_hex, body) = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (true, rest)
    } else {
        (false, s)
    };
    let mut value: f64;
    if is_hex {
        match i64::from_str_radix(body, 16) {
            Ok(v) => value = v as f64,
            Err(_) => {
                return ParsedWrite { ok: false, value: 0.0, msg: Some("无效的十六进制数值".into()) }
            }
        }
    } else {
        match s.parse::<f64>() {
            Ok(v) => value = v,
            Err(_) => {
                return ParsedWrite {
                    ok: false,
                    value: 0.0,
                    msg: Some("请输入有效数值（十进制或 0x 前缀十六进制）".into()),
                }
            }
        }
    }
    let is_int = value.fract() == 0.0;
    // coil: only 0/1
    if func == "01" {
        if !is_int || value < 0.0 || value > 1.0 {
            return ParsedWrite { ok: false, value: 0.0, msg: Some("线圈值无效：请输入 0 或 1".into()) };
        }
        return ParsedWrite { ok: true, value, msg: None };
    }
    // signed two's-complement hex input
    if fmt == "i16" && is_hex && value >= 0x8000 as f64 && value <= 0xffff as f64 {
        value -= 0x10000 as f64;
    } else if fmt == "i32" && is_hex && value >= 0x80000000u64 as f64 && value <= 0xffffffffu64 as f64 {
        value -= 0x100000000u64 as f64;
    }
    let now_int = value.fract() == 0.0;
    let (label, min, max, int_only) = range_label(fmt);
    if int_only && !now_int {
        return ParsedWrite { ok: false, value: 0.0, msg: Some(format!("{} 仅支持整数", format_name(fmt))) };
    }
    if value < min || value > max {
        return ParsedWrite {
            ok: false,
            value: 0.0,
            msg: Some(format!("数值超出 {} 范围（{}）", format_name(fmt), label)),
        };
    }
    ParsedWrite { ok: true, value, msg: None }
}

/// Split a validated value into 1 or 2 u16 words (mirrors `valueToWords`).
fn value_to_words(value: f64, fmt: &str, order: &str, func: &str) -> Vec<u16> {
    if func == "01" {
        return vec![if value != 0.0 { 1 } else { 0 }];
    }
    match fmt {
        "u16" => vec![(value as i64 & 0xffff) as u16],
        "i16" => {
            let v = value as i64;
            vec![(if v < 0 { v + 0x10000 } else { v } & 0xffff) as u16]
        }
        _ => {
            let bytes: [u8; 4] = if fmt == "f32" {
                f32::to_be_bytes(value as f32)
            } else {
                (value as i64 as u32).to_be_bytes()
            };
            let [b0, b1, b2, b3] = bytes;
            match order {
                "cdab" => vec![((b2 as u16) << 8) | b3 as u16, ((b0 as u16) << 8) | b1 as u16],
                "badc" => vec![((b1 as u16) << 8) | b0 as u16, ((b3 as u16) << 8) | b2 as u16],
                "dcba" => vec![((b3 as u16) << 8) | b2 as u16, ((b1 as u16) << 8) | b0 as u16],
                _ => vec![((b0 as u16) << 8) | b1 as u16, ((b2 as u16) << 8) | b3 as u16],
            }
        }
    }
}

fn now_ts() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // HH:MM:SS local-ish (UTC, acceptable for a log timestamp)
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

// ── state structs ────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub enum LogKind {
    Tx,
    Rx,
    Ok,
    Err,
    Info,
}

#[derive(Clone)]
struct PollConfig {
    id: u32,
    name: String,
    unit_id: u8,
    func: String,
    start_addr: u16,
    count: u16,
    period: u64,
    write_value: String,
    byte_order: String,
}

#[derive(Clone)]
struct RowState {
    format: String,
    order: String,
    show_bin: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnMode {
    Tcp,
    Udp,
    Rtu,
    RtuTcp,
    RtuUdp,
}

pub struct MainWindow {
    service: Arc<Service>,
    _events: Option<Task<()>>,

    // polls
    polls: Vec<PollConfig>,
    active_poll_id: u32,
    next_poll_id: u32,
    row_states: HashMap<u16, RowState>,

    // data
    poll_data: Option<(u16, Vec<u16>)>,
    last_tx: String,
    last_rx: String,
    last_rtt: Option<u64>,

    // runtime
    connected: bool,
    polling: bool,

    // log
    log_lines: Vec<(String, LogKind)>,

    // ui flags
    plc_addr_mode: bool,
    show_trace: bool,

    // connection dialog
    conn_dialog_open: bool,
    conn_mode: ConnMode,
    conn_host: Entity<InputState>,
    conn_port: Entity<InputState>,
    conn_timeout: Entity<InputState>,
    conn_serial_port: Entity<InputState>,
    conn_baud: Entity<InputState>,
    conn_databits: Entity<InputState>,
    conn_stopbits: Entity<InputState>,
    conn_parity: String,
    conn_error: Option<String>,

    // write dialog
    write_dialog_open: bool,
    write_addr: u16,
    write_fmt: String,
    write_order: String,
    write_requires_restart: bool,
    write_value: Entity<InputState>,
    write_error: Option<String>,

    // input entities for poll controls
    in_unit: Entity<InputState>,
    in_start: Entity<InputState>,
    in_count: Entity<InputState>,
    in_period: Entity<InputState>,
    in_write: Entity<InputState>,

    _subs: Vec<Subscription>,
}

const FUNC_OPTIONS: &[(&str, &str)] = &[
    ("01", "01 读线圈"),
    ("02", "02 读离散输入"),
    ("03", "03 读保持寄存器"),
    ("04", "04 读输入寄存器"),
    ("05", "05 写单个线圈"),
    ("06", "06 写单个保持寄存器"),
    ("15", "15 写多个线圈"),
    ("16", "16 写多个保持寄存器"),
];

const FORMAT_OPTIONS: &[(&str, &str)] = &[
    ("u16", "无符号16"),
    ("i16", "有符号16"),
    ("u32", "无符号32"),
    ("i32", "有符号32"),
    ("f32", "浮点32"),
];

const ORDER_OPTIONS: &[&str] = &["abcd", "cdab", "badc", "dcba"];

fn is_write_func(func: &str) -> bool {
    matches!(func, "05" | "06" | "15" | "16")
}

impl MainWindow {
    pub fn new(service: Arc<Service>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mk_input = |window: &mut Window, cx: &mut Context<Self>, ph: &str, dv: &str| {
            cx.new(|cx| InputState::new(window, cx).placeholder(ph.to_string()).default_value(dv.to_string()))
        };

        let in_unit = mk_input(window, cx, "Unit ID", "1");
        let in_start = mk_input(window, cx, "0 或 0x00", "0");
        let in_count = mk_input(window, cx, "数量", "10");
        let in_period = mk_input(window, cx, "500", "500");
        let in_write = mk_input(window, cx, "值或 1,0,1", "");

        let conn_host = mk_input(window, cx, "127.0.0.1", "127.0.0.1");
        let conn_port = mk_input(window, cx, "502", "502");
        let conn_timeout = mk_input(window, cx, "1000", "1000");
        let conn_serial_port = mk_input(window, cx, "COM1 / /dev/tty…", "");
        let conn_baud = mk_input(window, cx, "9600", "9600");
        let conn_databits = mk_input(window, cx, "8", "8");
        let conn_stopbits = mk_input(window, cx, "1", "1");

        let write_value = mk_input(window, cx, "输入要写入的数值", "");

        // observe poll inputs → keep the active poll config in sync + refresh.
        let mut subs = Vec::new();
        subs.push(cx.observe(&in_unit, |this, _e, cx| this.sync_from_inputs(cx)));
        subs.push(cx.observe(&in_start, |this, _e, cx| this.sync_from_inputs(cx)));
        subs.push(cx.observe(&in_count, |this, _e, cx| this.sync_from_inputs(cx)));
        subs.push(cx.observe(&in_period, |this, _e, cx| this.sync_from_inputs(cx)));
        subs.push(cx.observe(&in_write, |this, _e, cx| this.sync_from_inputs(cx)));

        // event bridge
        let svc = service.clone();
        let _events = Some(bridge_events(svc, cx, |this: &mut Self, ev, cx| {
            this.on_event(ev, cx);
        }));

        let polls = vec![PollConfig {
            id: 0,
            name: "轮询 1".to_string(),
            unit_id: 1,
            func: "03".to_string(),
            start_addr: 0,
            count: 10,
            period: 500,
            write_value: String::new(),
            byte_order: "abcd".to_string(),
        }];

        Self {
            service,
            _events,
            polls,
            active_poll_id: 0,
            next_poll_id: 1,
            row_states: HashMap::new(),
            poll_data: None,
            last_tx: String::new(),
            last_rx: String::new(),
            last_rtt: None,
            connected: false,
            polling: false,
            log_lines: Vec::new(),
            plc_addr_mode: false,
            show_trace: true,
            conn_dialog_open: false,
            conn_mode: ConnMode::Tcp,
            conn_host,
            conn_port,
            conn_timeout,
            conn_serial_port,
            conn_baud,
            conn_databits,
            conn_stopbits,
            conn_parity: "none".to_string(),
            conn_error: None,
            write_dialog_open: false,
            write_addr: 0,
            write_fmt: "u16".to_string(),
            write_order: "abcd".to_string(),
            write_requires_restart: false,
            write_value,
            write_error: None,
            in_unit,
            in_start,
            in_count,
            in_period,
            in_write,
            _subs: subs,
        }
    }

    // ── config accessors ─────────────────────────────────────────
    fn active_poll(&self) -> &PollConfig {
        self.polls
            .iter()
            .find(|p| p.id == self.active_poll_id)
            .unwrap_or(&self.polls[0])
    }

    fn active_poll_mut(&mut self) -> &mut PollConfig {
        let id = self.active_poll_id;
        let idx = self.polls.iter().position(|p| p.id == id).unwrap_or(0);
        &mut self.polls[idx]
    }

    fn func(&self) -> String {
        self.active_poll().func.clone()
    }

    fn is_bit_func(&self) -> bool {
        matches!(self.func().as_str(), "01" | "02")
    }

    fn sync_from_inputs(&mut self, cx: &mut Context<Self>) {
        let unit: u8 = self.in_unit.read(cx).value().trim().parse().unwrap_or(1);
        let start = parse_poll_start_addr(&self.in_start.read(cx).value());
        let count: u16 = self.in_count.read(cx).value().trim().parse().unwrap_or(1).max(1);
        let period: u64 = self.in_period.read(cx).value().trim().parse().unwrap_or(500).max(50);
        let wv = self.in_write.read(cx).value().to_string();
        let func = self.func();
        // for FC15/16, count derives from comma list length
        let count = if func == "15" || func == "16" {
            let n = wv.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).count();
            if n > 0 {
                n as u16
            } else {
                count
            }
        } else {
            count
        };
        let p = self.active_poll_mut();
        p.unit_id = unit;
        p.start_addr = start;
        p.count = count;
        p.period = period;
        p.write_value = wv;
        cx.notify();
    }

    fn load_poll_to_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let p = self.active_poll().clone();
        self.in_unit.update(cx, |s, cx| s.set_value(p.unit_id.to_string(), window, cx));
        self.in_start.update(cx, |s, cx| s.set_value(p.start_addr.to_string(), window, cx));
        self.in_count.update(cx, |s, cx| s.set_value(p.count.to_string(), window, cx));
        self.in_period.update(cx, |s, cx| s.set_value(p.period.to_string(), window, cx));
        self.in_write.update(cx, |s, cx| s.set_value(p.write_value.clone(), window, cx));
    }

    // ── logging ──────────────────────────────────────────────────
    pub fn push_log(&mut self, msg: String, kind: LogKind) {
        self.log_lines.push((format!("[{}] {}", now_ts(), msg), kind));
        if self.log_lines.len() > 5000 {
            self.log_lines.drain(0..1000);
        }
    }

    fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        if let Event::PollFrame { addr, regs, tx, rx, rtt_ms, error, .. } = ev {
            if !tx.is_empty() {
                self.push_log(format!("TX: {tx}"), LogKind::Tx);
            }
            match error {
                Some(e) => self.push_log(format!("RX ERR: {e}"), LogKind::Err),
                None => {
                    self.last_tx = tx;
                    self.last_rx = rx.clone();
                    self.last_rtt = rtt_ms;
                    if let Some(r) = regs {
                        self.poll_data = Some((addr, r));
                    }
                    self.push_log(
                        format!("RX: {} （{}ms）", rx, rtt_ms.unwrap_or(0)),
                        LogKind::Rx,
                    );
                }
            }
            cx.notify();
        }
    }

    // ── poll management ──────────────────────────────────────────
    fn add_poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let src = self.active_poll().clone();
        let id = self.next_poll_id;
        self.next_poll_id += 1;
        let np = PollConfig {
            id,
            name: format!("轮询 {}", self.polls.len() + 1),
            unit_id: src.unit_id,
            func: src.func,
            start_addr: src.start_addr,
            count: src.count,
            period: src.period,
            write_value: String::new(),
            byte_order: src.byte_order,
        };
        self.polls.push(np);
        self.active_poll_id = id;
        self.poll_data = None;
        self.load_poll_to_inputs(window, cx);
        cx.notify();
    }

    fn remove_poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.polls.len() <= 1 {
            return;
        }
        let id = self.active_poll_id;
        let removed_id = id;
        self.polls.retain(|p| p.id != id);
        self.active_poll_id = self.polls[0].id;
        self.poll_data = None;
        // stop the removed poll's task if running
        let svc = self.service.clone();
        cx.spawn(async move |_this, _cx| {
            svc.stop_poll(removed_id).await;
        })
        .detach();
        self.load_poll_to_inputs(window, cx);
        cx.notify();
    }

    fn activate_poll(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        if id == self.active_poll_id {
            return;
        }
        self.active_poll_id = id;
        self.poll_data = None;
        self.load_poll_to_inputs(window, cx);
        cx.notify();
    }

    fn set_func(&mut self, func: &str, cx: &mut Context<Self>) {
        self.active_poll_mut().func = func.to_string();
        // write funcs stop polling
        if is_write_func(func) && self.polling {
            let svc = self.service.clone();
            cx.spawn(async move |this, cx| {
                svc.stop_all_polls().await;
                let _ = this.update(cx, |this, cx| {
                    this.polling = false;
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn set_byte_order(&mut self, order: &str, cx: &mut Context<Self>) {
        self.active_poll_mut().byte_order = order.to_string();
        cx.notify();
    }

    fn set_conn_mode(&mut self, mode: ConnMode, _cx: &mut Context<Self>) {
        self.conn_mode = mode;
    }

    // ── connect / disconnect ─────────────────────────────────────
    fn do_connect(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        let host = self.conn_host.read(cx).value().to_string();
        let port: u16 = self.conn_port.read(cx).value().trim().parse().unwrap_or(502);
        let timeout: u64 = self.conn_timeout.read(cx).value().trim().parse().unwrap_or(1000);
        let serial = self.conn_serial_port.read(cx).value().to_string();
        let baud: u32 = self.conn_baud.read(cx).value().trim().parse().unwrap_or(9600);
        let databits: u8 = self.conn_databits.read(cx).value().trim().parse().unwrap_or(8);
        let stopbits: u8 = self.conn_stopbits.read(cx).value().trim().parse().unwrap_or(1);
        let parity = self.conn_parity.clone();
        let mode = self.conn_mode;

        cx.spawn(async move |this, cx| {
            let res = match mode {
                ConnMode::Tcp => svc.connect(host, port, timeout, 1, None).await,
                ConnMode::Udp => svc.connect(host, port, timeout, 1, Some("udp".into())).await,
                ConnMode::Rtu => svc.connect_rtu(serial, baud, databits, stopbits, parity, timeout, None).await,
                ConnMode::RtuTcp => svc.connect_rtu_over_tcp(host, port, timeout, 1, None, None).await,
                ConnMode::RtuUdp => {
                    svc.connect_rtu_over_tcp(host, port, timeout, 1, None, Some("udp".into())).await
                }
            };
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(()) => {
                        this.connected = true;
                        this.conn_dialog_open = false;
                        this.conn_error = None;
                        this.push_log("已连接".into(), LogKind::Ok);
                    }
                    Err(e) => {
                        this.connected = false;
                        this.conn_error = Some(e.clone());
                        this.push_log(format!("连接失败：{e}"), LogKind::Err);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn do_disconnect(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let _ = svc.disconnect().await;
            let _ = this.update(cx, |this, cx| {
                this.connected = false;
                this.polling = false;
                this.push_log("已断开".into(), LogKind::Info);
                cx.notify();
            });
        })
        .detach();
    }

    // ── polling ──────────────────────────────────────────────────
    fn toggle_polling(&mut self, cx: &mut Context<Self>) {
        if !self.connected {
            self.push_log("请先连接".into(), LogKind::Err);
            cx.notify();
            return;
        }
        if self.polling {
            let svc = self.service.clone();
            cx.spawn(async move |this, cx| {
                svc.stop_all_polls().await;
                let _ = this.update(cx, |this, cx| {
                    this.polling = false;
                    this.push_log("所有轮询已停止".into(), LogKind::Info);
                    cx.notify();
                });
            })
            .detach();
        } else {
            let svc = self.service.clone();
            let configs: Vec<(u32, String, String, u16, u16, u64, u8)> = self
                .polls
                .iter()
                .map(|p| (p.id, p.name.clone(), p.func.clone(), p.start_addr, p.count, p.period, p.unit_id))
                .collect();
            cx.spawn(async move |this, cx| {
                for (id, name, func, addr, count, period, unit) in configs {
                    // skip write funcs in polling
                    if is_write_func(&func) {
                        continue;
                    }
                    if let Err(e) = svc.start_poll(id, name, func, addr, count, period, unit).await {
                        let _ = this.update(cx, |this, cx| {
                            this.push_log(format!("启动轮询失败：{e}"), LogKind::Err);
                            cx.notify();
                        });
                    }
                }
                let _ = this.update(cx, |this, cx| {
                    this.polling = true;
                    this.push_log("轮询已启动".into(), LogKind::Ok);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn send_once(&mut self, cx: &mut Context<Self>) {
        if !self.connected {
            self.push_log("请先连接".into(), LogKind::Err);
            cx.notify();
            return;
        }
        let p = self.active_poll().clone();
        let func = p.func.clone();
        let unit = p.unit_id;
        let addr = p.start_addr;
        let svc = self.service.clone();

        if is_write_func(&func) {
            // build values from write_value for FC05/06/15/16
            let words: Vec<u16> = if func == "15" {
                p.write_value
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse::<u16>().unwrap_or(0))
                    .collect()
            } else if func == "16" {
                p.write_value
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                            u16::from_str_radix(h, 16).unwrap_or(0)
                        } else {
                            s.parse::<u16>().unwrap_or(0)
                        }
                    })
                    .collect()
            } else if func == "05" {
                let v = p.write_value.trim().parse::<u16>().unwrap_or(0);
                vec![if v != 0 { 1 } else { 0 }]
            } else {
                let s = p.write_value.trim();
                let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                    u16::from_str_radix(h, 16).unwrap_or(0)
                } else {
                    s.parse::<u16>().unwrap_or(0)
                };
                vec![v]
            };
            if words.is_empty() {
                self.push_log("写入值为空".into(), LogKind::Err);
                cx.notify();
                return;
            }
            cx.spawn(async move |this, cx| {
                let res = svc.write_point(unit, func.clone(), addr, words).await;
                let _ = this.update(cx, |this, cx| match res {
                    Ok(w) => {
                        this.push_log(format!("[写入发送] {}", w.tx), LogKind::Tx);
                        this.push_log(format!("[写入接收] {} （{}ms）", w.rx, w.rtt_ms), LogKind::Rx);
                        cx.notify();
                    }
                    Err(e) => {
                        this.push_log(format!("写入失败：{e}"), LogKind::Err);
                        cx.notify();
                    }
                });
            })
            .detach();
        } else {
            let count = p.count;
            cx.spawn(async move |this, cx| {
                let res = svc.read_points(unit, func, addr, count).await;
                let _ = this.update(cx, |this, cx| match res {
                    Ok(r) => {
                        this.poll_data = Some((addr, r.regs.clone()));
                        this.last_tx = r.tx.clone();
                        this.last_rx = r.rx.clone();
                        this.last_rtt = Some(r.rtt_ms);
                        this.push_log(format!("[发送] {}", r.tx), LogKind::Tx);
                        this.push_log(format!("[接收] {} （{}ms）", r.rx, r.rtt_ms), LogKind::Rx);
                        cx.notify();
                    }
                    Err(e) => {
                        this.push_log(format!("读取失败：{e}"), LogKind::Err);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    // ── write dialog ─────────────────────────────────────────────
    fn open_write_dialog(&mut self, addr: u16, window: &mut Window, cx: &mut Context<Self>) {
        let st = self.row_states.get(&addr).cloned().unwrap_or(RowState {
            format: "u16".into(),
            order: self.active_poll().byte_order.clone(),
            show_bin: false,
        });
        self.write_addr = addr;
        self.write_fmt = st.format;
        self.write_order = st.order;
        self.write_error = None;
        self.write_dialog_open = true;
        // pause polling during write
        if self.polling {
            self.write_requires_restart = true;
            let svc = self.service.clone();
            cx.spawn(async move |this, cx| {
                svc.stop_all_polls().await;
                let _ = this.update(cx, |this, cx| {
                    this.polling = false;
                    cx.notify();
                });
            })
            .detach();
        } else {
            self.write_requires_restart = false;
        }
        self.write_value.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn confirm_write(&mut self, cx: &mut Context<Self>) {
        let raw = self.write_value.read(cx).value().to_string();
        let fmt = self.write_fmt.clone();
        let order = self.write_order.clone();
        let func = self.func();
        let parsed = parse_write_value(&raw, &fmt, &func);
        if !parsed.ok {
            self.write_error = parsed.msg;
            cx.notify();
            return;
        }
        let words = value_to_words(parsed.value, &fmt, &order, &func);
        let addr = self.write_addr;
        let unit = self.active_poll().unit_id;
        let svc = self.service.clone();
        let write_func = if func == "01" { "01".to_string() } else { "06".to_string() };
        // For 32-bit or multi, service write_point routes by values length; pass func 03-style? Use write_point with the correct func.
        let use_func = if matches!(fmt.as_str(), "u32" | "i32" | "f32") && func != "01" {
            "16".to_string()
        } else if func == "01" {
            "01".to_string()
        } else {
            write_func
        };
        cx.spawn(async move |this, cx| {
            let res = svc.write_point(unit, use_func, addr, words).await;
            let _ = this.update(cx, |this, cx| match res {
                Ok(w) => {
                    this.push_log(format!("[写入发送] {}", w.tx), LogKind::Tx);
                    this.push_log(format!("[写入接收] {} （{}ms）", w.rx, w.rtt_ms), LogKind::Rx);
                    this.write_dialog_open = false;
                    cx.notify();
                    // restart polling if we paused it
                    if this.write_requires_restart && this.connected {
                        this.toggle_polling(cx);
                    }
                }
                Err(e) => {
                    this.write_error = Some(e.clone());
                    this.push_log(format!("写入失败：{e}"), LogKind::Err);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    // ── project save / import ────────────────────────────────────
    fn save_project(&mut self, cx: &mut Context<Self>) {
        let polls: Vec<serde_json::Value> = self
            .polls
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "unitId": p.unit_id,
                    "func": p.func,
                    "startAddr": p.start_addr,
                    "count": p.count,
                    "period": p.period,
                    "writeValue": p.write_value,
                    "byteOrder": p.byte_order,
                })
            })
            .collect();
        let val = serde_json::json!({
            "version": 1,
            "polls": polls,
            "activePollId": self.active_poll_id,
        });
        let text = serde_json::to_string_pretty(&val).unwrap_or_default();
        let path = rfd::FileDialog::new()
            .set_file_name("modbus-project.json")
            .add_filter("JSON", &["json"])
            .save_file();
        match path {
            Some(p) => match std::fs::write(&p, text) {
                Ok(()) => self.push_log(format!("项目已保存：{}", p.display()), LogKind::Ok),
                Err(e) => self.push_log(format!("保存失败：{e}"), LogKind::Err),
            },
            None => {}
        }
        cx.notify();
    }

    fn import_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .pick_file();
        let Some(p) = path else { return };
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) => {
                self.push_log(format!("读取失败：{e}"), LogKind::Err);
                cx.notify();
                return;
            }
        };
        let val: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                self.push_log(format!("解析失败：{e}"), LogKind::Err);
                cx.notify();
                return;
            }
        };
        let mut new_polls = Vec::new();
        if let Some(arr) = val.get("polls").and_then(|v| v.as_array()) {
            let mut max_id = 0;
            for item in arr {
                let id = item.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                max_id = max_id.max(id);
                new_polls.push(PollConfig {
                    id,
                    name: item.get("name").and_then(|v| v.as_str()).unwrap_or("轮询").to_string(),
                    unit_id: item.get("unitId").and_then(|v| v.as_u64()).unwrap_or(1) as u8,
                    func: item
                        .get("func")
                        .and_then(|v| v.as_str())
                        .unwrap_or("03")
                        .to_string(),
                    start_addr: item.get("startAddr").and_then(|v| v.as_u64()).unwrap_or(0) as u16,
                    count: item.get("count").and_then(|v| v.as_u64()).unwrap_or(10) as u16,
                    period: item.get("period").and_then(|v| v.as_u64()).unwrap_or(500),
                    write_value: item
                        .get("writeValue")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    byte_order: item
                        .get("byteOrder")
                        .and_then(|v| v.as_str())
                        .unwrap_or("abcd")
                        .to_string(),
                });
            }
            self.next_poll_id = max_id + 1;
        }
        if !new_polls.is_empty() {
            self.polls = new_polls;
            self.active_poll_id = val
                .get("activePollId")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .filter(|id| self.polls.iter().any(|p| p.id == *id))
                .unwrap_or(self.polls[0].id);
            self.poll_data = None;
            self.load_poll_to_inputs(window, cx);
            self.push_log("项目已导入".into(), LogKind::Ok);
        }
        cx.notify();
    }

    fn toggle_plc(&mut self, cx: &mut Context<Self>) {
        self.plc_addr_mode = !self.plc_addr_mode;
        cx.notify();
    }

    fn addr_label(&self, addr: u16) -> String {
        if self.plc_addr_mode {
            let base: u32 = match self.func().as_str() {
                "01" => 1,
                "02" => 10001,
                "04" => 30001,
                _ => 40001,
            };
            format!("{}", base + addr as u32)
        } else {
            format!("0x{:02X}", addr)
        }
    }

    fn set_row_format(&mut self, addr: u16, fmt: &str, cx: &mut Context<Self>) {
        let order = self.active_poll().byte_order.clone();
        let st = self.row_states.entry(addr).or_insert(RowState {
            format: "u16".into(),
            order,
            show_bin: false,
        });
        st.format = fmt.to_string();
        cx.notify();
    }

    fn set_row_order(&mut self, addr: u16, order: &str, cx: &mut Context<Self>) {
        let fmt = self.active_poll().byte_order.clone();
        let st = self.row_states.entry(addr).or_insert(RowState {
            format: "u16".into(),
            order: fmt,
            show_bin: false,
        });
        st.order = order.to_string();
        cx.notify();
    }

    /// Compute which addresses are occupied by 32-bit rows: occupied_addr -> owner_addr.
    fn compute_occupied(&self) -> HashMap<u16, u16> {
        let mut occ = HashMap::new();
        let mut addrs: Vec<u16> = self.row_states.keys().copied().collect();
        addrs.sort_unstable();
        for addr in addrs {
            if occ.contains_key(&addr) {
                continue;
            }
            if let Some(st) = self.row_states.get(&addr) {
                if is_32bit(&st.format) {
                    occ.insert(addr.wrapping_add(1), addr);
                }
            }
        }
        occ
    }

    fn row_state_or_default(&self, addr: u16) -> RowState {
        self.row_states.get(&addr).cloned().unwrap_or(RowState {
            format: "u16".into(),
            order: self.active_poll().byte_order.clone(),
            show_bin: false,
        })
    }
}

impl Render for MainWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = div()
            .v_flex()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text())
            .child(self.render_toolbar(cx))
            .child(self.render_poll_panel(cx))
            .child(self.render_table(cx))
            .child(self.render_trace())
            .child(self.render_log())
            .child(self.render_status());

        // Overlay dialogs rendered absolutely on top.
        let mut root = div().relative().size_full().child(body);
        if self.conn_dialog_open {
            root = root.child(self.render_conn_dialog(cx));
        }
        if self.write_dialog_open {
            root = root.child(self.render_write_dialog(cx));
        }
        root
    }
}

// ── render helpers ───────────────────────────────────────────────
impl MainWindow {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let conn_label = if self.connected { "⬤ 断开" } else { "⬤ 连接" };
        let poll_label = if self.polling { "⬤ 停止" } else { "⬤ 轮询" };

        let svc_scan = self.service.clone();
        let svc_sim = self.service.clone();

        div()
            .h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .text_color(theme::accent())
                    .child("▦ Modbus Tool"),
            )
            .child(div().w(px(8.)))
            .child(
                Button::new("btn-connect")
                    .label(conn_label)
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        if this.connected {
                            this.do_disconnect(cx);
                        } else {
                            this.conn_dialog_open = true;
                            this.conn_error = None;
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("btn-plc")
                    .label("# PLC 地址")
                    .on_click(cx.listener(|this, _ev, _w, cx| this.toggle_plc(cx))),
            )
            .child(
                Button::new("btn-add-poll")
                    .label("+ 添加轮询")
                    .on_click(cx.listener(|this, _ev, w, cx| this.add_poll(w, cx))),
            )
            .child(
                Button::new("btn-del-poll")
                    .label("— 删除轮询")
                    .on_click(cx.listener(|this, _ev, w, cx| this.remove_poll(w, cx))),
            )
            .child(Button::new("btn-scan").label("⌕ 扫描").on_click(
                move |_ev, _w, cx: &mut App| crate::app::open_scanner(svc_scan.clone(), cx),
            ))
            .child(
                Button::new("btn-poll")
                    .label(poll_label)
                    .primary()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.toggle_polling(cx))),
            )
            .child(div().w(px(8.)))
            .child(
                Button::new("btn-save")
                    .label("💾 保存项目")
                    .on_click(cx.listener(|this, _ev, _w, cx| this.save_project(cx))),
            )
            .child(
                Button::new("btn-import")
                    .label("📂 导入项目")
                    .on_click(cx.listener(|this, _ev, w, cx| this.import_project(w, cx))),
            )
            .child(div().w(px(8.)))
            .child(Button::new("btn-sim").label("⚙ 模拟器").on_click(
                move |_ev, _w, cx: &mut App| crate::app::open_simulator(svc_sim.clone(), cx),
            ))
    }

    fn render_poll_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // tab strip
        let active = self.active_poll_id;
        let mut tabs = div().h_flex().gap_1().flex_wrap();
        for p in &self.polls {
            let id = p.id;
            let name = p.name.clone();
            let is_active = id == active;
            let btn = Button::new(SharedString::from(format!("tab-{id}")))
                .label(name)
                .on_click(cx.listener(move |this, _ev, w, cx| this.activate_poll(id, w, cx)));
            let btn = if is_active { btn.primary() } else { btn };
            tabs = tabs.child(btn);
        }

        let func = self.func();
        let func_label = FUNC_OPTIONS
            .iter()
            .find(|(v, _)| *v == func)
            .map(|(_, l)| *l)
            .unwrap_or("03 读保持寄存器");
        let func_for_drop = func.clone();
        let order = self.active_poll().byte_order.clone();

        // func dropdown row (cycle via buttons)
        let mut func_row = div().h_flex().gap_1();
        for (v, _label) in FUNC_OPTIONS {
            let vv = v.to_string();
            let sel = *v == func_for_drop;
            let b = Button::new(SharedString::from(format!("fc-{v}")))
                .label(*v)
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.set_func(&vv, cx);
                }));
            let b = if sel { b.primary() } else { b };
            func_row = func_row.child(b);
        }

        let mut order_row = div().h_flex().gap_1();
        for o in ORDER_OPTIONS {
            let oo = o.to_string();
            let sel = order == *o;
            let b = Button::new(SharedString::from(format!("ord-{o}")))
                .label(o.to_uppercase())
                .on_click(cx.listener(move |this, _ev, _w, cx| this.set_byte_order(&oo, cx)));
            let b = if sel { b.primary() } else { b };
            order_row = order_row.child(b);
        }

        let is_bit = self.is_bit_func();
        let write_func = is_write_func(&func);

        let mut controls = div().h_flex().gap_3().flex_wrap().items_end();
        controls = controls
            .child(self.field_wrap("Unit ID", Input::new(&self.in_unit).h(px(30.)).w(px(70.))))
            .child(self.field_wrap("功能码", func_row))
            .child(self.field_wrap("起始地址", Input::new(&self.in_start).h(px(30.)).w(px(90.))));

        if !write_func {
            controls = controls.child(self.field_wrap("数量", Input::new(&self.in_count).h(px(30.)).w(px(70.))));
        }
        if write_func {
            controls = controls.child(self.field_wrap(
                "写入值",
                Input::new(&self.in_write).h(px(30.)).w(px(180.)),
            ));
        }
        if !is_bit {
            controls = controls.child(self.field_wrap("默认字序", order_row));
        }

        controls = controls.child(
            div()
                .text_sm()
                .text_color(theme::text_dim())
                .child(func_label.to_string()),
        );

        let period_input = self.field_wrap("周期(ms)", Input::new(&self.in_period).h(px(30.)).w(px(80.)));

        // trace toggle
        let trace_label = if self.show_trace { "隐藏" } else { "显示" };

        div()
            .v_flex()
            .gap_2()
            .p_3()
            .bg(theme::surface())
            .border_b_1()
            .border_color(theme::border())
            .child(tabs)
            .child(controls)
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(period_input)
                    .child(
                        Button::new("btn-send-once")
                            .label("发送")
                            .primary()
                            .on_click(cx.listener(|this, _ev, _w, cx| this.send_once(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("btn-toggle-trace")
                            .label(trace_label)
                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                this.show_trace = !this.show_trace;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn field_wrap(&self, label: &str, control: impl IntoElement) -> impl IntoElement {
        div()
            .v_flex()
            .gap_1()
            .child(div().text_xs().text_color(theme::text_dim()).child(label.to_string()))
            .child(control)
    }

    fn render_table(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let func = self.func();
        let is_bit = self.is_bit_func();
        let writable = func == "01" || func == "03";
        let occ = self.compute_occupied();

        let (data_addr, regs) = match &self.poll_data {
            Some((a, r)) => (Some(*a), r.clone()),
            None => (None, Vec::new()),
        };

        // header
        let mut header = div()
            .h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .text_sm()
            .text_color(theme::text_dim());
        header = header
            .child(div().w(px(80.)).child("点位"))
            .child(div().w(px(80.)).child("原始字"))
            .child(div().w(px(90.)).child("显示格式"))
            .child(div().w(px(70.)).child(if is_bit { "" } else { "字节序" }))
            .child(div().w(px(160.)).child("解析值"))
            .child(div().w(px(60.)).child("操作"));

        // rows
        let mut rows = div().v_flex().gap_1().p_3().flex_1().id("table-scroll").overflow_y_scroll();

        let start = self.active_poll().start_addr;
        let count = self.active_poll().count.max(1);
        let empty = data_addr.is_none();

        for i in 0..count {
            let addr = start.wrapping_add(i);
            let offset = i as usize;
            let st = self.row_state_or_default(addr);

            let reg = if empty {
                None
            } else {
                regs.get(offset).copied()
            };
            let raw = match reg {
                Some(r) => format_raw_word(r),
                None => "—".to_string(),
            };

            let occupied_by = occ.get(&addr).copied();
            let is_occupied = occupied_by.is_some();

            // parsed value
            let parsed = if empty {
                "—".to_string()
            } else if let Some(by) = occupied_by {
                format!("由点位 {} 占用", self.addr_label(by))
            } else if st.show_bin {
                parsed_binary(&regs, offset, &st.format, &st.order, &func)
            } else {
                parse_reg(&regs, offset, &st.format, &st.order, &func)
            };
            let parsed_color = if is_occupied { theme::text_dim() } else { theme::text() };

            // format selector (bit row fixed, occupied frozen)
            let format_cell = if is_bit {
                div().w(px(70.)).text_sm().text_color(theme::text_dim()).child("布尔型")
            } else if is_occupied {
                div()
                    .w(px(90.))
                    .text_sm()
                    .text_color(theme::text_dim())
                    .child(
                        FORMAT_OPTIONS
                            .iter()
                            .find(|(v, _)| *v == st.format)
                            .map(|(_, l)| *l)
                            .unwrap_or("无符号16"),
                    )
            } else {
                div().w(px(90.)).h_flex().child(
                    Button::new(SharedString::from(format!("fmtlbl-{addr}")))
                        .label(
                            FORMAT_OPTIONS
                                .iter()
                                .find(|(v, _)| *v == st.format)
                                .map(|(_, l)| *l)
                                .unwrap_or("无符号16"),
                        )
                        .compact()
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            let cur = this.row_state_or_default(addr).format;
                            let idx = FORMAT_OPTIONS.iter().position(|(v, _)| *v == cur).unwrap_or(0);
                            let next = FORMAT_OPTIONS[(idx + 1) % FORMAT_OPTIONS.len()].0;
                            this.set_row_format(addr, next, cx);
                        })),
                )
            };

            // order selector
            let order_cell = if is_bit {
                div().w(px(70.))
            } else {
                let disabled = !is_32bit(&st.format) || is_occupied;
                let label = st.order.to_uppercase();
                let cell = div().w(px(70.)).h_flex();
                if disabled {
                    cell.child(
                        div()
                            .text_sm()
                            .text_color(theme::text_dim())
                            .px_2()
                            .py_1()
                            .child(label),
                    )
                } else {
                    cell.child(
                        Button::new(SharedString::from(format!("ordlbl-{addr}")))
                            .label(label)
                            .compact()
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                let cur = this.row_state_or_default(addr).order;
                                let idx = ORDER_OPTIONS.iter().position(|o| *o == cur).unwrap_or(0);
                                let next = ORDER_OPTIONS[(idx + 1) % ORDER_OPTIONS.len()];
                                this.set_row_order(addr, next, cx);
                            })),
                    )
                }
            };

            // op cell — write button
            let show_write = writable && !is_occupied && !empty;
            let mut op_cell = div().w(px(60.)).h_flex();
            if show_write {
                let a = addr;
                op_cell = op_cell.child(
                    Button::new(SharedString::from(format!("write-{addr}")))
                        .label("写入")
                        .compact()
                        .on_click(cx.listener(move |this, _ev, w, cx| {
                            this.open_write_dialog(a, w, cx);
                        })),
                );
            }

            let mut row = div()
                .h_flex()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(4.));
            if is_occupied {
                row = row.bg(theme::panel());
            }
            let row = row
                .child(
                    div()
                        .w(px(80.))
                        .text_sm()
                        .text_color(theme::amber())
                        .child(self.addr_label(addr)),
                )
                .child(
                    div()
                        .w(px(80.))
                        .text_sm()
                        .text_color(theme::text_dim())
                        .child(raw),
                )
                .child(format_cell)
                .child(order_cell)
                .child(
                    div()
                        .w(px(160.))
                        .text_sm()
                        .text_color(parsed_color)
                        .child(parsed),
                )
                .child(op_cell);

            rows = rows.child(row);
        }

        div()
            .v_flex()
            .flex_1()
            .min_h(px(160.))
            .border_b_1()
            .border_color(theme::border())
            .child(header)
            .child(rows)
    }

    fn render_trace(&self) -> impl IntoElement {
        if !self.show_trace {
            return div().px_3().py_1().bg(theme::surface()).child(
                div().text_xs().text_color(theme::text_dim()).child("通讯报文 Trace（已隐藏）"),
            );
        }
        let tx = if self.last_tx.is_empty() { "--".to_string() } else { self.last_tx.clone() };
        let rx = if self.last_rx.is_empty() { "--".to_string() } else { self.last_rx.clone() };
        let rtt = self
            .last_rtt
            .map(|v| format!("{v}ms"))
            .unwrap_or_else(|| "--".to_string());
        div()
            .v_flex()
            .gap_1()
            .px_3()
            .py_2()
            .bg(theme::surface())
            .border_b_1()
            .border_color(theme::border())
            .child(div().text_xs().font_semibold().text_color(theme::text_dim()).child("通讯报文 Trace"))
            .child(
                div()
                    .h_flex()
                    .gap_4()
                    .text_xs()
                    .child(
                        div()
                            .h_flex()
                            .gap_1()
                            .child(div().text_color(theme::text_dim()).child("发送："))
                            .child(div().text_color(theme::accent()).child(tx)),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_1()
                            .child(div().text_color(theme::text_dim()).child("接收："))
                            .child(div().text_color(theme::green()).child(rx)),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_1()
                            .child(div().text_color(theme::text_dim()).child("耗时："))
                            .child(div().text_color(theme::text()).child(rtt)),
                    ),
            )
    }

    fn render_log(&self) -> impl IntoElement {
        let mut list = div().v_flex().gap_1().p_2().flex_1().id("log-scroll").overflow_y_scroll();
        for (line, kind) in &self.log_lines {
            let color = match kind {
                LogKind::Tx => theme::accent(),
                LogKind::Rx => theme::green(),
                LogKind::Ok => theme::green(),
                LogKind::Err => theme::red(),
                LogKind::Info => theme::text_dim(),
            };
            list = list.child(div().text_xs().text_color(color).child(line.clone()));
        }
        div()
            .v_flex()
            .flex_1()
            .min_h(px(120.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .px_3()
                    .py_1()
                    .bg(theme::panel())
                    .text_xs()
                    .font_semibold()
                    .text_color(theme::text_dim())
                    .child("日志"),
            )
            .child(list)
    }

    fn render_status(&self) -> impl IntoElement {
        let conn = if self.connected {
            ("已连接", theme::green())
        } else {
            ("未连接", theme::text_dim())
        };
        let poll = if self.polling {
            ("轮询中", theme::green())
        } else {
            ("轮询停止", theme::text_dim())
        };
        let name = self.active_poll().name.clone();
        div()
            .h_flex()
            .gap_4()
            .px_3()
            .py_1()
            .bg(theme::panel())
            .border_t_1()
            .border_color(theme::border())
            .text_xs()
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .child(div().text_color(theme::text_dim()).child("连接："))
                    .child(div().text_color(conn.1).child(conn.0)),
            )
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .child(div().text_color(theme::text_dim()).child("轮询："))
                    .child(div().text_color(poll.1).child(poll.0)),
            )
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .child(div().text_color(theme::text_dim()).child("当前："))
                    .child(div().text_color(theme::text()).child(name)),
            )
    }

    fn render_conn_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mode_btn = |id: &str, label: &str, mode: ConnMode, current: ConnMode, cx: &mut Context<Self>| {
            let b = Button::new(SharedString::from(id.to_string()))
                .label(label)
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.set_conn_mode(mode, cx);
                    cx.notify();
                }));
            if mode == current {
                b.primary()
            } else {
                b
            }
        };

        let mode = self.conn_mode;
        let mut modes = div().h_flex().gap_1().flex_wrap();
        modes = modes
            .child(mode_btn("m-tcp", "TCP", ConnMode::Tcp, mode, cx))
            .child(mode_btn("m-udp", "UDP", ConnMode::Udp, mode, cx))
            .child(mode_btn("m-rtu", "RTU 串口", ConnMode::Rtu, mode, cx))
            .child(mode_btn("m-rtutcp", "RTU over TCP", ConnMode::RtuTcp, mode, cx))
            .child(mode_btn("m-rtuudp", "RTU over UDP", ConnMode::RtuUdp, mode, cx));

        let is_serial = mode == ConnMode::Rtu;
        let mut form = div().v_flex().gap_3();

        form = form.child(self.field_wrap("模式", modes));

        if is_serial {
            let port_label = if self.conn_parity.is_empty() {
                "none".to_string()
            } else {
                self.conn_parity.clone()
            };
            let _ = port_label;
            form = form
                .child(self.field_wrap("串口", Input::new(&self.conn_serial_port).h(px(30.)).w_full()))
                .child(
                    div()
                        .h_flex()
                        .gap_3()
                        .child(self.field_wrap("波特率", Input::new(&self.conn_baud).h(px(30.)).w(px(90.))))
                        .child(self.field_wrap("数据位", Input::new(&self.conn_databits).h(px(30.)).w(px(60.))))
                        .child(self.field_wrap("停止位", Input::new(&self.conn_stopbits).h(px(30.)).w(px(60.))))
                        .child(self.field_wrap(
                            "校验",
                            {
                                let cur = self.conn_parity.clone();
                                let mut pr = div().h_flex().gap_1();
                                for par in ["none", "even", "odd"] {
                                    let pv = par.to_string();
                                    let b = Button::new(SharedString::from(format!("par-{par}")))
                                        .label(par.to_string())
                                        .compact()
                                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                                            this.conn_parity = pv.clone();
                                            cx.notify();
                                        }));
                                    let b = if cur == par { b.primary() } else { b };
                                    pr = pr.child(b);
                                }
                                pr
                            },
                        )),
                );
        } else {
            form = form.child(
                div()
                    .h_flex()
                    .gap_3()
                    .child(self.field_wrap("主机", Input::new(&self.conn_host).h(px(30.)).w(px(180.))))
                    .child(self.field_wrap("端口", Input::new(&self.conn_port).h(px(30.)).w(px(80.)))),
            );
        }

        form = form.child(self.field_wrap("超时(ms)", Input::new(&self.conn_timeout).h(px(30.)).w(px(100.))));

        if let Some(err) = &self.conn_error {
            form = form.child(div().text_sm().text_color(theme::red()).child(err.clone()));
        }

        form = form.child(
            div()
                .h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("conn-cancel")
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, _w, cx| {
                            this.conn_dialog_open = false;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("conn-ok")
                        .label("连接")
                        .primary()
                        .on_click(cx.listener(|this, _ev, _w, cx| this.do_connect(cx))),
                ),
        );

        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::gpui::rgba(0x00000088))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .v_flex()
                    .gap_3()
                    .w(px(520.))
                    .p_5()
                    .bg(theme::panel())
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .text_color(theme::text())
                            .child("连接设置"),
                    )
                    .child(form),
            )
    }

    fn render_write_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is32 = is_32bit(&self.write_fmt);
        let title = if is32 {
            format!("写入寄存器 {} & {}", self.write_addr, self.write_addr + 1)
        } else {
            format!("写入寄存器 {}", self.write_addr)
        };
        let (label, _, _, _) = range_label(&self.write_fmt);
        let hint = format!(
            "{} @{} — 范围：{}；支持十六进制（0x 前缀）",
            format_name(&self.write_fmt),
            self.write_addr,
            label
        );

        let mut form = div().v_flex().gap_3()
            .child(div().text_sm().text_color(theme::text_dim()).child(hint))
            .child(Input::new(&self.write_value).h(px(34.)));
        if let Some(err) = &self.write_error {
            form = form.child(div().text_sm().text_color(theme::red()).child(err.clone()));
        }
        form = form.child(
            div()
                .h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("w-cancel")
                        .label("取消")
                        .on_click(cx.listener(|this, _ev, _w, cx| {
                            this.write_dialog_open = false;
                            if this.write_requires_restart && this.connected {
                                this.write_requires_restart = false;
                                this.toggle_polling(cx);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("w-ok")
                        .label("写入")
                        .primary()
                        .on_click(cx.listener(|this, _ev, _w, cx| this.confirm_write(cx))),
                ),
        );

        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::gpui::rgba(0x00000088))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .v_flex()
                    .gap_3()
                    .w(px(460.))
                    .p_5()
                    .bg(theme::panel())
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme::border())
                    .child(div().text_lg().font_semibold().text_color(theme::text()).child(title))
                    .child(form),
            )
    }
}
