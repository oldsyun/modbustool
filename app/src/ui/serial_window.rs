//! Pure serial-port debugging window (independent of Modbus framing).
//!
//! Simple terminal: pick a port + parameters, connect, send text or HEX,
//! optionally repeat-send, view RX with timestamps, toggle HEX display,
//! pause/auto-scroll, clear, export log, and see TX/RX byte counters.

use std::sync::atomic::AtomicBool;
use crate::core::service::ORDERING;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::gpui::prelude::FluentBuilder;
use gpui_kit::gpui::{
    div, px, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Task, Window,
};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable, StyledExt};

use crate::core::service::serial::SerialConfig;
use crate::core::service::SharedSerialParams;
use crate::core::service::{Event, Service};
use crate::ui::common::{bridge_events, theme};

const LINE_ENDINGS: &[(&str, &str)] = &[("none", "无"), ("cr", "CR"), ("lf", "LF"), ("crlf", "CRLF")];

pub struct SerialWindow {
    service: Arc<Service>,
    _events: Option<Task<()>>,

    // connection: reused from the main page's RTU connection
    shared: Option<SharedSerialParams>,
    connected: bool,
    status_msg: Option<String>,

    // input fields
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

        let shared = service.shared_serial_params();
        let me = Self {
            service,
            _events,
            shared,
            connected: false,
            status_msg: None,
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
        me
    }

    /// Re-read the main page's current RTU serial parameters (call when the
    /// window is shown or the main page reconnects).
    fn reload_shared(&mut self) {
        self.shared = self.service.shared_serial_params();
    }

    /// Keep the displayed summary in sync with the main page while disconnected.
    fn reload_shared_if_closed(&mut self) {
        if !self.connected {
            self.reload_shared();
        }
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.connected {
            return;
        }
        self.reload_shared();
        let shared = match &self.shared {
            Some(p) if !p.port.trim().is_empty() => p.clone(),
            _ => {
                self.status_msg =
                    Some("主页面尚未使用 RTU 串口连接，请先在主页面「连接」中选择串口并填写参数".into());
                cx.notify();
                return;
            }
        };
        let cfg = SerialConfig {
            port: shared.port,
            baud: shared.baud,
            data_bits: shared.data_bits,
            stop_bits: shared.stop_bits,
            parity: shared.parity,
            flow: "none".into(),
            dtr: true,
            rts: false,
            read_timeout_ms: 50,
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
        // Connection parameters are reused from the main page's RTU connection.
        self.reload_shared_if_closed();
        let shared = self.shared.clone();

        let param_summary = match &shared {
            Some(p) if !p.port.trim().is_empty() => format!(
                "复用主页面串口：{}  {}  {}  {}{}{}",
                p.port,
                p.baud,
                match p.data_bits {
                    5 => "5",
                    6 => "6",
                    7 => "7",
                    _ => "8",
                },
                match p.parity.to_ascii_lowercase().as_str() {
                    "odd" => "O",
                    "even" => "E",
                    _ => "N",
                },
                p.stop_bits,
                "  位/校验/停止位"
            ),
            _ => "主页面尚未使用 RTU 串口连接（请先在主页面「连接」中配置串口）".to_string(),
        };
        let has_params = matches!(&shared, Some(p) if !p.port.trim().is_empty());

        // ── connection bar (read-only summary + connect/disconnect) ──
        let mut conn_bar = div()
            .h_flex()
            .gap_1()
            .items_center()
            .flex_wrap()
            .px_2()
            .py_1()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border());

        conn_bar = conn_bar
            .child(div().text_sm().text_color(theme::text_dim()).child("连接参数"))
            .child(
                div()
                    .text_sm()
                    .text_color(if has_params { theme::text() } else { theme::text_tertiary() })
                    .child(param_summary),
            );

        if self.connected {
            conn_bar = conn_bar.child(
                Button::new("disconnect")
                    .danger()
                    .compact()
                    .label("断开")
                    .on_click(cx.listener(|this, _ev, _w, cx| this.disconnect(cx))),
            );
        } else {
            conn_bar = conn_bar.child(
                Button::new("connect")
                    .primary()
                    .compact()
                    .label("连接")
                    .when(!has_params, |b| b.disabled(true))
                    .on_click(cx.listener(|this, _ev, _w, cx| this.connect(cx))),
            );
        }

        let status_text = if self.connected {
            let (port, baud) = shared
                .as_ref()
                .map(|p| (p.port.clone(), p.baud))
                .unwrap_or_default();
            format!(
                "● 已连接 {} {}  TX:{} B  RX:{} B",
                port, baud, self.tx_bytes, self.rx_bytes
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
            .gap_1()
            .items_center()
            .px_2()
            .py_1()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border_light())
            .child(
                Button::new("toggle_ts")
                .compact()
                    .label("时间戳"),
            )
            .child(
                Button::new("toggle_hex")
                .compact()
                    .label(if self.show_hex { "HEX 显示 ✓" } else { "HEX 显示" })
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.show_hex = !this.show_hex;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("autoscroll")
                .compact()
                    .label(if self.auto_scroll { "自动滚动 ✓" } else { "自动滚动" })
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.auto_scroll = !this.auto_scroll;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("pause")
                .compact()
                    .label(if self.paused { "继续" } else { "暂停" })
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.paused = !this.paused;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("clear")
                .compact()
                    .label("清空")
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        this.rx_lines.clear();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("export")
                .compact()
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
                .compact()
                            .label(if self.tx_hex { "HEX" } else { "文本" })
                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                this.tx_hex = !this.tx_hex;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().child(Input::new(&self.tx_input).h(px(34.))))
                    .child(
                        Button::new("send")
                .compact()
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
                .compact()
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
                .compact()
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
