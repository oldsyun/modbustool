//! Application service: state + Modbus operations + event stream.
//! Ported from the Tauri command layer with the webview bridge removed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use modbus_core::client::ModbusClient;
use modbus_core::server::{self, ImageSnapshot, SharedImage, UnitImages};
use modbus_core::simulator::Simulator;
use modbus_core::transport::rtu::RtuTransport;
use modbus_core::transport::udp::UdpTransport;
use tokio::sync::{broadcast, Mutex as AsyncMutex};

use super::reg::{default_reg_samples, RegListResp, RegisterDef};

mod sim;
pub mod serial;

pub const CONN_NONE: u8 = 0;
pub const CONN_TCP: u8 = 1;
pub const CONN_RTU: u8 = 2;
pub const CONN_UDP: u8 = 3;

pub const SLAVE_NONE: u8 = 0;
pub const SLAVE_TCP: u8 = 1;
pub const SLAVE_UDP: u8 = 2;
pub const SLAVE_RTU: u8 = 4;
pub const SLAVE_RTU_TCP: u8 = 8;
pub const SLAVE_RTU_UDP: u8 = 16;

pub const ORDERING: Ordering = Ordering::SeqCst;

/// Backend → UI push events (replaces Tauri `emit`).
#[derive(Clone)]
pub enum Event {
    /// One poll cycle finished (or errored).
    PollFrame {
        poll_id: u32,
        poll_name: String,
        addr: u16,
        regs: Option<Vec<u16>>,
        tx: String,
        rx: String,
        rtt_ms: Option<u64>,
        error: Option<String>,
    },
    /// Live values of a simulated Unit changed.
    SlaveValues { unit_id: u8, snapshot: ImageSnapshot },
    /// Register definitions of a simulated Unit changed.
    SimRegsUpdated(RegListResp),
    /// Raw bytes received on the serial debugging port.
    SerialRx { bytes: Vec<u8>, ts_ms: u64 },
    /// Serial debugging port opened/closed (or errored).
    SerialState { open: bool, error: Option<String> },
}

// ── runtime handles ──────────────────────────────────────────────
pub struct SlaveRuntime {
    pub shutdown: broadcast::Sender<()>,
    pub handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    pub conns: Arc<AtomicUsize>,
}
impl SlaveRuntime {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel::<()>(2);
        Self {
            shutdown: tx,
            handle: Arc::new(Mutex::new(None)),
            conns: Arc::new(AtomicUsize::new(0)),
        }
    }
}

pub struct SlaveSerialRuntime {
    pub stop: Arc<AtomicBool>,
    pub handle: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}
impl SlaveSerialRuntime {
    fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            handle: Arc::new(Mutex::new(None)),
        }
    }
}

pub struct PollTask {
    pub stop: Arc<AtomicBool>,
    pub handle: tokio::task::JoinHandle<()>,
}

pub struct SimUnit {
    pub unit_id: u8,
    pub image: SharedImage,
    pub regs: Arc<Mutex<Vec<RegisterDef>>>,
}

pub fn new_sim_unit(unit_id: u8) -> SimUnit {
    let image: SharedImage = server::new_shared_image();
    let mut regs: Vec<RegisterDef> = Vec::new();
    {
        let mut img = image.lock().unwrap();
        for s in default_reg_samples() {
            img.write_slot(&s.def.area, s.def.addr, s.value);
            regs.push(s.def);
        }
    }
    SimUnit { unit_id, image, regs: Arc::new(Mutex::new(regs)) }
}

/// Serialisable slave-start request (camelCase over the wire / stored configs).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlaveStartReq {
    pub mode: String,
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub port_name: Option<String>,
    pub baud_rate: Option<u32>,
    pub data_bits: Option<u8>,
    pub stop_bits: Option<u8>,
    pub parity: Option<String>,
    pub inter_frame_ms: Option<u64>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct WriteResult {
    pub tx: String,
    pub rx: String,
    pub rtt_ms: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ReadPointsResult {
    pub regs: Vec<u16>,
    pub tx: String,
    pub rx: String,
    pub rtt_ms: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SendRawFrameResp {
    pub tx: String,
    pub rx: String,
    pub rtt_ms: u64,
    pub pdu_resp: String,
}

pub struct Service {
    pub client: AsyncMutex<Option<ModbusClient>>,
    pub rtu: Mutex<Option<RtuTransport>>,
    pub udp: AsyncMutex<Option<UdpTransport>>,
    pub conn_type: AtomicU8,
    pub slave_units: Mutex<Vec<SimUnit>>,
    pub unit_images: Arc<Mutex<UnitImages>>,
    pub simulator: Mutex<Simulator>,
    pub poll_running: AtomicBool,
    pub poll_tasks: Mutex<HashMap<u32, PollTask>>,
    pub slave_tcp: SlaveRuntime,
    pub slave_udp: SlaveRuntime,
    pub slave_rtu_tcp: SlaveRuntime,
    pub slave_rtu_udp: SlaveRuntime,
    pub slave_rtu_serial: SlaveSerialRuntime,
    pub slave_configs: Mutex<HashMap<String, serde_json::Value>>,
    pub slave_mode: AtomicU8,
    /// Broadcast channel to all UI subscribers.
    pub events: broadcast::Sender<Event>,
    /// Pure serial-debug session (mutually exclusive with the Modbus master).
    pub serial: Mutex<Option<serial::SerialSession>>,
    /// Remember last serial port/baud for status display.
    pub serial_meta: Mutex<(String, u32)>,
}

impl Service {
    pub fn new() -> Arc<Self> {
        let default_unit = new_sim_unit(1);
        let mut unit_images: UnitImages = UnitImages::new();
        unit_images.insert(default_unit.unit_id, default_unit.image.clone());
        let (events, _rx) = broadcast::channel(256);
        Arc::new(Self {
            client: AsyncMutex::new(None),
            rtu: Mutex::new(None),
            udp: AsyncMutex::new(None),
            conn_type: AtomicU8::new(CONN_NONE),
            slave_units: Mutex::new(vec![default_unit]),
            unit_images: Arc::new(Mutex::new(unit_images)),
            simulator: Mutex::new(Simulator::new()),
            poll_running: AtomicBool::new(false),
            poll_tasks: Mutex::new(HashMap::new()),
            slave_tcp: SlaveRuntime::new(),
            slave_udp: SlaveRuntime::new(),
            slave_rtu_tcp: SlaveRuntime::new(),
            slave_rtu_udp: SlaveRuntime::new(),
            slave_rtu_serial: SlaveSerialRuntime::new(),
            slave_configs: Mutex::new(HashMap::new()),
            slave_mode: AtomicU8::new(SLAVE_NONE),
            events,
            serial: Mutex::new(None),
            serial_meta: Mutex::new((String::new(), 0)),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub(crate) fn emit(&self, ev: Event) {
        let _ = self.events.send(ev);
    }

    pub fn rebuild_unit_images(&self) {
        let mut map = self.unit_images.lock().unwrap();
        map.clear();
        let units = self.slave_units.lock().unwrap();
        for u in units.iter() {
            map.insert(u.unit_id, u.image.clone());
        }
    }

    pub(crate) fn unit_image(&self, unit_id: u8) -> Result<SharedImage, String> {
        self.slave_units
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.unit_id == unit_id)
            .map(|u| u.image.clone())
            .ok_or_else(|| format!("Unit ID {unit_id} 未配置"))
    }

    pub(crate) fn unit_regs(&self, unit_id: u8) -> Result<Arc<Mutex<Vec<RegisterDef>>>, String> {
        self.slave_units
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.unit_id == unit_id)
            .map(|u| u.regs.clone())
            .ok_or_else(|| format!("Unit ID {unit_id} 未配置"))
    }

    // ── Connection ────────────────────────────────────────────────
    pub async fn connect(
        &self,
        host: String,
        port: u16,
        timeout_ms: u64,
        retries: u32,
        transport: Option<String>,
    ) -> Result<(), String> {
        *self.client.lock().await = None;
        *self.rtu.lock().unwrap() = None;
        *self.udp.lock().await = None;
        self.conn_type.store(CONN_NONE, ORDERING);
        let use_udp = transport
            .as_deref()
            .map(|t| t.eq_ignore_ascii_case("udp"))
            .unwrap_or(false);
        if use_udp {
            let udp = UdpTransport::connect(&host, port, Duration::from_millis(timeout_ms))
                .await
                .map_err(|e| e.to_string())?;
            *self.udp.lock().await = Some(udp);
            self.conn_type.store(CONN_UDP, ORDERING);
            return Ok(());
        }
        let client = ModbusClient::connect_tcp(&host, port, 1, Duration::from_millis(timeout_ms), retries)
            .await
            .map_err(|e| e.to_string())?;
        *self.client.lock().await = Some(client);
        self.conn_type.store(CONN_TCP, ORDERING);
        Ok(())
    }

    pub async fn connect_rtu(
        &self,
        port_name: String,
        baud_rate: u32,
        data_bits: u8,
        stop_bits: u8,
        parity: String,
        timeout_ms: u64,
        inter_frame_ms: Option<u64>,
    ) -> Result<(), String> {
        *self.client.lock().await = None;
        *self.rtu.lock().unwrap() = None;
        *self.udp.lock().await = None;
        self.conn_type.store(CONN_NONE, ORDERING);
        let rtu = RtuTransport::open(
            &port_name,
            baud_rate,
            data_bits,
            stop_bits,
            &parity,
            1,
            inter_frame_ms.unwrap_or(5),
            timeout_ms,
        )
        .map_err(|e| friendly_serial_error(e.to_string()))?;
        *self.rtu.lock().unwrap() = Some(rtu);
        self.conn_type.store(CONN_RTU, ORDERING);
        Ok(())
    }

    pub async fn connect_rtu_over_tcp(
        &self,
        host: String,
        port: u16,
        timeout_ms: u64,
        retries: u32,
        inter_frame_ms: Option<u64>,
        transport: Option<String>,
    ) -> Result<(), String> {
        *self.client.lock().await = None;
        *self.rtu.lock().unwrap() = None;
        *self.udp.lock().await = None;
        self.conn_type.store(CONN_NONE, ORDERING);
        let use_udp = transport
            .as_deref()
            .map(|t| t.eq_ignore_ascii_case("udp"))
            .unwrap_or(false);
        if use_udp {
            let udp = UdpTransport::connect_with_mode(
                &host,
                port,
                Duration::from_millis(timeout_ms),
                modbus_core::transport::udp::UdpMode::Rtu,
            )
            .await
            .map_err(|e| e.to_string())?;
            *self.udp.lock().await = Some(udp);
            self.conn_type.store(CONN_UDP, ORDERING);
            return Ok(());
        }
        let client = ModbusClient::connect_rtu_over_tcp(
            &host,
            port,
            1,
            Duration::from_millis(timeout_ms),
            retries,
            Duration::from_millis(inter_frame_ms.unwrap_or(5)),
        )
        .await
        .map_err(|e| e.to_string())?;
        *self.client.lock().await = Some(client);
        self.conn_type.store(CONN_TCP, ORDERING);
        Ok(())
    }

    pub async fn conn_info(&self) -> String {
        match self.conn_type.load(ORDERING) {
            CONN_TCP => "tcp".into(),
            CONN_RTU => "rtu".into(),
            CONN_UDP => "udp".into(),
            _ => "none".into(),
        }
    }

    pub async fn disconnect(&self) -> Result<(), String> {
        self.stop_all_polls().await;
        *self.client.lock().await = None;
        *self.rtu.lock().unwrap() = None;
        *self.udp.lock().await = None;
        self.conn_type.store(CONN_NONE, ORDERING);
        Ok(())
    }

    // ── Frame exchange (the one primitive everything routes through) ──
    /// Send a PDU over the active transport; returns (tx, rx, rtt, resp_pdu).
    async fn request_frame(
        &self,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>, Duration, Vec<u8>), String> {
        match self.conn_type.load(ORDERING) {
            CONN_TCP => {
                let mut client = self.client.lock().await;
                let c = client.as_mut().ok_or("not connected")?;
                c.request_frame(unit_id, pdu).await.map_err(|e| e.to_string())
            }
            CONN_RTU => {
                let mut rtu = self.rtu.lock().unwrap();
                let r = rtu.as_mut().ok_or("not connected")?;
                r.request_frame_for(unit_id, pdu).map_err(|e| e.to_string())
            }
            CONN_UDP => {
                let mut udp = self.udp.lock().await;
                let u = udp.as_mut().ok_or("not connected")?;
                u.request_frame(unit_id, pdu).await.map_err(|e| e.to_string())
            }
            _ => Err("not connected".into()),
        }
    }

    // ── Read / Write points ───────────────────────────────────────
    pub async fn read_points(
        &self,
        unit_id: u8,
        func: String,
        addr: u16,
        count: u16,
    ) -> Result<ReadPointsResult, String> {
        let fc: u8 = func.parse().unwrap_or(0x03);
        if !matches!(fc, 0x01 | 0x02 | 0x03 | 0x04) {
            return Err("read_points supports FC01/FC02/FC03/FC04 only".into());
        }
        let pdu = build_read_pdu(fc, addr, count);
        let (tx, rx, rtt, resp) = self.request_frame(unit_id, &pdu).await?;
        let vals = decode_read(&resp, fc, count as usize);
        if vals.is_empty() {
            return Err("unexpected read response".into());
        }
        Ok(ReadPointsResult {
            regs: vals,
            tx: hex_encode(&tx),
            rx: hex_encode(&rx),
            rtt_ms: rtt.as_millis() as u64,
        })
    }

    pub async fn write_point(
        &self,
        unit_id: u8,
        func: String,
        addr: u16,
        values: Vec<u16>,
    ) -> Result<WriteResult, String> {
        let fc: u8 = func.parse().unwrap_or(0x03);
        let pdu: Vec<u8> = if fc == 0x01 {
            let v = if values.first().copied().unwrap_or(0) != 0 { 0xFF } else { 0x00 };
            vec![0x05, (addr >> 8) as u8, addr as u8, v, 0x00]
        } else if values.len() >= 2 {
            let mut p = vec![0x10, (addr >> 8) as u8, addr as u8, 0x00, 0x02, 0x04];
            for v in values.iter().take(2) {
                p.push((v >> 8) as u8);
                p.push((v & 0xff) as u8);
            }
            p
        } else {
            let v = values.first().copied().unwrap_or(0);
            vec![0x06, (addr >> 8) as u8, addr as u8, (v >> 8) as u8, v as u8]
        };
        let (tx, rx, rtt, _resp) = self.request_frame(unit_id, &pdu).await?;
        Ok(WriteResult {
            tx: hex_encode(&tx),
            rx: hex_encode(&rx),
            rtt_ms: rtt.as_millis() as u64,
        })
    }

    pub async fn send_raw_frame(
        &self,
        unit_id: u8,
        hex: String,
    ) -> Result<SendRawFrameResp, String> {
        let pdu = hex_decode(&hex)?;
        let (tx, rx, rtt, resp_pdu) = self.request_frame(unit_id, &pdu).await?;
        Ok(SendRawFrameResp {
            tx: hex_encode(&tx),
            rx: hex_encode(&rx),
            rtt_ms: rtt.as_millis() as u64,
            pdu_resp: hex_encode(&resp_pdu),
        })
    }

    // ── Polling ───────────────────────────────────────────────────
    pub async fn start_poll(
        self: &Arc<Self>,
        poll_id: u32,
        poll_name: String,
        func: String,
        addr: u16,
        count: u16,
        interval_ms: u64,
        unit_id: u8,
    ) -> Result<(), String> {
        {
            let map = self.poll_tasks.lock().unwrap();
            if map.contains_key(&poll_id) {
                return Err(format!("poll {poll_id} already running"));
            }
        }
        let fc: u8 = func.parse().unwrap_or(0x03);
        let stop = Arc::new(AtomicBool::new(false));
        let pdu = build_read_pdu(fc, addr, count);
        let me = self.clone();
        let stop_inner = stop.clone();
        let handle = tokio::spawn(async move {
            while !stop_inner.load(ORDERING) {
                let outcome = me.poll_once(unit_id, &pdu, fc, count).await;
                match outcome {
                    Some(Ok((regs, tx, rx, rtt))) => me.emit(Event::PollFrame {
                        poll_id,
                        poll_name: poll_name.clone(),
                        addr,
                        regs: Some(regs),
                        tx,
                        rx,
                        rtt_ms: Some(rtt),
                        error: None,
                    }),
                    Some(Err(e)) => me.emit(Event::PollFrame {
                        poll_id,
                        poll_name: poll_name.clone(),
                        addr,
                        regs: None,
                        tx: String::new(),
                        rx: String::new(),
                        rtt_ms: None,
                        error: Some(e),
                    }),
                    None => break, // disconnected
                }
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            }
        });
        self.poll_tasks
            .lock()
            .unwrap()
            .insert(poll_id, PollTask { stop, handle });
        self.poll_running.store(true, ORDERING);
        Ok(())
    }

    async fn poll_once(
        &self,
        unit_id: u8,
        pdu: &[u8],
        fc: u8,
        count: u16,
    ) -> Option<Result<(Vec<u16>, String, String, u64), String>> {
        if self.conn_type.load(ORDERING) == CONN_NONE {
            return None;
        }
        match self.request_frame(unit_id, pdu).await {
            Ok((tx, rx, rtt, resp)) => Some(Ok((
                decode_read(&resp, fc, count as usize),
                hex_encode(&tx),
                hex_encode(&rx),
                rtt.as_millis() as u64,
            ))),
            Err(e) => {
                if e == "not connected" {
                    None
                } else {
                    Some(Err(e))
                }
            }
        }
    }

    pub async fn stop_poll(&self, poll_id: u32) {
        if let Some(task) = self.poll_tasks.lock().unwrap().remove(&poll_id) {
            task.stop.store(true, ORDERING);
            task.handle.abort();
        }
        if self.poll_tasks.lock().unwrap().is_empty() {
            self.poll_running.store(false, ORDERING);
        }
    }

    pub async fn stop_all_polls(&self) {
        let tasks: Vec<PollTask> = self
            .poll_tasks
            .lock()
            .unwrap()
            .drain()
            .map(|(_, v)| v)
            .collect();
        for t in tasks {
            t.stop.store(true, ORDERING);
            t.handle.abort();
        }
        self.poll_running.store(false, ORDERING);
    }
}

// ── helpers ──────────────────────────────────────────────────────
pub fn build_read_pdu(func: u8, addr: u16, count: u16) -> Vec<u8> {
    vec![func, (addr >> 8) as u8, addr as u8, (count >> 8) as u8, count as u8]
}

pub fn decode_read(resp: &[u8], func: u8, count: usize) -> Vec<u16> {
    if resp.len() < 2 || resp[0] != func {
        return Vec::new();
    }
    let bc = resp[1] as usize;
    let data = &resp[2..];
    if data.len() < bc {
        return Vec::new();
    }
    match func {
        0x01 | 0x02 => {
            let mut out = Vec::new();
            for &b in &data[..bc] {
                for bit in 0..8 {
                    out.push(((b >> bit) & 0x01) as u16);
                }
            }
            out.truncate(count);
            out
        }
        _ => (0..bc / 2)
            .map(|i| u16::from_be_bytes([data[i * 2], data[i * 2 + 1]]))
            .collect(),
    }
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s: String = s.split_whitespace().collect();
    if s.len() % 2 != 0 {
        return Err("hex length must be even".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

pub fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02X}", x)).collect::<Vec<_>>().join(" ")
}

pub fn friendly_serial_error(msg: String) -> String {
    if msg.contains("Resource busy") || msg.contains("os error 16") {
        format!("{msg}\n\n提示：串口已被其他程序独占用，请先关闭占用该串口的程序，或重启该串口设备后重试。")
    } else {
        msg
    }
}
