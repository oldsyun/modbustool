//! Pure serial-port debugging window (independent of Modbus framing).
//!
//! Simple terminal: pick a port + parameters, connect, send text or HEX,
//! optionally repeat-send, view RX with timestamps, toggle HEX display,
//! pause/auto-scroll, clear, export log, and see TX/RX byte counters.

use std::sync::atomic::{AtomicBool, Ordering};
use crate::core::service::ORDERING;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_kit::gpui::prelude::FluentBuilder;
use gpui_kit::gpui::{
    div, px, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Task, Window,
};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable, StyledExt};

use crate::core::service::serial::{SerialConfig, SerialStatus};
use crate::core::service::{Event, Service};
use crate::ui::common::{bridge_events, theme};

const BAUD_PRESETS: &[u32] = &[1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600];
const PARITIES: &[(&str, &str)] = &[("none", "无校验"), ("odd", "奇校验"), ("even", "偶校验")];
const FLOWS: &[(&str, &str)] = &[("none", "无流控"), ("rtscts", "RTS/CTS"), ("xonxoff", "XON/XOFF")];
const LINE_ENDINGS: &[(&str, &str)] = &[("none", "无"), ("cr", "CR"), ("lf", "LF"), ("crlf", "CRLF")];

#[derive(Clone, Copy, PartialEq)]
enum DisplayMode {
    Text,
    Hex,
}

pub struct SerialWindow {
    service: Arc<Service>,
    _events: Option<Task<()>>,

    // connection params
    ports: Vec<String>,
    port_idx: usize,
    baud_idx: usize,
    data_bits: u8,
    stop_bits: u8,
    parity_idx: usize,
    flow_idx: usize,
    dtr: bool,
    rts: bool,
    connected: bool,
    status_msg: Option<String>,

    // input fields
    custom_baud: Entity<InputState>,
    tx_input: Entity<InputState>,

    // display state
    rx_lines: Vec<RxLine>,
    paused: bool,
    auto_scroll: bool,
    show_hex: bool,
    tx_hex: bool,
    line_ending_idx: usize,

    // repeat send
    repeat_on: bool,
    repeat_stop: Arc<AtomicBool>,
    repeat_handle: Option<Task<()>>,
    repeat_ms: u64,
    repeat_ms_input: Entity<InputState>,

    tx_bytes: u64,
    rx_bytes: u64,
}

struct RxLine {
    ts_ms: u64,
    bytes: Vec<u8>,
}

fn fmt_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let frac = ms % 1000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}.{frac:03}")
}

fn bytes_to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02X}", x)).collect::<Vec<_>>().join(" ")
}

fn bytes_to_text(b: &[u8]) -> String {
    // UTF-8 / ASCII: valid UTF-8 decoded; invalid bytes shown as replacement.
    String::from_utf8_lossy(b).to_string()
}

impl SerialWindow {
    pub fn new(service: Arc<Service>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let custom_baud = cx.new(|cx| InputState::new(window, cx).placeholder("115200"));
        let tx_input = cx.new(|cx| InputState::new(window, cx).placeholder("输入要发送的内容…"));
        let repeat_ms_input = cx.new(|cx| InputState::new(window, cx).default_value("1000"));

        let svc = service.clone();
        let _events = Some(bridge_events(svc, cx, |this: &mut Self, ev, _cx| match ev {
            Event::SerialRx { bytes, ts_ms } => {
                this.rx_bytes += bytes.len() as u64;
                if !this.paused {
                    this.rx_lines.push(RxLine { bytes, ts_ms });
                    if this.rx_lines.len() > 10000 {
                        this.rx_lines.drain(0..2000);
                    }
                }
            }
            Event::SerialState { open, error } => {
                this.connected = open;
                if let Some(e) = error {
                    this.status_msg = Some(e);
                }
            }
            _ => {}
        }));

        let mut me = Self {
            service,
            _events,
            ports: Vec::new(),
            port_idx: 0,
            baud_idx: 7, // 115200
            data_bits: 8,
            stop_bits: 1,
            parity_idx: 0,
            flow_idx: 0,
            dtr: true,
            rts: false,
            connected: false,
            status_msg: None,
            custom_baud,
            tx_input,
            rx_lines: Vec::new(),
            paused: false,
            auto_scroll: true,
            show_hex: false,
            tx_hex: false,
            line_ending_idx: 0,
            repeat_on: false,
            repeat_stop: Arc::new(AtomicBool::new(false)),
            repeat_handle: None,
            repeat_ms: 1000,
            repeat_ms_input,
            tx_bytes: 0,
            rx_bytes: 0,
        };
        me.refresh_ports(cx);
        me
    }

    fn refresh_ports(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let ports = svc.list_serial_ports().await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.ports = ports;
                if this.port_idx >= this.ports.len() {
                    this.port_idx = 0;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn current_baud(&self, cx: &Context<Self>) -> u32 {
        let custom = self.custom_baud.read(cx).value().to_string();
        if let Ok(v) = custom.trim().parse::<u32>() {
            if v > 0 {
                return v;
            }
        }
        BAUD_PRESETS.get(self.baud_idx).copied().unwrap_or(115200)
    }

    fn build_config(&self, cx: &Context<Self>) -> Option<SerialConfig> {
        let port = self.ports.get(self.port_idx)?.clone();
        Some(SerialConfig {
            port,
            baud: self.current_baud(cx),
            data_bits: self.data_bits,
            stop_bits: self.stop_bits,
            parity: PARITIES[self.parity_idx].0.to_string(),
            flow: FLOWS[self.flow_idx].0.to_string(),
            dtr: self.dtr,
            rts: self.rts,
            read_timeout_ms: 50,
        })
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.connected {
            return;
        }
        let cfg = match self.build_config(cx) {
            Some(c) => c,
            None => {
                self.status_msg = Some("请选择串口".into());
                cx.notify();
                return;
            }
        };
        let svc = self.service.clone();
        self.status_msg = Some("正在连接…".into());
        cx.spawn(async move |this, cx| {
            let res = svc.serial_open(cfg).await;
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(()) => {
                        this.connected = true;
                        this.status_msg = None;
                    }
                    Err(e) => {
                        this.connected = false;
                        this.status_msg = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.stop_repeat();
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            svc.serial_close().await;
            let _ = this.update(cx, |this, cx| {
                this.connected = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Encode the TX input into bytes according to mode + line ending.
    fn encode_tx(&self, cx: &Context<Self>) -> Result<Vec<u8>, String> {
        let raw = self.tx_input.read(cx).value().to_string();
        let mut bytes: Vec<u8> = if self.tx_hex {
            let cleaned: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
            if cleaned.len() % 2 != 0 {
                return Err("HEX 长度必须为偶数".into());
            }
            let mut out = Vec::with_capacity(cleaned.len() / 2);
            let chars: Vec<char> = cleaned.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let hi = chars[i];
                let lo = chars[i + 1];
                let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16)
                    .map_err(|_| "HEX 含非法字符".to_string())?;
                out.push(byte);
                i += 2;
            }
            out
        } else {
            raw.into_bytes()
        };
        match LINE_ENDINGS[self.line_ending_idx].0 {
            "cr" => bytes.push(0x0D),
            "lf" => bytes.push(0x0A),
            "crlf" => {
                bytes.push(0x0D);
                bytes.push(0x0A);
            }
            _ => {}
        }
        Ok(bytes)
    }

    fn send_once(&mut self, cx: &mut Context<Self>) {
        if !self.connected {
            self.status_msg = Some("请先连接串口".into());
            cx.notify();
            return;
        }
        let bytes = match self.encode_tx(cx) {
            Ok(b) => b,
            Err(e) => {
                self.status_msg = Some(e);
                cx.notify();
                return;
            }
        };
        if bytes.is_empty() {
            return;
        }
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let res = svc.serial_send(bytes).await;
            let _ = this.update(cx, |this, cx| {
                this.tx_bytes = this.service.serial_status().tx_bytes;
                if let Err(e) = res {
                    this.status_msg = Some(e);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_repeat(&mut self, cx: &mut Context<Self>) {
        if self.repeat_on {
            self.stop_repeat();
            cx.notify();
            return;
        }
        if !self.connected {
            self.status_msg = Some("请先连接串口".into());
            cx.notify();
            return;
        }
        if let Ok(v) = self.repeat_ms_input.read(cx).value().to_string().trim().parse::<u64>() {
            if v > 0 {
                self.repeat_ms = v;
            }
        }
        self.repeat_on = true;
        self.repeat_stop.store(false, ORDERING);
        let stop = self.repeat_stop.clone();
        let svc = self.service.clone();
        let interval = self.repeat_ms;
        // Capture the encoded frame now (repeat uses the same bytes).
        let bytes = match self.encode_tx(cx) {
            Ok(b) => b,
            Err(e) => {
                self.repeat_on = false;
                self.status_msg = Some(e);
                cx.notify();
                return;
            }
        };
        let handle = cx.spawn(async move |this, cx| {
            while !stop.load(ORDERING) {
                let _ = svc.serial_send(bytes.clone()).await;
                let _ = this.update(cx, |this, cx| {
                    this.tx_bytes = svc.serial_status().tx_bytes;
                    cx.notify();
                });
                tokio::time::sleep(Duration::from_millis(interval)).await;
            }
        });
        self.repeat_handle = Some(handle);
        cx.notify();
    }

    fn stop_repeat(&mut self) {
        self.repeat_stop.store(true, ORDERING);
        self.repeat_handle = None;
        self.repeat_on = false;
    }

    fn export_log(&mut self, cx: &mut Context<Self>) {
        let mut out = String::new();
        for line in &self.rx_lines {
            let body = if self.show_hex {
                bytes_to_hex(&line.bytes)
            } else {
                bytes_to_text(&line.bytes)
            };
            out.push_str(&format!("[{}] RX: {}\n", fmt_ts(line.ts_ms), body));
        }
        let path = rfd::FileDialog::new()
            .add_filter("文本文件", &["txt", "log"])
            .set_file_name("serial_log.txt")
            .save_file();
        if let Some(p) = path {
            let _ = std::fs::write(p, out);
            self.status_msg = Some("日志已导出".into());
        }
        cx.notify();
    }
}

impl Drop for SerialWindow {
    fn drop(&mut self) {
        self.repeat_stop.store(true, ORDERING);
    }
}

impl Render for SerialWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = "串口调试";
        let ports_empty = self.ports.is_empty();
        let port_label = self
            .ports
            .get(self.port_idx)
            .cloned()
            .unwrap_or_else(|| "无可用串口".to_string());

        // ── connection bar ──
        let mut conn_bar = div()
            .h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .px_3()
            .py_2()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border());

        conn_bar = conn_bar
            .child(div().text_sm().child("端口"))
            .child(
                Button::new("port")
                    .label(SharedString::from(port_label))
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        if !this.connected && !this.ports.is_empty() {
                            this.port_idx = (this.port_idx + 1) % this.ports.len();
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("refresh")
                    .label("↻")
                    .on_click(cx.listener(|this, _ev, _w, cx| this.refresh_ports(cx))),
            )
            .child(div().text_sm().child("波特率"))
            .child(
                Button::new("baud")
                    .label(format!("{}", self.current_baud(cx)))
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        if !this.connected {
                            this.baud_idx = (this.baud_idx + 1) % BAUD_PRESETS.len();
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("databits")
                    .label(format!("{} 数据位", self.data_bits))
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        if !this.connected {
                            this.data_bits = if this.data_bits == 8 { 7 } else { 8 };
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("parity")
                    .label(PARITIES[self.parity_idx].1)
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        if !this.connected {
                            this.parity_idx = (this.parity_idx + 1) % PARITIES.len();
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("stopbits")
                    .label(format!("{} 停止位", self.stop_bits))
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        if !this.connected {
                            this.stop_bits = if this.stop_bits == 1 { 2 } else { 1 };
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("flow")
                    .label(FLOWS[self.flow_idx].1)
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        if !this.connected {
                            this.flow_idx = (this.flow_idx + 1) % FLOWS.len();
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("dtr")
                    .label(if self.dtr { "DTR ✓" } else { "DTR" })
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.dtr = !this.dtr;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("rts")
                    .label(if self.rts { "RTS ✓" } else { "RTS" })
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.rts = !this.rts;
                        cx.notify();
                    })),
            );

        if self.connected {
            conn_bar = conn_bar.child(
                Button::new("disconnect")
                    .danger()
                    .label("断开")
                    .on_click(cx.listener(|this, _ev, _w, cx| this.disconnect(cx))),
            );
        } else {
            conn_bar = conn_bar.child(
                Button::new("connect")
                    .primary()
                    .label("连接")
                    .when(ports_empty, |b| b.disabled(true))
                    .on_click(cx.listener(|this, _ev, _w, cx| this.connect(cx))),
            );
        }

        let status_text = if self.connected {
            format!(
                "● 已连接 {} {}  TX:{} B  RX:{} B",
                self.ports.get(self.port_idx).cloned().unwrap_or_default(),
                self.current_baud(cx),
                self.tx_bytes,
                self.rx_bytes
            )
        } else {
            self.status_msg.clone().unwrap_or_else(|| "未连接".to_string())
        };
        let status_color = if self.connected { theme::green() } else { theme::text_dim() };

        // ── RX pane ──
        let mut rx_list = div()
            .id("serial-rx")
            .flex_1()
            .overflow_y_scroll()
            .p_3()
            .bg(theme::surface())
            .text_sm()
            .text_color(theme::text())
            .font_family("monospace");
        if self.rx_lines.is_empty() {
            rx_list = rx_list.child(
                div()
                    .text_color(theme::text_tertiary())
                    .child("接收区：连接后将在此显示收到的数据（文本 / HEX）"),
            );
        } else {
            let show_hex = self.show_hex;
            for line in &self.rx_lines {
                let body = if show_hex {
                    bytes_to_hex(&line.bytes)
                } else {
                    bytes_to_text(&line.bytes)
                };
                rx_list = rx_list.child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(div().text_color(theme::text_tertiary()).child(fmt_ts(line.ts_ms)))
                        .child(div().child(body)),
                );
            }
        }

        let rx_toolbar = div()
            .h_flex()
            .gap_2()
            .items_center()
            .px_3()
            .py_1()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border_light())
            .child(
                Button::new("toggle_ts")
                    .label("时间戳"),
            )
            .child(
                Button::new("toggle_hex")
                    .label(if self.show_hex { "HEX 显示 ✓" } else { "HEX 显示" })
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.show_hex = !this.show_hex;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("autoscroll")
                    .label(if self.auto_scroll { "自动滚动 ✓" } else { "自动滚动" })
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.auto_scroll = !this.auto_scroll;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("pause")
                    .label(if self.paused { "继续" } else { "暂停" })
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.paused = !this.paused;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("clear")
                    .label("清空")
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.rx_lines.clear();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("export")
                    .label("导出 TXT")
                    .on_click(cx.listener(|this, _ev, _w, cx| this.export_log(cx))),
            );

        // ── TX area ──
        let tx_bar = div()
            .v_flex()
            .gap_2()
            .p_3()
            .bg(theme::panel())
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("tx_mode")
                            .label(if self.tx_hex { "HEX" } else { "文本" })
                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                this.tx_hex = !this.tx_hex;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().child(Input::new(&self.tx_input).h(px(34.))))
                    .child(
                        Button::new("send")
                            .primary()
                            .label("发送")
                            .when(!self.connected, |b| b.disabled(true))
                            .on_click(cx.listener(|this, _ev, _w, cx| this.send_once(cx))),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().text_color(theme::text_dim()).child("行结束"))
                    .child(
                        Button::new("line_end")
                            .label(LINE_ENDINGS[self.line_ending_idx].1)
                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                this.line_ending_idx =
                                    (this.line_ending_idx + 1) % LINE_ENDINGS.len();
                                cx.notify();
                            })),
                    )
                    .child(div().text_sm().text_color(theme::text_dim()).child("循环发送"))
                    .child(
                        Button::new("repeat")
                            .label(if self.repeat_on { "停止" } else { "开始" })
                            .when(self.repeat_on, |b| b.primary())
                            .on_click(cx.listener(|this, _ev, _w, cx| this.toggle_repeat(cx))),
                    )
                    .child(div().w(px(80.)).child(Input::new(&self.repeat_ms_input).h(px(30.))))
                    .child(div().text_sm().text_color(theme::text_dim()).child("ms")),
            );

        div()
            .v_flex()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text())
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .bg(theme::panel())
                    .border_b_1()
                    .border_color(theme::border())
                    .child(div().text_lg().child(title))
                    .child(div().flex_1())
                    .child(div().text_sm().text_color(status_color).child(status_text)),
            )
            .child(conn_bar)
            .child(rx_toolbar)
            .child(div().flex_1().min_h(px(0.)).child(rx_list))
            .child(tx_bar)
    }
}
