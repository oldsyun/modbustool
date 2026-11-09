//! Simulator window — native port of the "从站模拟器" tool.
//!
//! Layout:
//!   ┌ top bar ──────────────────────────────────────────────┐
//!   │ 标题 · Unit chips · 加/删 Unit · 通道设置 · Excel · 示例 │
//!   ├ area tabs ───────────────────────────────────────────┤
//!   │ 保持寄存器 / 输入寄存器 / 线圈 / 离散输入 (+计数)        │
//!   ├ register table ──────────────────────────────────────┤
//!   │ 地址 · 名称 · 类型 · 访问 · 自动变化 · 值 · HEX · 操作   │
//!   └───────────────────────────────────────────────────────┘
//! Overlays (in-window panels, not OS modals): register edit, delete confirm,
//! duplicate-address replace, and channel settings.

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Selectable, StyledExt};
use gpui_kit::gpui::{
    div, px, AnyElement, AppContext, Context, Div, Focusable, InteractiveElement as _, Entity,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled,
    Task, Window,
};

use crate::core::reg::{RegInput, RegisterDef};
use crate::core::service::{Event, Service, SlaveStartReq};
use crate::ui::common::{bridge_events, field, theme};

// ── label helpers ────────────────────────────────────────────────
fn area_label(area: &str) -> &'static str {
    match area {
        "holding" => "保持寄存器",
        "input" => "输入寄存器",
        "coil" => "线圈",
        "discrete" => "离散输入",
        _ => "未知区域",
    }
}

fn dtype_label(dtype: &str) -> &'static str {
    match dtype {
        "u16" => "U16",
        "i16" => "I16",
        "u32" => "U32",
        "i32" => "I32",
        "f32" => "F32",
        "bit" => "BOOL",
        "bits" => "位图",
        _ => "—",
    }
}

fn vary_label(vary: &str) -> &'static str {
    match vary {
        "off" => "关闭",
        "sine" => "正弦",
        "random" => "随机",
        "increment" => "自增",
        _ => "—",
    }
}

fn mode_label(mode: &str) -> &'static str {
    match mode {
        "tcp" => "Modbus TCP",
        "udp" => "Modbus UDP",
        "rtu_tcp" => "RTU over TCP",
        "rtu_udp" => "RTU over UDP",
        "rtu" => "RTU（串口）",
        _ => "—",
    }
}

fn is_bit_area(area: &str) -> bool {
    area == "coil" || area == "discrete"
}

fn hex16(v: u16) -> String {
    format!("0x{v:04X}")
}

fn bit_string(v: u16) -> String {
    // little-endian bit ordering, matching the web tool (B0 is rightmost slice).
    let mut s = String::with_capacity(16);
    for i in 0..16 {
        s.push(if (v >> i) & 1 == 1 { '1' } else { '0' });
    }
    s
}

fn display_value(dtype: &str, raw: u16) -> i64 {
    match dtype {
        "i16" => {
            if raw & 0x8000 != 0 {
                raw as i64 - 0x1_0000
            } else {
                raw as i64
            }
        }
        _ => raw as i64,
    }
}

fn raw_value(dtype: &str, v: i64) -> u16 {
    let mut n = v;
    if dtype == "i16" && n < 0 {
        n += 0x1_0000;
    }
    n.clamp(0, 65535) as u16
}

// ── dialogs / overlays ───────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialog {
    None,
    Edit,
    ConfirmDelete,
    Replace,
    Channel,
    Batch,
}

// ── the view ─────────────────────────────────────────────────────
pub struct SimulatorWindow {
    service: Arc<Service>,
    _events: Option<Task<()>>,

    active_unit: u8,
    units: Vec<u8>,
    defs: Vec<RegisterDef>,
    snapshot: Option<ImageSnapshot>,
    tab_area: String,
    mode_status: serde_json::Value,

    dialog: Dialog,

    // persistent per-register value inputs (editable numeric cells)
    row_inputs: std::collections::HashMap<String, Entity<InputState>>,

    // unit add input
    unit_ws: Entity<InputState>,

    // register edit form
    edit_id: Option<String>,
    form_area: String,
    form_dtype: String,
    form_access: String,
    form_vary: String,
    addr_ws: Entity<InputState>,
    name_ws: Entity<InputState>,
    value_ws: Entity<InputState>,
    form_error: Option<String>,

    // batch form
    batch_start_ws: Entity<InputState>,
    batch_count_ws: Entity<InputState>,

    // duplicate-replace state
    pending_replace_id: Option<String>,

    // delete state
    pending_delete_id: Option<String>,
    confirm_desc: String,

    // channel settings
    sel_mode: String,
    bind_ws: Entity<InputState>,
    port_ws: Entity<InputState>,
    serial_ws: Entity<InputState>,
    baud_ws: Entity<InputState>,
    databits_ws: Entity<InputState>,
    stopbits_ws: Entity<InputState>,
    parity_ws: Entity<InputState>,
    inter_ws: Entity<InputState>,
    serial_ports: Vec<String>,
    channel_msg: Option<String>,
}

/// Mirror of `modbus_core::server::ImageSnapshot` (fields kept snake_case here).
#[derive(Clone, Default)]
struct ImageSnapshot {
    coils: std::collections::HashMap<u16, bool>,
    discrete_inputs: std::collections::HashMap<u16, bool>,
    input_registers: std::collections::HashMap<u16, u16>,
    holding_registers: std::collections::HashMap<u16, u16>,
}

impl From<modbus_core::server::ImageSnapshot> for ImageSnapshot {
    fn from(s: modbus_core::server::ImageSnapshot) -> Self {
        Self {
            coils: s.coils,
            discrete_inputs: s.discrete_inputs,
            input_registers: s.input_registers,
            holding_registers: s.holding_registers,
        }
    }
}

impl SimulatorWindow {
    pub fn new(service: Arc<Service>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let unit_ws = cx.new(|cx| InputState::new(window, cx).placeholder("Unit ID"));
        let addr_ws = cx.new(|cx| InputState::new(window, cx).default_value("0"));
        let name_ws = cx.new(|cx| InputState::new(window, cx).placeholder("名称"));
        let value_ws = cx.new(|cx| InputState::new(window, cx).default_value("0"));
        let batch_start_ws = cx.new(|cx| InputState::new(window, cx).default_value("0"));
        let batch_count_ws = cx.new(|cx| InputState::new(window, cx).default_value("10"));
        let bind_ws = cx.new(|cx| InputState::new(window, cx).default_value("0.0.0.0"));
        let port_ws = cx.new(|cx| InputState::new(window, cx).default_value("502"));
        let serial_ws = cx.new(|cx| InputState::new(window, cx).placeholder("/dev/tty.usbserial"));
        let baud_ws = cx.new(|cx| InputState::new(window, cx).default_value("9600"));
        let databits_ws = cx.new(|cx| InputState::new(window, cx).default_value("8"));
        let stopbits_ws = cx.new(|cx| InputState::new(window, cx).default_value("1"));
        let parity_ws = cx.new(|cx| InputState::new(window, cx).default_value("none"));
        let inter_ws = cx.new(|cx| InputState::new(window, cx).placeholder("默认 5"));

        let svc = service.clone();
        let _events = Some(bridge_events(svc, cx, |this: &mut Self, ev, cx| {
            match ev {
                Event::SlaveValues { unit_id, snapshot } => {
                    if unit_id == this.active_unit {
                        this.snapshot = Some(ImageSnapshot::from(snapshot));
                        cx.notify();
                    }
                }
                Event::SimRegsUpdated(resp) => {
                    if resp.unit_id == this.active_unit {
                        this.defs = resp.defs;
                        this.snapshot = Some(ImageSnapshot::from(resp.snapshot));
                        cx.notify();
                    }
                }
                _ => {}
            }
        }));

        let mut this = Self {
            service,
            _events,
            active_unit: 1,
            units: Vec::new(),
            defs: Vec::new(),
            snapshot: None,
            tab_area: "holding".into(),
            mode_status: serde_json::json!({}),
            dialog: Dialog::None,
            row_inputs: std::collections::HashMap::new(),
            unit_ws,
            edit_id: None,
            form_area: "holding".into(),
            form_dtype: "u16".into(),
            form_access: "rw".into(),
            form_vary: "off".into(),
            addr_ws,
            name_ws,
            value_ws,
            form_error: None,
            batch_start_ws,
            batch_count_ws,
            pending_replace_id: None,
            pending_delete_id: None,
            confirm_desc: String::new(),
            sel_mode: "tcp".into(),
            bind_ws,
            port_ws,
            serial_ws,
            baud_ws,
            databits_ws,
            stopbits_ws,
            parity_ws,
            inter_ws,
            serial_ports: Vec::new(),
            channel_msg: None,
        };

        this.bootstrap_loads(window, cx);
        this
    }

    fn bootstrap_loads(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.reload_units(cx);
        self.refresh_status(cx);
        self.refresh_serial_ports(cx);
    }

    // ── data loading (async) ────────────────────────────────────
    fn reload_units(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let list = svc.unit_list().await;
            let mut ids: Vec<u8> = list
                .iter()
                .filter_map(|v| v.get("unit_id").and_then(|x| x.as_u64()).map(|n| n as u8))
                .collect();
            ids.sort();
            let uid = this
                .update(cx, |this, cx| {
                    this.units = ids;
                    if !this.units.contains(&this.active_unit) {
                        this.active_unit = this.units.first().copied().unwrap_or(1);
                    }
                    cx.notify();
                    this.active_unit
                })
                .unwrap_or(1);
            // then load regs for the active unit
            if let Ok(resp) = svc.sim_reg_list(None, Some(uid)).await {
                let _ = this.update(cx, |this, cx| {
                    this.defs = resp.defs;
                    this.snapshot = Some(ImageSnapshot::from(resp.snapshot));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn reload_regs(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        let uid = self.active_unit;
        cx.spawn(async move |this, cx| {
            if let Ok(resp) = svc.sim_reg_list(None, Some(uid)).await {
                let _ = this.update(cx, |this, cx| {
                    this.defs = resp.defs;
                    this.snapshot = Some(ImageSnapshot::from(resp.snapshot));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let st = svc.sim_slave_status().await;
            let _ = this.update(cx, |this, cx| {
                this.mode_status = st;
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_serial_ports(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(ports) = svc.list_serial_ports().await {
                let _ = this.update(cx, |this, cx| {
                    if this.serial_ports != ports {
                        this.serial_ports = ports;
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    // ── snapshot helpers ────────────────────────────────────────
    fn live_of(&self, d: &RegisterDef) -> u16 {
        let Some(snap) = &self.snapshot else { return 0 };
        if is_bit_area(&d.area) {
            let on = if d.area == "coil" {
                snap.coils.get(&d.addr).copied().unwrap_or(false)
            } else {
                snap.discrete_inputs.get(&d.addr).copied().unwrap_or(false)
            };
            if on { 1 } else { 0 }
        } else if d.area == "holding" {
            snap.holding_registers.get(&d.addr).copied().unwrap_or(0)
        } else {
            snap.input_registers.get(&d.addr).copied().unwrap_or(0)
        }
    }

    fn area_counts(&self) -> (usize, usize, usize, usize) {
        let (mut h, mut i, mut c, mut dsc) = (0, 0, 0, 0);
        for d in &self.defs {
            match d.area.as_str() {
                "holding" => h += 1,
                "input" => i += 1,
                "coil" => c += 1,
                "discrete" => dsc += 1,
                _ => {}
            }
        }
        (h, i, c, dsc)
    }

    fn visible_defs(&self) -> Vec<RegisterDef> {
        let mut v: Vec<RegisterDef> = self
            .defs
            .iter()
            .filter(|d| d.area == self.tab_area)
            .cloned()
            .collect();
        v.sort_by_key(|d| d.addr);
        v
    }

    fn suggest_addr(&self, area: &str) -> u16 {
        let used: std::collections::HashSet<u16> = self
            .defs
            .iter()
            .filter(|d| d.area == area)
            .map(|d| d.addr)
            .collect();
        let mut a = 0u16;
        while used.contains(&a) && a < u16::MAX {
            a += 1;
        }
        a
    }

    // ── register set (live value write) ─────────────────────────
    fn set_value(&mut self, d: &RegisterDef, raw: u16, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        let uid = self.active_unit;
        let area = d.area.clone();
        let addr = d.addr as usize;
        cx.spawn(async move |_this, _cx| {
            match area.as_str() {
                "holding" => {
                    let _ = svc.sim_set_register(addr, raw, Some(uid)).await;
                }
                "input" => {
                    let _ = svc.sim_set_input(addr, raw, Some(uid)).await;
                }
                "coil" => {
                    let _ = svc.sim_set_coil(addr, raw != 0, Some(uid)).await;
                }
                "discrete" => {
                    let _ = svc.sim_set_discrete(addr, raw != 0, Some(uid)).await;
                }
                _ => {}
            }
        })
        .detach();
    }

    // ── dialog openers ──────────────────────────────────────────
    fn open_edit(&mut self, d: Option<&RegisterDef>, window: &mut Window, cx: &mut Context<Self>) {
        self.form_error = None;
        match d {
            Some(d) => {
                self.edit_id = Some(d.id.clone());
                self.form_area = d.area.clone();
                self.form_dtype = d.dtype.clone();
                self.form_access = d.access.clone();
                self.form_vary = d.vary.clone();
                let raw = self.live_of(d);
                self.addr_ws
                    .update(cx, |s, cx| s.set_value(d.addr.to_string(), window, cx));
                self.name_ws
                    .update(cx, |s, cx| s.set_value(d.name.clone(), window, cx));
                self.value_ws.update(cx, |s, cx| s.set_value(raw.to_string(), window, cx));
                self.batch_count_ws
                    .update(cx, |s, cx| s.set_value("1", window, cx));
            }
            None => {
                self.edit_id = None;
                let area = self.tab_area.clone();
                self.form_area = area.clone();
                self.form_dtype = "u16".into();
                self.form_access = if area == "input" || area == "discrete" {
                    "ro".into()
                } else {
                    "rw".into()
                };
                self.form_vary = "off".into();
                let suggested = self.suggest_addr(&area);
                self.addr_ws
                    .update(cx, |s, cx| s.set_value(suggested.to_string(), window, cx));
                self.name_ws.update(cx, |s, cx| s.set_value("", window, cx));
                self.value_ws.update(cx, |s, cx| s.set_value("0", window, cx));
                self.batch_count_ws
                    .update(cx, |s, cx| s.set_value("1", window, cx));
                self.batch_start_ws
                    .update(cx, |s, cx| s.set_value(suggested.to_string(), window, cx));
            }
        }
        self.dialog = Dialog::Edit;
        cx.notify();
    }

    fn open_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.channel_msg = None;
        // Prefill from running config if present.
        let cfg = self.mode_status.get("configs").cloned();
        if let Some(cfg) = cfg {
            if let Some(c) = cfg.get(&self.sel_mode) {
                if let Some(b) = c.get("bind").and_then(|v| v.as_str()) {
                    self.bind_ws.update(cx, |s, cx| s.set_value(b, window, cx));
                }
                if let Some(p) = c.get("port").and_then(|v| v.as_u64()) {
                    self.port_ws
                        .update(cx, |s, cx| s.set_value(p.to_string(), window, cx));
                }
                if let Some(pn) = c.get("portName").and_then(|v| v.as_str()) {
                    self.serial_ws
                        .update(cx, |s, cx| s.set_value(pn, window, cx));
                }
                if let Some(br) = c.get("baudRate").and_then(|v| v.as_u64()) {
                    self.baud_ws
                        .update(cx, |s, cx| s.set_value(br.to_string(), window, cx));
                }
            }
        }
        self.refresh_serial_ports(cx);
        self.dialog = Dialog::Channel;
        cx.notify();
    }

    fn open_batch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form_error = None;
        self.edit_id = None;
        let area = self.tab_area.clone();
        self.form_area = area.clone();
        self.form_dtype = if is_bit_area(&area) { "bit".into() } else { "u16".into() };
        self.form_access = if area == "input" || area == "discrete" {
            "ro".into()
        } else {
            "rw".into()
        };
        self.form_vary = "off".into();
        let suggested = self.suggest_addr(&area);
        self.batch_start_ws
            .update(cx, |s, cx| s.set_value(suggested.to_string(), window, cx));
        self.name_ws.update(cx, |s, cx| s.set_value("", window, cx));
        self.value_ws.update(cx, |s, cx| s.set_value("0", window, cx));
        self.dialog = Dialog::Batch;
        cx.notify();
    }

    // ── register save ───────────────────────────────────────────
    fn collect_input(&self, cx: &mut Context<Self>) -> RegInput {
        let addr: u16 = self
            .addr_ws
            .read(cx)
            .value()
            .trim()
            .parse()
            .unwrap_or(0);
        let name = self.name_ws.read(cx).value().trim().to_string();
        let value: f64 = self.value_ws.read(cx).value().trim().parse().unwrap_or(0.0);
        RegInput {
            area: self.form_area.clone(),
            addr,
            name,
            dtype: self.form_dtype.clone(),
            access: self.form_access.clone(),
            vary: if self.form_area == "holding" || self.form_area == "input" {
                self.form_vary.clone()
            } else {
                "off".into()
            },
            value,
        }
    }

    fn save_reg(&mut self, cx: &mut Context<Self>) {
        let input = self.collect_input(cx);
        if input.addr == 0 && self.addr_ws.read(cx).value().trim().is_empty() {
            self.form_error = Some("请填写地址 (0 ~ 65535)".into());
            cx.notify();
            return;
        }
        let editing = self.edit_id.clone();
        let uid = self.active_unit;
        let svc = self.service.clone();

        // Duplicate detection for new registers: offer replace.
        if editing.is_none() {
            let dup = self
                .defs
                .iter()
                .find(|d| d.area == input.area && d.addr == input.addr)
                .cloned();
            if let Some(dup) = dup {
                self.pending_replace_id = Some(dup.id.clone());
                self.confirm_desc = format!(
                    "{} 0x{:04X}「{}」已存在。是否用新配置替换它？",
                    area_label(&input.area),
                    input.addr,
                    if dup.name.is_empty() { "未命名" } else { &dup.name }
                );
                self.dialog = Dialog::Replace;
                cx.notify();
                return;
            }
        }

        cx.spawn(async move |this, cx| {
            let res = match &editing {
                Some(id) => svc.sim_reg_update(id.clone(), input, Some(uid)).await,
                None => svc.sim_reg_add(input, Some(uid)).await.map(|_| ()),
            };
            let _ = this.update(cx, |this, cx| match res {
                Ok(()) => {
                    this.form_error = None;
                    this.dialog = Dialog::None;
                    cx.notify();
                }
                Err(e) => {
                    this.form_error = Some(e);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn do_replace(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_replace_id.clone() else { return };
        let input = self.collect_input(cx);
        let uid = self.active_unit;
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let res = svc.sim_reg_update(id, input, Some(uid)).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = res {
                    this.form_error = Some(e);
                } else {
                    this.dialog = Dialog::None;
                }
                this.pending_replace_id = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn do_save_batch(&mut self, cx: &mut Context<Self>) {
        let start: u16 = self
            .batch_start_ws
            .read(cx)
            .value()
            .trim()
            .parse()
            .unwrap_or(0);
        let count: u16 = self
            .batch_count_ws
            .read(cx)
            .value()
            .trim()
            .parse()
            .unwrap_or(0);
        if count == 0 {
            self.form_error = Some("数量须大于 0".into());
            cx.notify();
            return;
        }
        let area = self.form_area.clone();
        let dtype = self.form_dtype.clone();
        let access = self.form_access.clone();
        let vary = if area == "holding" || area == "input" {
            self.form_vary.clone()
        } else {
            "off".into()
        };
        let prefix = self.name_ws.read(cx).value().trim().to_string();
        let init = self.value_ws.read(cx).value().trim().parse().unwrap_or(0.0);
        let uid = self.active_unit;
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let res = svc
                .sim_reg_add_batch(
                    area, start, count, prefix, dtype, access, vary, init, Some(uid),
                )
                .await;
            let _ = this.update(cx, |this, cx| match res {
                Ok(_) => {
                    this.dialog = Dialog::None;
                    this.form_error = None;
                    cx.notify();
                }
                Err(e) => {
                    this.form_error = Some(e);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn delete_reg(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_delete_id.clone() else { return };
        let uid = self.active_unit;
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let res = svc.sim_reg_delete(id, Some(uid)).await;
            let _ = this.update(cx, |this, cx| {
                if res.is_ok() {
                    this.dialog = Dialog::None;
                }
                this.pending_delete_id = None;
                cx.notify();
            });
        })
        .detach();
    }

    // ── unit add / remove ───────────────────────────────────────
    fn add_unit(&mut self, cx: &mut Context<Self>) {
        let raw = self.unit_ws.read(cx).value().trim().to_string();
        let uid: u8 = raw.parse().unwrap_or(0);
        if uid == 0 || uid > 247 {
            self.channel_msg = Some("Unit ID 须在 1 ~ 247".into());
            cx.notify();
            return;
        }
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let res = svc.unit_add(uid).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = res {
                    this.channel_msg = Some(e);
                    cx.notify();
                } else {
                    this.channel_msg = None;
                }
            });
            this.update(cx, |this, cx| this.reload_units(cx)).ok();
        })
        .detach();
    }

    fn remove_unit(&mut self, cx: &mut Context<Self>) {
        if self.units.len() <= 1 {
            self.channel_msg = Some("至少保留一个 Unit ID".into());
            cx.notify();
            return;
        }
        let uid = self.active_unit;
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let res = svc.unit_remove(uid).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = res {
                    this.channel_msg = Some(e);
                    cx.notify();
                } else {
                    this.channel_msg = None;
                }
            });
            this.update(cx, |this, cx| this.reload_units(cx)).ok();
        })
        .detach();
    }

    fn select_unit(&mut self, uid: u8, cx: &mut Context<Self>) {
        if self.active_unit != uid {
            self.active_unit = uid;
            self.defs.clear();
            self.snapshot = None;
            self.reload_regs(cx);
            cx.notify();
        }
    }

    // ── maintenance actions ─────────────────────────────────────
    fn do_seed(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        let uid = self.active_unit;
        cx.spawn(async move |this, cx| {
            let _ = svc.sim_reg_seed(Some(uid)).await;
            let _ = this.update(cx, |this, cx| {
                this.reload_regs(cx);
            });
        })
        .detach();
    }

    fn do_reset(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        let uid = self.active_unit;
        cx.spawn(async move |this, cx| {
            let _ = svc.sim_reset(Some(uid)).await;
            let _ = this.update(cx, |this, cx| {
                this.reload_regs(cx);
            });
        })
        .detach();
    }

    fn do_export(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        let uid = self.active_unit;
        cx.spawn(async move |this, cx| {
            let res = svc.sim_reg_export_xlsx(Some(uid)).await;
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(p) if !p.is_empty() => {
                        this.channel_msg = Some(format!("已导出：{p}"));
                    }
                    Ok(_) => {}
                    Err(e) => this.channel_msg = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn do_import(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        let uid = self.active_unit;
        cx.spawn(async move |this, cx| {
            let res = svc.sim_reg_import_xlsx(Some(uid), false).await;
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok((ok, skip)) => {
                        if ok > 0 || skip > 0 {
                            this.channel_msg = Some(format!("导入完成：新增 {ok}，跳过 {skip}"));
                        }
                    }
                    Err(e) => this.channel_msg = Some(e),
                }
                this.reload_regs(cx);
            });
        })
        .detach();
    }

    // ── channel start / stop ────────────────────────────────────
    fn start_channel(&mut self, cx: &mut Context<Self>) {
        let mode = self.sel_mode.clone();
        let req = if mode == "rtu" {
            let port_name = self.serial_ws.read(cx).value().trim().to_string();
            if port_name.is_empty() {
                self.channel_msg = Some("请先选择/填写串口".into());
                cx.notify();
                return;
            }
            SlaveStartReq {
                mode: mode.clone(),
                bind: None,
                port: None,
                port_name: Some(port_name),
                baud_rate: Some(
                    self.baud_ws.read(cx).value().trim().parse().unwrap_or(9600),
                ),
                data_bits: Some(self.databits_ws.read(cx).value().trim().parse().unwrap_or(8)),
                stop_bits: Some(self.stopbits_ws.read(cx).value().trim().parse().unwrap_or(1)),
                parity: Some(self.parity_ws.read(cx).value().trim().to_string()),
                inter_frame_ms: self
                    .inter_ws
                    .read(cx)
                    .value()
                    .trim()
                    .parse()
                    .ok(),
            }
        } else {
            let bind = {
                let b = self.bind_ws.read(cx).value().trim().to_string();
                if b.is_empty() { "0.0.0.0".into() } else { b }
            };
            SlaveStartReq {
                mode: mode.clone(),
                bind: Some(bind),
                port: Some(self.port_ws.read(cx).value().trim().parse().unwrap_or(502)),
                port_name: None,
                baud_rate: None,
                data_bits: None,
                stop_bits: None,
                parity: None,
                inter_frame_ms: None,
            }
        };
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let res = svc.sim_slave_start(req).await;
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(()) => {
                        this.channel_msg = None;
                        this.dialog = Dialog::None;
                    }
                    Err(e) => this.channel_msg = Some(e),
                }
                cx.notify();
            });
            this.update(cx, |this, cx| this.refresh_status(cx)).ok();
        })
        .detach();
    }

    fn stop_channel(&mut self, mode: String, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let _ = svc.sim_slave_stop(mode).await;
            let _ = this.update(cx, |this, cx| this.refresh_status(cx));
        })
        .detach();
    }

    fn stop_all(&mut self, cx: &mut Context<Self>) {
        let svc = self.service.clone();
        cx.spawn(async move |this, cx| {
            let _ = svc.sim_slave_stop_all().await;
            let _ = this.update(cx, |this, cx| this.refresh_status(cx));
        })
        .detach();
    }

    fn mode_running(&self, mode: &str) -> bool {
        self.mode_status
            .get("modes")
            .and_then(|m| m.get(mode))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}

// ── rendering ─────────────────────────────────────────────────────
const COL_ADDR: f32 = 150.;
const COL_NAME: f32 = 170.;
const COL_TYPE: f32 = 78.;
const COL_ACCESS: f32 = 68.;
const COL_VARY: f32 = 90.;
const COL_VALUE: f32 = 160.;
const COL_HEX: f32 = 92.;
const COL_OP: f32 = 132.;

impl Render for SimulatorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .relative()
            .v_flex()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text())
            .child(self.render_top_bar(cx))
            .child(self.render_area_bar(cx))
            .child(self.render_table(window, cx));

        match self.dialog {
            Dialog::Edit => root = root.child(self.render_edit_overlay(window, cx)),
            Dialog::Batch => root = root.child(self.render_batch_overlay(window, cx)),
            Dialog::ConfirmDelete => root = root.child(self.render_delete_overlay(cx)),
            Dialog::Replace => root = root.child(self.render_replace_overlay(cx)),
            Dialog::Channel => root = root.child(self.render_channel_overlay(window, cx)),
            Dialog::None => {}
        }
        root
    }
}

/// A full-window, dimmed overlay that centers a panel.
fn overlay(panel: impl IntoElement) -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .h_flex()
        .items_center()
        .justify_center()
        .bg(gpui_kit::gpui::rgba(0x00000088))
        .child(panel)
}

fn panel_card(width: f32) -> Div {
    div()
        .v_flex()
        .w(px(width))
        .gap_3()
        .bg(theme::panel())
        .border_1()
        .border_color(theme::border())
        .rounded_lg()
        .shadow_lg()
        .p_4()
}

fn dialog_header(title: &str) -> Div {
    div()
        .h_flex()
        .items_center()
        .pb_2()
        .border_b_1()
        .border_color(theme::border())
        .child(div().text_lg().font_semibold().child(title.to_string()))
}

fn section_label(text: &str) -> Div {
    div().text_sm().text_color(theme::text_dim()).child(text.to_string())
}

impl SimulatorWindow {
    fn render_top_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut unit_row = div().h_flex().flex_wrap().gap_2().items_center();
        unit_row = unit_row.child(div().text_sm().text_color(theme::text_dim()).child("Unit"));
        for uid in self.units.clone() {
            let active = uid == self.active_unit;
            let b = Button::new(("unit", uid as u64))
                .label(format!("Unit {uid}"))
                .compact()
                .selected(active);
            let b = if active { b.primary() } else { b };
            unit_row = unit_row.child(b.on_click(cx.listener(move |this, _ev, _w, cx| {
                this.select_unit(uid, cx);
            })));
        }
        unit_row = unit_row
            .child(div().w(px(86.)).child(Input::new(&self.unit_ws).h(px(28.))))
            .child(
                Button::new("unit-add")
                    .label("+ Unit")
                    .compact()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.add_unit(cx))),
            )
            .child(
                Button::new("unit-del")
                    .label("− Unit")
                    .compact()
                    .danger()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.remove_unit(cx))),
            );

        let right = div()
            .h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("chan-settings")
                    .label("⚙ 通道设置")
                    .compact()
                    .on_click(cx.listener(|this, _ev, w, cx| this.open_channel(w, cx))),
            )
            .child(
                Button::new("import")
                    .label("📂 导入 Excel")
                    .compact()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.do_import(cx))),
            )
            .child(
                Button::new("export")
                    .label("💾 导出 Excel")
                    .compact()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.do_export(cx))),
            )
            .child(
                Button::new("seed")
                    .label("恢复示例")
                    .compact()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.do_seed(cx))),
            )
            .child(
                Button::new("reset")
                    .label("清零")
                    .compact()
                    .on_click(cx.listener(|this, _ev, _w, cx| this.do_reset(cx))),
            );

        let msg = self.channel_msg.clone();

        div()
            .v_flex()
            .gap_1()
            .px_2()
            .py_1()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_base().font_bold().child("从站模拟器"))
                    .child(div().flex_1())
                    .child(right),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(unit_row)
                    .child(div().flex_1())
                    .child(if let Some(m) = msg {
                        div().text_sm().text_color(theme::amber()).child(m)
                    } else {
                        div()
                    }),
            )
    }

    fn render_area_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (h, i, c, dsc) = self.area_counts();
        let counts = [h, i, c, dsc];
        let areas = ["holding", "input", "coil", "discrete"];
        let mut tabs = div().h_flex().gap_1().items_center();
        for (idx, area) in areas.iter().enumerate() {
            let active = self.tab_area == *area;
            let cnt = counts[idx];
            let label = if cnt > 0 {
                format!("{} ({})", area_label(area), cnt)
            } else {
                area_label(area).to_string()
            };
            let a = area.to_string();
            let b = Button::new(("tab", idx as u64))
                .label(label)
                .compact()
                .selected(active);
            let b = if active { b.primary() } else { b };
            tabs = tabs.child(b.on_click(cx.listener(move |this, _ev, _w, cx| {
                if this.tab_area != a {
                    this.tab_area = a.clone();
                    cx.notify();
                }
            })));
        }

        let total = self.defs.len();
        div()
            .h_flex()
            .gap_3()
            .items_center()
            .px_3()
            .py_2()
            .bg(theme::bg())
            .border_b_1()
            .border_color(theme::border())
            .child(tabs)
            .child(div().text_sm().text_color(theme::text_dim()).child(format!("共 {total} 个")))
            .child(div().flex_1())
            .child(
                Button::new("add-batch")
                    .label("+ 批量")
                    .compact()
                    .on_click(cx.listener(|this, _ev, w, cx| this.open_batch(w, cx))),
            )
            .child(
                Button::new("add-reg")
                    .label("+ 新增寄存器")
                    .compact()
                    .primary()
                    .on_click(cx.listener(|this, _ev, w, cx| this.open_edit(None, w, cx))),
            )
    }

    fn header_cell(label: &str, width: f32) -> impl IntoElement {
        div()
            .w(px(width))
            .flex_shrink_0()
            .px_2()
            .py_1()
            .text_sm()
            .font_medium()
            .text_color(theme::text_dim())
            .child(label.to_string())
    }

    fn render_table(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible = self.visible_defs();

        let header = div()
            .h_flex()
            .items_center()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .child(Self::header_cell("地址", COL_ADDR))
            .child(Self::header_cell("名称", COL_NAME))
            .child(Self::header_cell("类型", COL_TYPE))
            .child(Self::header_cell("访问", COL_ACCESS))
            .child(Self::header_cell("自动变化", COL_VARY))
            .child(Self::header_cell("值", COL_VALUE))
            .child(Self::header_cell("HEX", COL_HEX))
            .child(Self::header_cell("操作", COL_OP));

        let mut body = div().v_flex().w_full();
        if visible.is_empty() {
            body = body.child(
                div().p_6().text_sm().text_color(theme::text_dim()).child(format!(
                    "「{}」暂无寄存器，点击「+ 新增寄存器」添加。",
                    area_label(&self.tab_area)
                )),
            );
        } else {
            for (idx, d) in visible.iter().enumerate() {
                let row = self.render_row(idx, d, window, cx);
                body = body.child(row);
            }
        }

        div()
            .v_flex()
            .flex_1()
            .min_h(px(0.))
            .child(header)
            .child(
                div()
                    .id("sim-table-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(body),
            )
    }

    fn render_row(
        &mut self,
        idx: usize,
        d: &RegisterDef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_bit = is_bit_area(&d.area);
        let raw = self.live_of(d);
        let row_bg = if idx % 2 == 0 { theme::bg() } else { theme::panel() };
        let id_key = d.id.clone();
        let def_edit = d.clone();
        let def_del = d.clone();
        let def_live = d.clone();

        let value_cell: AnyElement = if is_bit {
            let on = raw != 0;
            let toggled = if on { 0u16 } else { 1u16 };
            let b = Button::new(SharedString::from(format!("bit-{}", d.id)))
                .label(if on { "ON" } else { "OFF" })
                .compact();
            let b = if on { b.success() } else { b.ghost() };
            div()
                .w(px(COL_VALUE))
                .flex_shrink_0()
                .px_2()
                .h_flex()
                .items_center()
                .child(b.on_click({
                    let dl = def_live.clone();
                    cx.listener(move |this, _ev, _w, cx| {
                        this.set_value(&dl, toggled, cx);
                    })
                }))
                .into_any_element()
        } else if d.dtype == "bits" {
            div()
                .w(px(COL_VALUE))
                .flex_shrink_0()
                .px_2()
                .h_flex()
                .items_center()
                .text_sm()
                .font_family("monospace")
                .child(bit_string(raw))
                .into_any_element()
        } else {
            let disp = display_value(&d.dtype, raw);
            let state = self.row_input(d, disp, window, cx);
            div()
                .w(px(COL_VALUE))
                .flex_shrink_0()
                .px_2()
                .h_flex()
                .items_center()
                .child(Input::new(&state).h(px(28.)))
                .into_any_element()
        };

        div()
            .h_flex()
            .items_center()
            .min_h(px(38.))
            .bg(row_bg)
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .w(px(COL_ADDR))
                    .flex_shrink_0()
                    .px_2()
                    .text_sm()
                    .font_family("monospace")
                    .child(format!("0x{:04X} ({})", d.addr, d.addr)),
            )
            .child(
                div()
                    .w(px(COL_NAME))
                    .flex_shrink_0()
                    .px_2()
                    .text_sm()
                    .child(if d.name.is_empty() {
                        "—".to_string()
                    } else {
                        d.name.clone()
                    }),
            )
            .child(
                div()
                    .w(px(COL_TYPE))
                    .flex_shrink_0()
                    .px_2()
                    .text_sm()
                    .child(dtype_label(&d.dtype)),
            )
            .child(
                div()
                    .w(px(COL_ACCESS))
                    .flex_shrink_0()
                    .px_2()
                    .text_sm()
                    .text_color(if d.access == "ro" {
                        theme::text_dim()
                    } else {
                        theme::text()
                    })
                    .child(if d.access == "ro" { "只读" } else { "读写" }),
            )
            .child(
                div()
                    .w(px(COL_VARY))
                    .flex_shrink_0()
                    .px_2()
                    .text_sm()
                    .text_color(if d.area == "holding" && d.vary != "off" {
                        theme::accent()
                    } else {
                        theme::text_dim()
                    })
                    .child(if d.area == "holding" || d.area == "input" {
                        vary_label(&d.vary)
                    } else {
                        "—"
                    }),
            )
            .child(value_cell)
            .child(
                div()
                    .w(px(COL_HEX))
                    .flex_shrink_0()
                    .px_2()
                    .text_sm()
                    .font_family("monospace")
                    .text_color(theme::text_dim())
                    .child(hex16(raw)),
            )
            .child(
                div()
                    .w(px(COL_OP))
                    .flex_shrink_0()
                    .px_2()
                    .h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Button::new(SharedString::from(format!("edit-{}", id_key)))
                            .label("编辑")
                            .compact()
                            .on_click(cx.listener(move |this, _ev, w, cx| {
                                this.open_edit(Some(&def_edit), w, cx);
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("del-{}", id_key)))
                            .label("删除")
                            .compact()
                            .danger()
                            .on_click(cx.listener(move |this, _ev, _w, cx| {
                                this.pending_delete_id = Some(def_del.id.clone());
                                this.confirm_desc = format!(
                                    "确定要删除 {} 0x{:04X}「{}」吗？删除后该地址读回为 0。",
                                    area_label(&def_del.area),
                                    def_del.addr,
                                    if def_del.name.is_empty() {
                                        "未命名"
                                    } else {
                                        &def_del.name
                                    }
                                );
                                this.dialog = Dialog::ConfirmDelete;
                                cx.notify();
                            })),
                    ),
            )
    }

    // ── overlays ────────────────────────────────────────────────
    fn render_edit_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.edit_id.is_some();
        let title = if editing { "编辑寄存器" } else { "新增寄存器" };
        let is_bit = is_bit_area(&self.form_area);

        let mut area_row = div().h_flex().gap_1().flex_wrap();
        for (ai, a) in ["holding", "input", "coil", "discrete"].iter().enumerate() {
            let active = self.form_area == *a;
            let av = a.to_string();
            let b = Button::new(("farea", ai as u64))
                .label(area_label(a))
                .compact();
            let b = if active { b.primary() } else { b };
            area_row = area_row.child(b.on_click(cx.listener(move |this, _ev, w, cx| {
                if this.form_area != av {
                    this.form_area = av.clone();
                    if is_bit_area(&av) {
                        this.form_dtype = "bit".into();
                        this.form_vary = "off".into();
                        if av == "discrete" {
                            this.form_access = "ro".into();
                        }
                    } else if this.form_dtype == "bit" {
                        this.form_dtype = "u16".into();
                    }
                    if av == "input" {
                        this.form_access = "ro".into();
                    }
                    this.value_ws.update(cx, |s, cx| s.set_value("0", w, cx));
                    cx.notify();
                }
            })));
        }

        let dtype_row: AnyElement = if is_bit {
            div()
                .h_flex()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme::surface())
                .child(div().text_sm().text_color(theme::text_dim()).child("BOOL（布尔）"))
                .into_any_element()
        } else {
            let mut r = div().h_flex().gap_1().flex_wrap();
            for (di, dt) in ["u16", "i16", "u32", "i32", "f32", "bits"].iter().enumerate() {
                let active = self.form_dtype == *dt;
                let dv = dt.to_string();
                let b = Button::new(("fdtype", di as u64))
                    .label(dtype_label(dt))
                    .compact();
                let b = if active { b.primary() } else { b };
                r = r.child(b.on_click(cx.listener(move |this, _ev, _w, cx| {
                    if this.form_dtype != dv {
                        this.form_dtype = dv.clone();
                        cx.notify();
                    }
                })));
            }
            r.into_any_element()
        };

        let mut access_row = div().h_flex().gap_1();
        for (ai, a) in ["rw", "ro"].iter().enumerate() {
            let active = self.form_access == *a;
            let av = a.to_string();
            let b = Button::new(("faccess", ai as u64))
                .label(if *a == "rw" { "读写" } else { "只读" })
                .compact();
            let b = if active { b.primary() } else { b };
            access_row = access_row.child(b.on_click(cx.listener(move |this, _ev, _w, cx| {
                if this.form_access != av {
                    this.form_access = av.clone();
                    cx.notify();
                }
            })));
        }

        let vary_section: AnyElement = if self.form_area == "holding" || self.form_area == "input" {
            let mut r = div().h_flex().gap_1().flex_wrap();
            for (vi, v) in ["off", "sine", "random", "increment"].iter().enumerate() {
                let active = self.form_vary == *v;
                let vv = v.to_string();
                let b = Button::new(("fvary", vi as u64))
                    .label(vary_label(v))
                    .compact();
                let b = if active { b.primary() } else { b };
                r = r.child(b.on_click(cx.listener(move |this, _ev, _w, cx| {
                    if this.form_vary != vv {
                        this.form_vary = vv.clone();
                        cx.notify();
                    }
                })));
            }
            div()
                .v_flex()
                .gap_1()
                .child(section_label("自动变化"))
                .child(r)
                .into_any_element()
        } else {
            div().into_any_element()
        };

        let value_label = if is_bit { "初始值 (0/1)" } else { "初始值" };
        let err = self.form_error.clone();

        let panel = panel_card(520.)
            .child(dialog_header(title))
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(section_label("区域"))
                    .child(area_row),
            )
            .child(
                div()
                    .h_flex()
                    .gap_3()
                    .items_end()
                    .child(div().w(px(150.)).child(field("地址", Input::new(&self.addr_ws).h(px(30.)))))
                    .child(div().flex_1().child(field("名称/前缀", Input::new(&self.name_ws).h(px(30.))))),
            )
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(section_label("数据类型"))
                    .child(dtype_row),
            )
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(section_label("访问"))
                    .child(access_row),
            )
            .child(
                div()
                    .h_flex()
                    .gap_3()
                    .items_start()
                    .child(vary_section)
                    .child(div().w(px(160.)).child(field(value_label, Input::new(&self.value_ws).h(px(30.))))),
            )
            .child(if let Some(e) = err {
                div().text_sm().text_color(theme::red()).child(e)
            } else {
                div()
            })
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("edit-cancel").label("取消").compact().on_click(cx.listener(
                        |this, _ev, _w, cx| {
                            this.dialog = Dialog::None;
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("edit-save")
                            .label("保存")
                            .compact()
                            .primary()
                            .on_click(cx.listener(|this, _ev, _w, cx| this.save_reg(cx))),
                    ),
            );

        let _ = window;
        overlay(panel)
    }

    fn render_batch_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = "批量新增寄存器";
        let err = self.form_error.clone();
        let is_bit = is_bit_area(&self.form_area);

        let panel = panel_card(460.)
            .child(dialog_header(title))
            .child(field("区域", div().text_sm().child(area_label(&self.form_area))))
            .child(
                div()
                    .h_flex()
                    .gap_3()
                    .items_end()
                    .child(div().w(px(120.)).child(field("起始地址", Input::new(&self.batch_start_ws).h(px(30.)))))
                    .child(div().w(px(100.)).child(field("数量", Input::new(&self.batch_count_ws).h(px(30.))))),
            )
            .child(div().child(field("名称前缀", Input::new(&self.name_ws).h(px(30.)))))
            .child(div().child(field(
                if is_bit { "初始值 (0/1)" } else { "初始值" },
                Input::new(&self.value_ws).h(px(30.)),
            )))
            .child(if let Some(e) = err {
                div().text_sm().text_color(theme::red()).child(e)
            } else {
                div()
                    .text_sm()
                    .text_color(theme::text_dim())
                    .child("类型/访问继承当前区域默认（可先新增单个后在编辑框调整）")
            })
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("batch-cancel").label("取消").compact().on_click(cx.listener(
                        |this, _ev, _w, cx| {
                            this.dialog = Dialog::None;
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("batch-save")
                            .label("批量创建")
                            .compact()
                            .primary()
                            .on_click(cx.listener(|this, _ev, _w, cx| this.do_save_batch(cx))),
                    ),
            );

        let _ = window;
        overlay(panel)
    }

    fn render_delete_overlay(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let msg = self.confirm_desc.clone();
        let panel = panel_card(420.)
            .child(dialog_header("删除寄存器"))
            .child(div().text_sm().child(msg))
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("del-cancel").label("取消").compact().on_click(cx.listener(
                        |this, _ev, _w, cx| {
                            this.dialog = Dialog::None;
                            this.pending_delete_id = None;
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("del-confirm")
                            .label("确认删除")
                            .compact()
                            .danger()
                            .on_click(cx.listener(|this, _ev, _w, cx| this.delete_reg(cx))),
                    ),
            );
        overlay(panel)
    }

    fn render_replace_overlay(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let msg = self.confirm_desc.clone();
        let panel = panel_card(440.)
            .child(dialog_header("地址已存在"))
            .child(div().text_sm().child(msg))
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("rep-cancel").label("取消").compact().on_click(cx.listener(
                        |this, _ev, _w, cx| {
                            this.dialog = Dialog::Edit;
                            this.pending_replace_id = None;
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("rep-ok")
                            .label("替换")
                            .compact()
                            .primary()
                            .on_click(cx.listener(|this, _ev, _w, cx| this.do_replace(cx))),
                    ),
            );
        overlay(panel)
    }

    fn render_channel_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut mode_row = div().h_flex().gap_1().flex_wrap();
        for (mi, m) in ["tcp", "udp", "rtu_tcp", "rtu_udp", "rtu"].iter().enumerate() {
            let active = self.sel_mode == *m;
            let mv = m.to_string();
            let b = Button::new(("mode", mi as u64))
                .label(mode_label(m))
                .compact();
            let b = if active { b.primary() } else { b };
            mode_row = mode_row.child(b.on_click(cx.listener(move |this, _ev, _w, cx| {
                if this.sel_mode != mv {
                    this.sel_mode = mv.clone();
                    cx.notify();
                }
            })));
        }

        let is_serial = self.sel_mode == "rtu";

        let mut fields = div().v_flex().gap_2();
        if is_serial {
            let ports_cycle = self.serial_ports.clone();
            fields = fields
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .items_end()
                        .child(div().text_sm().text_color(theme::text_dim()).w(px(70.)).pb_2().child("串口"))
                        .child(div().flex_1().child(Input::new(&self.serial_ws).h(px(30.))))
                        .child(
                            Button::new("port-cycle")
                                .label("↻ 切换")
                                .compact()
                                .on_click(cx.listener(move |this, _ev, w, cx| {
                                    if ports_cycle.is_empty() {
                                        return;
                                    }
                                    let cur = this.serial_ws.read(cx).value().trim().to_string();
                                    let idx = ports_cycle.iter().position(|p| *p == cur).unwrap_or(0);
                                    let next = ports_cycle[(idx + 1) % ports_cycle.len()].clone();
                                    this.serial_ws.update(cx, |s, cx| s.set_value(next, w, cx));
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(div().w(px(120.)).child(field("波特率", Input::new(&self.baud_ws).h(px(30.)))))
                        .child(div().w(px(90.)).child(field("数据位", Input::new(&self.databits_ws).h(px(30.)))))
                        .child(div().w(px(90.)).child(field("停止位", Input::new(&self.stopbits_ws).h(px(30.))))),
                )
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(div().w(px(120.)).child(field("校验 none/even/odd", Input::new(&self.parity_ws).h(px(30.)))))
                        .child(div().w(px(120.)).child(field("帧间(ms)", Input::new(&self.inter_ws).h(px(30.))))),
                );
        } else {
            fields = fields.child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_end()
                    .child(div().flex_1().child(field("绑定地址", Input::new(&self.bind_ws).h(px(30.)))))
                    .child(div().w(px(120.)).child(field("端口", Input::new(&self.port_ws).h(px(30.))))),
            );
        }

        let running = self.mode_running(&self.sel_mode);
        let status_text = if running {
            let detail = if self.sel_mode == "rtu" {
                let pn = self.serial_ws.read(cx).value().trim().to_string();
                format!(" {pn}")
            } else {
                let p = self.port_ws.read(cx).value().trim().to_string();
                format!(" 端口:{p}")
            };
            format!("{} · 已运行{detail}", mode_label(&self.sel_mode))
        } else {
            "未连接".to_string()
        };
        let tcp_conns = self
            .mode_status
            .get("tcpConns")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let mut status_row = div().h_flex().gap_3().items_center().child(
            div()
                .text_sm()
                .text_color(if running { theme::green() } else { theme::text_dim() })
                .child(status_text),
        );
        if self.sel_mode == "tcp" && running {
            status_row = status_row
                .child(div().text_sm().text_color(theme::text_dim()).child(format!("({tcp_conns} 连接)")));
        }

        let err = self.channel_msg.clone();
        let mode = self.sel_mode.clone();
        let start_btn = if running {
            Button::new("chan-stop")
                .label("停止")
                .compact()
                .danger()
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    this.stop_channel(mode.clone(), cx);
                }))
        } else {
            Button::new("chan-start")
                .label("启动")
                .compact()
                .primary()
                .on_click(cx.listener(|this, _ev, _w, cx| this.start_channel(cx)))
        };

        let panel = panel_card(520.)
            .child(dialog_header("通道设置 (Channel Settings)"))
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(section_label("运行模式"))
                    .child(mode_row),
            )
            .child(fields)
            .child(if let Some(e) = err {
                div().text_sm().text_color(theme::red()).child(e)
            } else {
                div()
            })
            .child(status_row)
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("chan-all-stop")
                            .label("全部停止")
                            .compact()
                            .on_click(cx.listener(|this, _ev, _w, cx| this.stop_all(cx))),
                    )
                    .child(Button::new("chan-close").label("关闭").compact().on_click(cx.listener(
                        |this, _ev, _w, cx| {
                            this.dialog = Dialog::None;
                            this.channel_msg = None;
                            cx.notify();
                        },
                    )))
                    .child(start_btn),
            );

        let _ = window;
        overlay(panel)
    }

    /// Return (creating if needed) the persistent value InputState for a row,
    /// keeping it in sync with the live value unless the user is typing.
    fn row_input(
        &mut self,
        d: &RegisterDef,
        disp: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let key = d.id.clone();
        let ent = if let Some(e) = self.row_inputs.get(&key) {
            e.clone()
        } else {
            let init = disp.to_string();
            let e = cx.new(|cx| InputState::new(window, cx).default_value(init));
            self.row_inputs.insert(key.clone(), e.clone());
            let def = d.clone();
            cx.observe(&e, move |this, st, cx| {
                let text = st.read(cx).value().trim().to_string();
                if let Ok(parsed) = text.parse::<i64>() {
                    let raw = raw_value(&def.dtype, parsed);
                    if this.live_of(&def) != raw {
                        this.set_value(&def, raw, cx);
                    }
                }
                cx.notify();
            })
            .detach();
            e
        };

        let focused = ent.read(cx).focus_handle(cx).is_focused(window);
        if !focused {
            let cur = ent.read(cx).value().trim().to_string();
            let want = disp.to_string();
            if cur != want {
                ent.update(cx, |s, cx| s.set_value(want, window, cx));
            }
        }
        ent
    }
}
