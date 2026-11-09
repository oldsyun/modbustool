//! Unit-ID scanner window.
//!
//! Probes holding registers at a chosen address across Unit IDs 1..=247 and
//! reports which units respond, showing them as green pills.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::gpui::{
    div, px, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Window,
};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable, StyledExt};
use gpui_kit::gpui::prelude::FluentBuilder;

use crate::core::service::Service;
use crate::ui::common::theme;

pub struct ScannerWindow {
    service: Arc<Service>,
    addr_input: Entity<InputState>,
    scanning: bool,
    stop_flag: Arc<AtomicBool>,
    responders: Vec<u8>,
    scanned: u32,
    error: Option<String>,
}

impl ScannerWindow {
    pub fn new(service: Arc<Service>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let addr_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("如 0 或 0x100")
                .default_value("0")
        });
        Self {
            service,
            addr_input,
            scanning: false,
            stop_flag: Arc::new(AtomicBool::new(false)),
            responders: Vec::new(),
            scanned: 0,
            error: None,
        }
    }

    fn parse_address(text: &str) -> Option<u16> {
        let t = text.trim();
        if t.is_empty() {
            return Some(0);
        }
        let parsed = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            t.parse::<u32>().ok()
        };
        parsed.and_then(|v| u16::try_from(v).ok())
    }

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        if self.scanning {
            return;
        }
        let text = self.addr_input.read(cx).value().to_string();
        let addr = match Self::parse_address(&text) {
            Some(a) => a,
            None => {
                self.error =
                    Some("地址无效（请输入 0–65535 的十进制，或 0x 开头的十六进制）".to_string());
                cx.notify();
                return;
            }
        };

        self.scanning = true;
        self.error = None;
        self.responders.clear();
        self.scanned = 0;
        self.stop_flag.store(false, Ordering::SeqCst);

        let service = self.service.clone();
        let stop = self.stop_flag.clone();

        cx.spawn(async move |this, cx| {
            for unit_id in 1u8..=247u8 {
                if stop.load(Ordering::SeqCst) {
                    break;
                }

                let ok = service
                    .read_points(unit_id, "03".into(), addr, 1)
                    .await
                    .is_ok();

                // Stop may have been requested while this probe was in flight.
                let stopped = stop.load(Ordering::SeqCst);

                let alive = this
                    .update(cx, |this, cx| {
                        if !stopped {
                            this.scanned = unit_id as u32;
                            if ok {
                                this.responders.push(unit_id);
                            }
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive || stopped {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            let _ = this.update(cx, |this, cx| {
                this.scanning = false;
                cx.notify();
            });
        })
        .detach();

        cx.notify();
    }

    fn stop_scan(&mut self, cx: &mut Context<Self>) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.scanning = false;
        cx.notify();
    }
}

impl Render for ScannerWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scanning = self.scanning;
        let scanned = self.scanned;
        let resp_count = self.responders.len();
        let responders = self.responders.clone();
        let error = self.error.clone();

        let status_text: SharedString = if let Some(err) = &error {
            err.clone().into()
        } else if scanning {
            format!("扫描中… 已探测 {scanned} / 247").into()
        } else if scanned == 0 && resp_count == 0 {
            "等待扫描".into()
        } else if resp_count == 0 {
            "扫描完成 · 未发现响应的 Unit ID".into()
        } else {
            format!("扫描完成 · 共发现 {resp_count} 个响应").into()
        };
        let status_color = if error.is_some() {
            theme::red()
        } else if scanning {
            theme::amber()
        } else {
            theme::text_dim()
        };

        div()
            .v_flex()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text())
            // ── header ──────────────────────────────────────────
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .px_5()
                    .py_4()
                    .bg(theme::panel())
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::accent())
                                    .child("MODBUS DISCOVERY"),
                            )
                            .child(div().text_xl().child("Unit ID 扫描")),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_baseline()
                            .gap_1()
                            .child(
                                div()
                                    .text_2xl()
                                    .text_color(theme::green())
                                    .child(format!("{resp_count}")),
                            )
                            .child(div().text_sm().text_color(theme::text_dim()).child("个响应")),
                    ),
            )
            // ── controls ────────────────────────────────────────
            .child(
                div()
                    .h_flex()
                    .items_end()
                    .gap_3()
                    .px_5()
                    .py_4()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::text_dim())
                                    .child("保持寄存器地址"),
                            )
                            .child(
                                div()
                                    .w(px(180.))
                                    .child(Input::new(&self.addr_input).h(px(30.))),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .pb(px(6.))
                            .child(div().text_sm().text_color(theme::text_dim()).child("已扫描"))
                            .child(
                                div()
                                    .text_base()
                                    .text_color(theme::text())
                                    .child(format!("{scanned}")),
                            )
                            .child(div().text_sm().text_color(theme::text_dim()).child("/ 247")),
                    )
                    .child(
                        Button::new("start_scan")
                            .primary()
                            .label("开始扫描")
                            .disabled(scanning)
                            .on_click(cx.listener(|this, _ev, _w, cx| this.start_scan(cx))),
                    )
                    .child(
                        Button::new("stop_scan")
                            .label("停止扫描")
                            .disabled(!scanning)
                            .on_click(cx.listener(|this, _ev, _w, cx| this.stop_scan(cx))),
                    ),
            )
            // ── results ─────────────────────────────────────────
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .mx_5()
                    .mb_5()
                    .rounded_md()
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .justify_between()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(theme::border())
                            .child(div().text_sm().text_color(theme::text_dim()).child("响应地址"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(status_color)
                                    .child(status_text),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .p_4()
                            .overflow_hidden()
                            .when(responders.is_empty(), |el| {
                                el.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme::text_dim())
                                        .child(if scanning {
                                            "正在依次探测 Unit ID…"
                                        } else if scanned > 0 {
                                            "未发现响应的 Unit ID"
                                        } else {
                                            "点击“开始扫描”探测 1 至 247 号 Unit ID"
                                        }),
                                )
                            })
                            .when(!responders.is_empty(), |el| {
                                el.child(
                                    div().h_flex().flex_wrap().gap_2().children(
                                        responders.into_iter().map(|id| {
                                            div()
                                                .px_3()
                                                .py_1()
                                                .rounded_full()
                                                .bg(theme::green())
                                                .text_color(theme::bg())
                                                .text_sm()
                                                .child(format!("{id}"))
                                        }),
                                    ),
                                )
                            }),
                    ),
            )
    }
}
