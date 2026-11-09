//! Simulator (built-in slave) operations, register CRUD, Excel import/export,
//! and native file dialogs. Second half of the service impl.

use std::sync::Arc;
use std::time::Duration;

use calamine::{open_workbook_auto, Reader};
use modbus_core::server;
use modbus_core::simulator::VaryMode;
use rust_xlsxwriter::{Color, Format, Workbook};

use crate::core::reg::{area_label, default_reg_samples, gen_id, now_ms, RegInput, RegListResp, RegisterDef};
use super::{
    Event, Service, SlaveStartReq, SLAVE_RTU, SLAVE_RTU_TCP, SLAVE_RTU_UDP, SLAVE_TCP, SLAVE_UDP,
    ORDERING,
};

impl Service {
    // ── vary loop (drives auto-changing registers) ────────────────
    pub fn spawn_vary_loop(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick: u64 = 0;
            loop {
                tick = tick.wrapping_add(1);
                let _ = tick;
                tokio::time::sleep(Duration::from_millis(500)).await;
                if me.slave_mode.load(ORDERING) == 0 {
                    continue;
                }
                let units = me.slave_units.lock().unwrap();
                for u in units.iter() {
                    let (holding_varies, input_varies): (
                        Vec<(u16, VaryMode)>,
                        Vec<(u16, VaryMode)>,
                    ) = {
                        let regs = u.regs.lock().unwrap();
                        regs.iter().filter(|d| d.vary != "off").fold(
                            (Vec::new(), Vec::new()),
                            |(mut h, mut i), d| {
                                let mode = VaryMode::parse(&d.vary);
                                match d.area.as_str() {
                                    "holding" => h.push((d.addr, mode)),
                                    "input" => i.push((d.addr, mode)),
                                    _ => {}
                                }
                                (h, i)
                            },
                        )
                    };
                    if holding_varies.is_empty() && input_varies.is_empty() {
                        continue;
                    }
                    let mut s = me.simulator.lock().unwrap();
                    let mut img = u.image.lock().unwrap();
                    s.step_on(&mut img, &holding_varies, &input_varies);
                    let snap = server::snapshot_from(&img);
                    drop(img);
                    drop(s);
                    me.emit(Event::SlaveValues { unit_id: u.unit_id, snapshot: snap });
                }
            }
        });
    }

    fn on_write_snapshot(self: &Arc<Self>) -> server::OnWrite {
        let me = self.clone();
        Arc::new(move |unit_id| {
            let found = me
                .slave_units
                .lock()
                .unwrap()
                .iter()
                .find(|u| u.unit_id == unit_id)
                .map(|u| u.image.clone());
            if let Some(img) = found {
                let snap = server::snapshot(&img);
                me.emit(Event::SlaveValues { unit_id, snapshot: snap });
            }
        })
    }

    // ── slave start/stop ──────────────────────────────────────────
    pub async fn sim_slave_start(self: &Arc<Self>, req: SlaveStartReq) -> Result<(), String> {
        self.sim_slave_stop_inner(&req.mode);
        let units = self.unit_images.clone();
        let on_write = self.on_write_snapshot();
        if let Ok(cfg) = serde_json::to_value(&req) {
            self.slave_configs
                .lock()
                .unwrap()
                .insert(req.mode.clone(), cfg);
        }
        match req.mode.as_str() {
            "tcp" => {
                let bind = req.bind.clone().unwrap_or_else(|| "0.0.0.0".into());
                let port = req.port.unwrap_or(502);
                let rx = self.slave_tcp.shutdown.subscribe();
                let conns = self.slave_tcp.conns.clone();
                let handle = tokio::spawn(async move {
                    let _ = server::run_tcp_slave(units, on_write, &bind, port, rx, conns).await;
                });
                *self.slave_tcp.handle.lock().unwrap() = Some(handle);
                self.slave_mode.fetch_or(SLAVE_TCP, ORDERING);
                Ok(())
            }
            "udp" => {
                let bind = req.bind.clone().unwrap_or_else(|| "0.0.0.0".into());
                let port = req.port.unwrap_or(502);
                let rx = self.slave_udp.shutdown.subscribe();
                let handle = tokio::spawn(async move {
                    let _ = server::run_udp_slave(units, on_write, &bind, port, rx).await;
                });
                *self.slave_udp.handle.lock().unwrap() = Some(handle);
                self.slave_mode.fetch_or(SLAVE_UDP, ORDERING);
                Ok(())
            }
            "rtu_tcp" => {
                let bind = req.bind.clone().unwrap_or_else(|| "0.0.0.0".into());
                let port = req.port.unwrap_or(502);
                let rx = self.slave_rtu_tcp.shutdown.subscribe();
                let conns = self.slave_rtu_tcp.conns.clone();
                let handle = tokio::spawn(async move {
                    let _ = server::run_rtu_over_tcp_slave(units, on_write, &bind, port, rx, conns).await;
                });
                *self.slave_rtu_tcp.handle.lock().unwrap() = Some(handle);
                self.slave_mode.fetch_or(SLAVE_RTU_TCP, ORDERING);
                Ok(())
            }
            "rtu_udp" => {
                let bind = req.bind.clone().unwrap_or_else(|| "0.0.0.0".into());
                let port = req.port.unwrap_or(502);
                let rx = self.slave_rtu_udp.shutdown.subscribe();
                let handle = tokio::spawn(async move {
                    let _ = server::run_rtu_over_udp_slave(units, on_write, &bind, port, rx).await;
                });
                *self.slave_rtu_udp.handle.lock().unwrap() = Some(handle);
                self.slave_mode.fetch_or(SLAVE_RTU_UDP, ORDERING);
                Ok(())
            }
            "rtu" => {
                let port_name = req.port_name.clone().ok_or("请选择串口")?;
                let baud = req.baud_rate.unwrap_or(9600);
                let parity = req.parity.clone().unwrap_or_else(|| "none".into());
                let data_bits = req.data_bits.unwrap_or(8);
                let stop_bits = req.stop_bits.unwrap_or(1);
                let inter = req.inter_frame_ms.unwrap_or(5);
                let port = server::open_rtu_port(&port_name, baud, data_bits, stop_bits, &parity)
                    .map_err(|e| e.to_string())?;
                self.slave_rtu_serial.stop.store(false, ORDERING);
                let units = units.clone();
                let on_write = on_write.clone();
                let stop = self.slave_rtu_serial.stop.clone();
                let handle = std::thread::spawn(move || {
                    let _ = server::run_rtu_slave_on_port(port, units, on_write, inter, stop);
                });
                *self.slave_rtu_serial.handle.lock().unwrap() = Some(handle);
                self.slave_mode.fetch_or(SLAVE_RTU, ORDERING);
                Ok(())
            }
            _ => Err(format!("未知模拟模式：{}", req.mode)),
        }
    }

    fn sim_slave_stop_inner(&self, mode: &str) {
        match mode {
            "tcp" => {
                let _ = self.slave_tcp.shutdown.send(());
                if let Some(h) = self.slave_tcp.handle.lock().unwrap().take() {
                    h.abort();
                }
                self.slave_tcp.conns.store(0, ORDERING);
                self.slave_mode.fetch_and(!SLAVE_TCP, ORDERING);
            }
            "udp" => {
                let _ = self.slave_udp.shutdown.send(());
                if let Some(h) = self.slave_udp.handle.lock().unwrap().take() {
                    h.abort();
                }
                self.slave_mode.fetch_and(!SLAVE_UDP, ORDERING);
            }
            "rtu_tcp" => {
                let _ = self.slave_rtu_tcp.shutdown.send(());
                if let Some(h) = self.slave_rtu_tcp.handle.lock().unwrap().take() {
                    h.abort();
                }
                self.slave_mode.fetch_and(!SLAVE_RTU_TCP, ORDERING);
            }
            "rtu_udp" => {
                let _ = self.slave_rtu_udp.shutdown.send(());
                if let Some(h) = self.slave_rtu_udp.handle.lock().unwrap().take() {
                    h.abort();
                }
                self.slave_mode.fetch_and(!SLAVE_RTU_UDP, ORDERING);
            }
            "rtu" => {
                self.slave_rtu_serial.stop.store(true, ORDERING);
                self.slave_mode.fetch_and(!SLAVE_RTU, ORDERING);
            }
            _ => {}
        }
    }

    pub async fn sim_slave_stop(&self, mode: String) -> Result<(), String> {
        self.sim_slave_stop_inner(&mode);
        Ok(())
    }

    pub async fn sim_slave_stop_all(&self) -> Result<(), String> {
        let m = self.slave_mode.load(ORDERING);
        if (m & SLAVE_TCP) != 0 { self.sim_slave_stop_inner("tcp"); }
        if (m & SLAVE_UDP) != 0 { self.sim_slave_stop_inner("udp"); }
        if (m & SLAVE_RTU_TCP) != 0 { self.sim_slave_stop_inner("rtu_tcp"); }
        if (m & SLAVE_RTU_UDP) != 0 { self.sim_slave_stop_inner("rtu_udp"); }
        if (m & SLAVE_RTU) != 0 { self.sim_slave_stop_inner("rtu"); }
        Ok(())
    }

    pub async fn sim_slave_status(&self) -> serde_json::Value {
        let m = self.slave_mode.load(ORDERING);
        let mut modes = serde_json::Map::new();
        modes.insert("tcp".into(), serde_json::json!((m & SLAVE_TCP) != 0));
        modes.insert("udp".into(), serde_json::json!((m & SLAVE_UDP) != 0));
        modes.insert("rtu_tcp".into(), serde_json::json!((m & SLAVE_RTU_TCP) != 0));
        modes.insert("rtu_udp".into(), serde_json::json!((m & SLAVE_RTU_UDP) != 0));
        modes.insert("rtu".into(), serde_json::json!((m & SLAVE_RTU) != 0));
        serde_json::json!({
            "modes": modes,
            "tcpConns": self.slave_tcp.conns.load(ORDERING),
            "configs": self.slave_configs.lock().unwrap().clone(),
        })
    }

    fn emit_slave_snapshot(&self, unit_id: u8) {
        if let Ok(img) = self.unit_image(unit_id) {
            let snap = server::snapshot(&img);
            self.emit(Event::SlaveValues { unit_id, snapshot: snap });
        }
    }

    // ── register value writes (editor) ────────────────────────────
    pub async fn sim_set_register(&self, addr: usize, value: u16, unit_id: Option<u8>) -> Result<(), String> {
        let uid = unit_id.unwrap_or(1);
        let img = self.unit_image(uid)?;
        img.lock().unwrap().set_holding(addr, value);
        self.emit_slave_snapshot(uid);
        Ok(())
    }
    pub async fn sim_set_input(&self, addr: usize, value: u16, unit_id: Option<u8>) -> Result<(), String> {
        let uid = unit_id.unwrap_or(1);
        let img = self.unit_image(uid)?;
        img.lock().unwrap().set_input(addr, value);
        self.emit_slave_snapshot(uid);
        Ok(())
    }
    pub async fn sim_set_coil(&self, addr: usize, on: bool, unit_id: Option<u8>) -> Result<(), String> {
        let uid = unit_id.unwrap_or(1);
        let img = self.unit_image(uid)?;
        img.lock().unwrap().set_coil(addr, on);
        self.emit_slave_snapshot(uid);
        Ok(())
    }
    pub async fn sim_set_discrete(&self, addr: usize, on: bool, unit_id: Option<u8>) -> Result<(), String> {
        let uid = unit_id.unwrap_or(1);
        let img = self.unit_image(uid)?;
        img.lock().unwrap().set_discrete(addr, on);
        self.emit_slave_snapshot(uid);
        Ok(())
    }
    pub async fn sim_reset(&self, unit_id: Option<u8>) -> Result<(), String> {
        let uid = unit_id.unwrap_or(1);
        let img = self.unit_image(uid)?;
        img.lock().unwrap().reset();
        Ok(())
    }
    pub async fn sim_snapshot(&self, unit_id: Option<u8>) -> Result<server::ImageSnapshot, String> {
        let uid = unit_id.unwrap_or(1);
        let img = self.unit_image(uid)?;
        Ok(server::snapshot(&img))
    }

    // ── Unit management ───────────────────────────────────────────
    pub async fn unit_list(&self) -> Vec<serde_json::Value> {
        let units = self.slave_units.lock().unwrap();
        units
            .iter()
            .map(|u| {
                serde_json::json!({
                    "unit_id": u.unit_id,
                    "reg_count": u.regs.lock().unwrap().len(),
                })
            })
            .collect()
    }

    pub async fn unit_add(&self, unit_id: u8) -> Result<(), String> {
        if unit_id == 0 || unit_id > 247 {
            return Err("Unit ID 须在 1 ~ 247".into());
        }
        let mut units = self.slave_units.lock().unwrap();
        if units.iter().any(|u| u.unit_id == unit_id) {
            return Err(format!("Unit ID {unit_id} 已存在"));
        }
        units.push(super::new_sim_unit(unit_id));
        drop(units);
        self.rebuild_unit_images();
        Ok(())
    }

    pub async fn unit_remove(&self, unit_id: u8) -> Result<(), String> {
        let mut units = self.slave_units.lock().unwrap();
        if units.len() <= 1 {
            return Err("至少保留一个 Unit ID".into());
        }
        let before = units.len();
        units.retain(|u| u.unit_id != unit_id);
        if units.len() == before {
            return Err(format!("Unit ID {unit_id} 不存在"));
        }
        drop(units);
        self.rebuild_unit_images();
        Ok(())
    }

    // ── register registry CRUD ────────────────────────────────────
    fn emit_regs(&self, unit_id: u8) {
        let Ok(regs) = self.unit_regs(unit_id) else { return };
        let Ok(img) = self.unit_image(unit_id) else { return };
        let resp = RegListResp {
            unit_id,
            defs: regs.lock().unwrap().clone(),
            snapshot: server::snapshot(&img),
        };
        self.emit(Event::SimRegsUpdated(resp));
    }

    pub async fn sim_reg_list(&self, keyword: Option<String>, unit_id: Option<u8>) -> Result<RegListResp, String> {
        let uid = unit_id.unwrap_or(1);
        let regs = self.unit_regs(uid)?;
        let img = self.unit_image(uid)?;
        let defs = regs.lock().unwrap();
        let kw = keyword.unwrap_or_default().trim().to_lowercase();
        let filtered: Vec<RegisterDef> = if kw.is_empty() {
            defs.clone()
        } else {
            defs.iter()
                .filter(|d| {
                    d.name.to_lowercase().contains(&kw)
                        || d.area.contains(&kw)
                        || area_label(&d.area).contains(&kw)
                        || d.addr.to_string().contains(&kw)
                        || format!("{:04x}", d.addr).contains(&kw)
                })
                .cloned()
                .collect()
        };
        drop(defs);
        Ok(RegListResp { unit_id: uid, defs: filtered, snapshot: server::snapshot(&img) })
    }

    pub async fn sim_reg_add(&self, input: RegInput, unit_id: Option<u8>) -> Result<RegisterDef, String> {
        let uid = unit_id.unwrap_or(1);
        let regs = self.unit_regs(uid)?;
        let img = self.unit_image(uid)?;
        let mut defs = regs.lock().unwrap();
        if defs.iter().any(|d| d.area == input.area && d.addr == input.addr) {
            return Err(format!(
                "{} 0x{:04X} 已存在。如需修改该地址的寄存器，请使用「编辑」，或在弹出提示中选择「替换」用新配置覆盖。",
                area_label(&input.area),
                input.addr
            ));
        }
        let def = RegisterDef {
            id: gen_id(),
            area: input.area.clone(),
            addr: input.addr,
            name: input.name,
            dtype: input.dtype,
            access: input.access,
            vary: input.vary,
            created_at_ms: now_ms(),
        };
        img.lock().unwrap().write_slot(&def.area, def.addr, input.value);
        defs.push(def.clone());
        drop(defs);
        self.emit_regs(uid);
        Ok(def)
    }

    pub async fn sim_reg_update(&self, id: String, input: RegInput, unit_id: Option<u8>) -> Result<(), String> {
        let uid = unit_id.unwrap_or(1);
        let regs = self.unit_regs(uid)?;
        let img = self.unit_image(uid)?;
        let mut defs = regs.lock().unwrap();
        if defs.iter().any(|d| d.id != id && d.area == input.area && d.addr == input.addr) {
            return Err(format!(
                "{} 0x{:04X} 已被其他寄存器占用，请换个地址",
                area_label(&input.area),
                input.addr
            ));
        }
        let def = defs.iter_mut().find(|d| d.id == id).ok_or("寄存器不存在")?;
        let moved = def.area != input.area || def.addr != input.addr;
        let old = (def.area.clone(), def.addr);
        def.area = input.area.clone();
        def.addr = input.addr;
        def.name = input.name;
        def.dtype = input.dtype;
        def.access = input.access;
        def.vary = input.vary;
        {
            let mut g = img.lock().unwrap();
            g.write_slot(&def.area, def.addr, input.value);
            if moved {
                g.clear_slot(&old.0, old.1);
            }
        }
        drop(defs);
        self.emit_regs(uid);
        Ok(())
    }

    pub async fn sim_reg_delete(&self, id: String, unit_id: Option<u8>) -> Result<(), String> {
        let uid = unit_id.unwrap_or(1);
        let regs = self.unit_regs(uid)?;
        let img = self.unit_image(uid)?;
        let mut defs = regs.lock().unwrap();
        let idx = defs.iter().position(|d| d.id == id).ok_or("寄存器不存在")?;
        let d = defs.remove(idx);
        img.lock().unwrap().clear_slot(&d.area, d.addr);
        drop(defs);
        self.emit_regs(uid);
        Ok(())
    }

    pub async fn sim_reg_seed(&self, unit_id: Option<u8>) -> Result<(), String> {
        let uid = unit_id.unwrap_or(1);
        let regs = self.unit_regs(uid)?;
        let img = self.unit_image(uid)?;
        let mut defs = regs.lock().unwrap();
        {
            let mut g = img.lock().unwrap();
            g.reset();
            let mut fresh: Vec<RegisterDef> = Vec::new();
            for s in default_reg_samples() {
                g.write_slot(&s.def.area, s.def.addr, s.value);
                fresh.push(s.def);
            }
            *defs = fresh;
        }
        drop(defs);
        self.emit_regs(uid);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn sim_reg_add_batch(
        &self,
        area: String,
        start_addr: u16,
        count: u16,
        name_prefix: String,
        dtype: String,
        access: String,
        vary: String,
        init_value: f64,
        unit_id: Option<u8>,
    ) -> Result<u16, String> {
        let uid = unit_id.unwrap_or(1);
        let regs = self.unit_regs(uid)?;
        let img = self.unit_image(uid)?;
        let mut defs = regs.lock().unwrap();
        let mut added: u16 = 0;
        for i in 0..count {
            let addr = start_addr.saturating_add(i);
            if defs.iter().any(|d| d.area == area && d.addr == addr) {
                continue;
            }
            let name = if name_prefix.is_empty() {
                format!("{area}_{addr}")
            } else {
                format!("{name_prefix}{addr}")
            };
            let def = RegisterDef {
                id: gen_id(),
                area: area.clone(),
                addr,
                name,
                dtype: dtype.clone(),
                access: access.clone(),
                vary: vary.clone(),
                created_at_ms: now_ms(),
            };
            img.lock().unwrap().write_slot(&def.area, def.addr, init_value);
            defs.push(def);
            added += 1;
        }
        drop(defs);
        self.emit_regs(uid);
        Ok(added)
    }

    // ── Excel import / export ─────────────────────────────────────
    pub async fn sim_reg_export_xlsx(&self, unit_id: Option<u8>) -> Result<String, String> {
        let uid = unit_id.unwrap_or(1);
        let regs = self.unit_regs(uid)?;
        let img = self.unit_image(uid)?;
        let defs: Vec<RegisterDef>;
        let snap: server::ImageSnapshot;
        {
            defs = regs.lock().unwrap().clone();
            snap = server::snapshot_from(&img.lock().unwrap());
        }
        let path = rfd::FileDialog::new()
            .add_filter("Excel 工作簿", &["xlsx"])
            .set_file_name(format!("modbus_unit{uid}_regs.xlsx"))
            .save_file();
        let path = match path {
            Some(p) => p.to_string_lossy().to_string(),
            None => return Ok(String::new()),
        };
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet();
        ws.set_name("寄存器列表").map_err(|e| e.to_string())?;
        let hdr = Format::new()
            .set_bold()
            .set_background_color(Color::RGB(0x1d1d1f))
            .set_font_color(Color::White);
        let headers = ["区域", "地址(DEC)", "地址(HEX)", "名称", "数据类型", "访问", "自动变化", "当前值"];
        for (c, h) in headers.iter().enumerate() {
            ws.write_with_format(0, c as u16, *h, &hdr).map_err(|e| e.to_string())?;
        }
        ws.set_column_width(0, 14).ok();
        ws.set_column_width(1, 10).ok();
        ws.set_column_width(2, 10).ok();
        ws.set_column_width(3, 20).ok();
        ws.set_column_width(4, 10).ok();
        ws.set_column_width(5, 8).ok();
        ws.set_column_width(6, 12).ok();
        ws.set_column_width(7, 12).ok();
        for (r, def) in defs.iter().enumerate() {
            let row = (r + 1) as u32;
            let raw_val = match def.area.as_str() {
                "holding" => snap.holding_registers.get(&def.addr).copied().unwrap_or(0) as f64,
                "input" => snap.input_registers.get(&def.addr).copied().unwrap_or(0) as f64,
                "coil" => if snap.coils.get(&def.addr).copied().unwrap_or(false) { 1.0 } else { 0.0 },
                _ => if snap.discrete_inputs.get(&def.addr).copied().unwrap_or(false) { 1.0 } else { 0.0 },
            };
            ws.write(row, 0, &def.area).ok();
            ws.write(row, 1, def.addr as i32).ok();
            ws.write(row, 2, format!("0x{:04X}", def.addr)).ok();
            ws.write(row, 3, &def.name).ok();
            ws.write(row, 4, &def.dtype).ok();
            ws.write(row, 5, &def.access).ok();
            ws.write(row, 6, &def.vary).ok();
            ws.write(row, 7, raw_val).ok();
        }
        wb.save(&path).map_err(|e| format!("保存 xlsx 失败：{e}"))?;
        Ok(path)
    }

    pub async fn sim_reg_import_xlsx(&self, unit_id: Option<u8>, replace: bool) -> Result<(u32, u32), String> {
        let uid = unit_id.unwrap_or(1);
        let path = rfd::FileDialog::new()
            .add_filter("Excel 工作簿", &["xlsx", "xls", "ods"])
            .pick_file();
        let path = match path {
            Some(p) => p.to_string_lossy().to_string(),
            None => return Ok((0, 0)),
        };
        let rows: Vec<Vec<String>> = tokio::task::spawn_blocking(move || {
            let mut xl = open_workbook_auto(&path).map_err(|e| format!("打开文件失败：{e}"))?;
            let sheet = xl
                .worksheet_range_at(0)
                .ok_or_else(|| "文件没有工作表".to_string())?
                .map_err(|e| format!("读取工作表失败：{e}"))?;
            let mut rows = Vec::new();
            for row in sheet.rows().skip(1) {
                let cells: Vec<String> = row.iter().map(|c| c.to_string()).collect();
                if cells.len() >= 7 {
                    rows.push(cells);
                }
            }
            Ok::<_, String>(rows)
        })
        .await
        .map_err(|e| e.to_string())??;

        let regs = self.unit_regs(uid)?;
        let img = self.unit_image(uid)?;
        let mut defs = regs.lock().unwrap();
        let mut ok_count: u32 = 0;
        let mut skip_count: u32 = 0;
        for cells in &rows {
            let area = cells.first().map(|s| s.trim().to_lowercase()).unwrap_or_default();
            let addr_s = cells.get(1).map(|s| s.trim().to_string()).unwrap_or_default();
            let addr: u16 = match addr_s.parse() {
                Ok(v) => v,
                Err(_) => { skip_count += 1; continue; }
            };
            if !matches!(area.as_str(), "holding" | "input" | "coil" | "discrete") {
                skip_count += 1;
                continue;
            }
            let name = cells.get(3).cloned().unwrap_or_default();
            let dtype = cells.get(4).cloned().unwrap_or_else(|| "u16".into());
            let access = cells.get(5).cloned().unwrap_or_else(|| "rw".into());
            let vary = cells.get(6).cloned().unwrap_or_else(|| "off".into());
            let value: f64 = cells.get(7).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            if let Some(pos) = defs.iter().position(|d| d.area == area && d.addr == addr) {
                if replace {
                    defs[pos].name = name.clone();
                    defs[pos].dtype = dtype.clone();
                    defs[pos].access = access.clone();
                    defs[pos].vary = vary.clone();
                    img.lock().unwrap().write_slot(&area, addr, value);
                    ok_count += 1;
                } else {
                    skip_count += 1;
                }
            } else {
                let def = RegisterDef {
                    id: gen_id(),
                    area: area.clone(),
                    addr,
                    name,
                    dtype,
                    access,
                    vary,
                    created_at_ms: now_ms(),
                };
                img.lock().unwrap().write_slot(&area, addr, value);
                defs.push(def);
                ok_count += 1;
            }
        }
        drop(defs);
        self.emit_regs(uid);
        Ok((ok_count, skip_count))
    }

    // ── serial ports ──────────────────────────────────────────────
    pub async fn list_serial_ports(&self) -> Result<Vec<String>, String> {
        let ports = serial2::SerialPort::available_ports().map_err(|e| e.to_string())?;
        let mut names: Vec<String> = ports
            .iter()
            .map(|p| p.display().to_string())
            .filter(|n| !n.contains("/dev/tty."))
            .collect();
        if names.is_empty() {
            names = ports.iter().map(|p| p.display().to_string()).collect();
        }
        names.sort();
        Ok(names)
    }
}
