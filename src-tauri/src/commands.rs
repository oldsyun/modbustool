//! Tauri command handlers — the bridge between the web UI and `modbus-core`.

use crate::state::{
    AppState, CONN_NONE, CONN_RTU, CONN_TCP, CONN_UDP, ORDERING, SLAVE_RTU, SLAVE_RTU_TCP,
    SLAVE_RTU_UDP, SLAVE_TCP, SLAVE_UDP, new_sim_unit,
};
use serde::Deserialize;
use crate::simreg::{RegisterDef, RegInput, RegListResp, area_label, default_reg_samples, gen_id, now_ms};
use modbus_core::server::{self, ImageSnapshot, SharedImage};
use modbus_core::simulator::VaryMode;
use modbus_core::transport::rtu::RtuTransport;
use modbus_core::transport::udp::UdpTransport;
use std::sync::Arc;
use std::thread;
use modbus_core::workspace::Workspace;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::time::sleep;
use rust_xlsxwriter::{Format, Workbook, Color};
use calamine::{open_workbook_auto, Reader};

/// 对活动 UDP 通道执行一次帧交换（Frame = TX/RX ADU + RTT + 响应 PDU）。
async fn udp_frame(
    state: &State<'_, AppState>,
    unit_id: u8,
    pdu: &[u8],
) -> Result<(Vec<u8>, Vec<u8>, Duration, Vec<u8>), String> {
    let mut udp = state.udp.lock().await;
    let u = udp.as_mut().ok_or("not connected")?;
    u.request_frame(unit_id, pdu).await.map_err(|e| e.to_string())
}

// ── 多 Unit 模拟：按 Unit ID 定位从站的数据镜像与寄存器注册表 ──

/// 取指定 Unit 的共享镜像（未配置 → Err）。
fn unit_image(state: &AppState, unit_id: u8) -> Result<SharedImage, String> {
    let units = state.slave_units.lock().unwrap();
    units
        .iter()
        .find(|u| u.unit_id == unit_id)
        .map(|u| u.image.clone())
        .ok_or_else(|| format!("Unit ID {unit_id} 未配置"))
}

/// 取指定 Unit 的寄存器注册表（未配置 → Err）。
fn unit_regs(state: &AppState, unit_id: u8) -> Result<Arc<std::sync::Mutex<Vec<RegisterDef>>>, String> {
    let units = state.slave_units.lock().unwrap();
    units
        .iter()
        .find(|u| u.unit_id == unit_id)
        .map(|u| u.regs.clone())
        .ok_or_else(|| format!("Unit ID {unit_id} 未配置"))
}

// ══════════════════════════════════════════════════════════════════
// Connection
// ══════════════════════════════════════════════════════════════════

/// Connect to a Modbus TCP/UDP slave.
///
/// `transport` selects the socket type: `"tcp"` (stream) or `"udp"`
/// (datagram, Modbus/UDP — same MBAP framing, no connection handshake).
/// For TCP only verifies the port opens; for UDP only binds the local socket.
/// No Modbus handshake is performed and no Unit ID is bound to the
/// connection. Each request (poll / raw send) carries its own Unit ID.
#[tauri::command]
pub async fn connect_tcp(
    state: State<'_, AppState>,
    host: String,
    port: u16,
    timeout_ms: u64,
    retries: u32,
    transport: Option<String>,
) -> Result<(), String> {
    // Release any previous connection first so the state machine stays clean.
    *state.client.lock().await = None;
    *state.rtu.lock().unwrap() = None;
    *state.udp.lock().await = None;
    state.conn_type.store(CONN_NONE, ORDERING);

    let use_udp = transport
        .as_deref()
        .map(|t| t.eq_ignore_ascii_case("udp"))
        .unwrap_or(false);
    if use_udp {
        let udp = UdpTransport::connect(&host, port, Duration::from_millis(timeout_ms))
            .await
            .map_err(|e| e.to_string())?;
        *state.udp.lock().await = Some(udp);
        state.conn_type.store(CONN_UDP, ORDERING);
        return Ok(());
    }

    let client = modbus_core::client::ModbusClient::connect_tcp(
        &host,
        port,
        1, // default station; real unit_id comes per-request
        Duration::from_millis(timeout_ms),
        retries,
    )
    .await
    .map_err(|e| e.to_string())?;
    *state.client.lock().await = Some(client);
    state.conn_type.store(CONN_TCP, ORDERING);
    Ok(())
}

/// Connect to a Modbus RTU slave over serial.
///
/// Only verifies the serial port can be opened — no application-layer test
/// read is performed and no Unit ID is bound to the connection (the transport
/// keeps a default of 1; every poll/request stamps its own Unit ID).
#[tauri::command]
pub async fn connect_rtu(
    state: State<'_, AppState>,
    port_name: String,
    baud_rate: u32,
    data_bits: u8,
    stop_bits: u8,
    parity: String,
    timeout_ms: u64,
    inter_frame_ms: Option<u64>,
) -> Result<(), String> {
    // Release any previous connection FIRST. Keeping the old RtuTransport alive
    // while opening the same serial device makes the second open fail with
    // EBUSY ("Resource busy", os error 16) on macOS/Linux.
    *state.client.lock().await = None;
    *state.rtu.lock().unwrap() = None;
    *state.udp.lock().await = None;
    state.conn_type.store(CONN_NONE, ORDERING);

    let rtu = RtuTransport::open(
        &port_name,
        baud_rate,
        data_bits,
        stop_bits,
        &parity,
        1,  // default station; real unit_id comes per-request
        inter_frame_ms.unwrap_or(5),  // default 5 ms (≈3.5 char @ 9600 baud)
        timeout_ms,
    )
    .map_err(|e| friendly_serial_error(e.to_string()))?;

    *state.rtu.lock().unwrap() = Some(rtu);
    state.conn_type.store(CONN_RTU, ORDERING);
    Ok(())
}

/// Connect to a Modbus RTU slave over a TCP or UDP transport
/// (no MBAP header — serial-style RTU frames on the wire).
/// Common with serial-to-Ethernet gateways and Modbus-to-network converters.
///
/// `transport` selects the socket type: `"tcp"` (RTU over TCP/IP) or
/// `"udp"` (RTU over UDP/IP — one RTU frame per datagram).
///
/// Only verifies the port opens — no Modbus handshake and no Unit ID bound
/// to the connection (every request stamps its own Unit ID).
#[tauri::command]
pub async fn connect_rtu_over_tcp(
    state: State<'_, AppState>,
    host: String,
    port: u16,
    timeout_ms: u64,
    retries: u32,
    inter_frame_ms: Option<u64>,
    transport: Option<String>,
) -> Result<(), String> {
    // Release any previous connection first so the state machine stays clean.
    *state.client.lock().await = None;
    *state.rtu.lock().unwrap() = None;
    *state.udp.lock().await = None;
    state.conn_type.store(CONN_NONE, ORDERING);

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
        *state.udp.lock().await = Some(udp);
        state.conn_type.store(CONN_UDP, ORDERING);
        return Ok(());
    }

    let client = modbus_core::client::ModbusClient::connect_rtu_over_tcp(
        &host,
        port,
        1, // default station; real unit_id comes per-request
        Duration::from_millis(timeout_ms),
        retries,
        Duration::from_millis(inter_frame_ms.unwrap_or(5)),
    )
    .await
    .map_err(|e| e.to_string())?;
    *state.client.lock().await = Some(client);
    // Route via CONN_TCP — the transport internally frames RTU.
    state.conn_type.store(CONN_TCP, ORDERING);
    Ok(())
}

/// Disconnect the active client (TCP or RTU).
#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    // Stop every active poll first so they don't try to use the closed transport.
    let tasks: Vec<crate::state::PollTask> =
        state.poll_tasks.lock().unwrap().drain().map(|(_, v)| v).collect();
    for t in tasks {
        t.stop.store(true, ORDERING);
        t.handle.abort();
    }
    state.poll_running.store(false, ORDERING);
    *state.client.lock().await = None;
    *state.rtu.lock().unwrap() = None;
    *state.udp.lock().await = None;
    state.conn_type.store(CONN_NONE, ORDERING);
    Ok(())
}

/// Return the current connection type: "none" | "tcp" | "rtu" | "udp".
#[tauri::command]
pub async fn conn_info(state: State<'_, AppState>) -> Result<String, String> {
    match state.conn_type.load(ORDERING) {
        CONN_TCP => Ok("tcp".into()),
        CONN_RTU => Ok("rtu".into()),
        CONN_UDP => Ok("udp".into()),
        _ => Ok("none".into()),
    }
}

// ══════════════════════════════════════════════════════════════════
// Register I/O (routed to active transport)
// ══════════════════════════════════════════════════════════════════

/// Read holding registers (FC03) over the active connection (TCP or RTU).
#[tauri::command]
pub async fn read_registers(
    state: State<'_, AppState>,
    addr: u16,
    count: u16,
) -> Result<Vec<u16>, String> {
    match state.conn_type.load(ORDERING) {
        CONN_TCP => {
            let mut client = state.client.lock().await;
            let c = client.as_mut().ok_or("not connected")?;
            c.read_holding_registers(addr, count)
                .await
                .map_err(|e| e.to_string())
        }
        CONN_RTU => {
            let mut rtu = state.rtu.lock().unwrap();
            let r = rtu.as_mut().ok_or("not connected")?;
            // Build FC03 PDU: [03, addr_hi, addr_lo, count_hi, count_lo]
            let pdu = [0x03, (addr >> 8) as u8, addr as u8, (count >> 8) as u8, count as u8];
            let resp = r.request(&pdu).map_err(|e| e.to_string())?;
            // Response PDU: [03, byte_count, data...]
            if resp.len() < 2 || resp[0] != 0x03 {
                return Err("unexpected response".into());
            }
            let bc = resp[1] as usize;
            let data = &resp[2..];
            if data.len() < bc {
                return Err("truncated response".into());
            }
            Ok((0..bc / 2)
                .map(|i| u16::from_be_bytes([data[i * 2], data[i * 2 + 1]]))
                .collect())
        }
        CONN_UDP => {
            let pdu = [0x03, (addr >> 8) as u8, addr as u8, (count >> 8) as u8, count as u8];
            let (_tx, _rx, _rtt, resp) = udp_frame(&state, 1, &pdu).await?;
            if resp.len() < 2 || resp[0] != 0x03 {
                return Err("unexpected response".into());
            }
            let bc = resp[1] as usize;
            let data = &resp[2..];
            if data.len() < bc {
                return Err("truncated response".into());
            }
            Ok((0..bc / 2)
                .map(|i| u16::from_be_bytes([data[i * 2], data[i * 2 + 1]]))
                .collect())
        }
        _ => Err("not connected".into()),
    }
}

/// Write a single holding register (FC06).
#[tauri::command]
pub async fn write_register(
    state: State<'_, AppState>,
    addr: u16,
    value: u16,
) -> Result<(), String> {
    match state.conn_type.load(ORDERING) {
        CONN_TCP => {
            let mut client = state.client.lock().await;
            let c = client.as_mut().ok_or("not connected")?;
            c.write_single_register(addr, value)
                .await
                .map_err(|e| e.to_string())
        }
        CONN_RTU => {
            let mut rtu = state.rtu.lock().unwrap();
            let r = rtu.as_mut().ok_or("not connected")?;
            // FC06 PDU: [06, addr_hi, addr_lo, val_hi, val_lo]
            let pdu = [
                0x06,
                (addr >> 8) as u8,
                addr as u8,
                (value >> 8) as u8,
                value as u8,
            ];
            r.request(&pdu).map_err(|e| e.to_string())?;
            Ok(())
        }
        CONN_UDP => {
            let pdu = [
                0x06,
                (addr >> 8) as u8,
                addr as u8,
                (value >> 8) as u8,
                value as u8,
            ];
            udp_frame(&state, 1, &pdu).await?;
            Ok(())
        }
        _ => Err("not connected".into()),
    }
}

/// Write a data point from the table's 写入 button.
///
/// `func` selects the function code family:
/// - "01" (coil): `values[0]` 0/1 → FC05 write single coil.
/// - "03" (holding register): 1 value → FC06 write single register;
///   2 values → FC16 write multiple registers (used by 32-bit formats,
///   writing the high and low word to consecutive addresses `addr`, `addr+1`).
///
/// `unit_id` comes from the owning poll, so RTU frames stamp the right slave.
/// Result of a single-shot write: the raw TX/RX ADU hex strings and the
/// round-trip time, so the UI can render the frame exchange in the same
/// TX/RX style as periodic polling / single-shot reads.
#[derive(serde::Serialize)]
pub struct WriteResult {
    pub tx: String,
    pub rx: String,
    pub rtt_ms: u64,
}

#[tauri::command]
pub async fn write_point(
    state: State<'_, AppState>,
    unit_id: u8,
    func: String,
    addr: u16,
    values: Vec<u16>,
) -> Result<WriteResult, String> {
    let fc: u8 = func.parse().unwrap_or(0x03);
    let pdu: Vec<u8> = if fc == 0x01 {
        // FC05: [05, addr_hi, addr_lo, 0xFF|0x00, 0x00]
        let v = if values.first().copied().unwrap_or(0) != 0 { 0xFF } else { 0x00 };
        vec![0x05, (addr >> 8) as u8, addr as u8, v, 0x00]
    } else if values.len() >= 2 {
        // FC16: [10, addr_hi, addr_lo, qty_hi, qty_lo, byte_count, v1hi, v1lo, v2hi, v2lo]
        let mut p = vec![0x10, (addr >> 8) as u8, addr as u8, 0x00, 0x02, 0x04];
        for v in values.iter().take(2) {
            p.push((v >> 8) as u8);
            p.push((v & 0xff) as u8);
        }
        p
    } else {
        // FC06: [06, addr_hi, addr_lo, val_hi, val_lo]
        let v = values.first().copied().unwrap_or(0);
        vec![0x06, (addr >> 8) as u8, addr as u8, (v >> 8) as u8, v as u8]
    };

    // request_frame* returns (tx_adu, rx_adu, rtt, resp_pdu) — keep the full
    // frame so the UI can show the raw TX/RX exchange with RTT.
    let (tx, rx, rtt_ms) = match state.conn_type.load(ORDERING) {
        CONN_TCP => {
            let mut client = state.client.lock().await;
            let c = client.as_mut().ok_or("not connected")?;
            let (tx, rx, rtt, _resp) = c
                .request_frame(unit_id, &pdu)
                .await
                .map_err(|e| e.to_string())?;
            (hex_encode(&tx), hex_encode(&rx), rtt.as_millis() as u64)
        }
        CONN_RTU => {
            let mut rtu = state.rtu.lock().unwrap();
            let r = rtu.as_mut().ok_or("not connected")?;
            // RTU frames stamp the per-request unit_id.
            let (tx, rx, rtt, _resp) = r
                .request_frame_for(unit_id, &pdu)
                .map_err(|e| e.to_string())?;
            (hex_encode(&tx), hex_encode(&rx), rtt.as_millis() as u64)
        }
        CONN_UDP => {
            let (tx, rx, rtt, _resp) = udp_frame(&state, unit_id, &pdu).await?;
            (hex_encode(&tx), hex_encode(&rx), rtt.as_millis() as u64)
        }
        _ => return Err("not connected".into()),
    };
    Ok(WriteResult { tx, rx, rtt_ms })
}

/// Read back a data point family after a write (immediate read-back).
///
/// `func`: "01" reads coils (bits → 0/1 words), "03" reads holding registers
/// (16-bit words). `unit_id` stamps the target slave for RTU framing.
/// Returns the raw values decoded from the response PDU.
/// Result of a single-shot read: decoded registers plus the raw TX/RX ADU
/// hex strings and the round-trip time, so the UI can render the frame exchange
/// in the same TX/RX style as periodic polling.
#[derive(serde::Serialize)]
pub struct ReadPointsResult {
    pub regs: Vec<u16>,
    pub tx: String,
    pub rx: String,
    pub rtt_ms: u64,
}

#[tauri::command]
pub async fn read_points(
    state: State<'_, AppState>,
    unit_id: u8,
    func: String,
    addr: u16,
    count: u16,
) -> Result<ReadPointsResult, String> {
    let fc: u8 = func.parse().unwrap_or(0x03);
    if fc != 0x01 && fc != 0x02 && fc != 0x03 && fc != 0x04 {
        return Err("read_points supports FC01/FC02/FC03/FC04 only".into());
    }
    let pdu = build_read_pdu(fc, addr, count);
    // request_frame* returns (tx_adu, rx_adu, rtt, resp_pdu) — we keep the full
    // frame so the UI can show the raw TX/RX exchange with RTT.
    let (tx, rx, rtt, resp): (Vec<u8>, Vec<u8>, std::time::Duration, Vec<u8>) =
        match state.conn_type.load(ORDERING) {
            CONN_TCP => {
                let mut client = state.client.lock().await;
                let c = client.as_mut().ok_or("not connected")?;
                c.request_frame(unit_id, &pdu)
                    .await
                    .map_err(|e| e.to_string())?
            }
            CONN_RTU => {
                let mut rtu = state.rtu.lock().unwrap();
                let r = rtu.as_mut().ok_or("not connected")?;
                // RTU frames stamp the per-request unit_id.
                r.request_frame_for(unit_id, &pdu).map_err(|e| e.to_string())?
            }
            CONN_UDP => udp_frame(&state, unit_id, &pdu).await?,
            _ => return Err("not connected".into()),
        };
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
#[derive(serde::Serialize)]
pub struct SendRawFrameResp {
    pub tx: String,
    pub rx: String,
    pub rtt_ms: u64,
    pub pdu_resp: String,
}

#[tauri::command]
pub async fn send_raw(
    state: State<'_, AppState>,
    unit_id: u8,
    hex: String,
) -> Result<String, String> {
    let pdu = hex_decode(&hex)?;
    match state.conn_type.load(ORDERING) {
        CONN_TCP => {
            let mut client = state.client.lock().await;
            let c = client.as_mut().ok_or("not connected")?;
            let resp = c
                .request_raw(unit_id, &pdu)
                .await
                .map_err(|e| e.to_string())?;
            Ok(hex_encode(&resp))
        }
        CONN_RTU => {
            let mut rtu = state.rtu.lock().unwrap();
            let r = rtu.as_mut().ok_or("not connected")?;
            // RTU transport uses its configured unit_id for framing.
            let resp = r.request(&pdu).map_err(|e| e.to_string())?;
            Ok(hex_encode(&resp))
        }
        CONN_UDP => {
            let mut udp = state.udp.lock().await;
            let u = udp.as_mut().ok_or("not connected")?;
            let resp = u.request(unit_id, &pdu).await.map_err(|e| e.to_string())?;
            Ok(hex_encode(&resp))
        }
        _ => Err("not connected".into()),
    }
}

#[tauri::command]
pub async fn send_raw_frame(
    state: State<'_, AppState>,
    unit_id: u8,
    hex: String,
) -> Result<SendRawFrameResp, String> {
    let pdu = hex_decode(&hex)?;
    let (tx, rx, rtt, resp_pdu) = match state.conn_type.load(ORDERING) {
        CONN_TCP => {
            let mut client = state.client.lock().await;
            let c = client.as_mut().ok_or("not connected")?;
            c.request_frame(unit_id, &pdu).await.map_err(|e| e.to_string())?
        }
        CONN_RTU => {
            let mut rtu = state.rtu.lock().unwrap();
            let r = rtu.as_mut().ok_or("not connected")?;
            r.request_frame_for(unit_id, &pdu).map_err(|e| e.to_string())?
        }
        CONN_UDP => {
            let mut udp = state.udp.lock().await;
            let u = udp.as_mut().ok_or("not connected")?;
            u.request_frame(unit_id, &pdu).await.map_err(|e| e.to_string())?
        }
        _ => return Err("not connected".into()),
    };
    Ok(SendRawFrameResp {
        tx: hex_encode(&tx),
        rx: hex_encode(&rx),
        rtt_ms: rtt.as_millis() as u64,
        pdu_resp: hex_encode(&resp_pdu),
    })
}

// ══════════════════════════════════════════════════════════════════
// Polling — multi-poll concurrent model
// ══════════════════════════════════════════════════════════════════

/// One poll-cycle exchange: `(regs, tx_hex, rx_hex, rtt_ms)`.
type PollOutcome = Result<(Vec<u16>, String, String, u64), String>;

/// Build a read PDU for a function code: FC01/FC02 → bit reads (count = bits),
/// FC03/FC04 → register reads.
fn build_read_pdu(func: u8, addr: u16, count: u16) -> Vec<u8> {
    vec![func, (addr >> 8) as u8, addr as u8, (count >> 8) as u8, count as u8]
}

/// Decode a read response PDU into register values.
/// FC01/FC02 responses are bit-packed — each bit becomes a u16 (0/1).
/// FC03/FC04 responses are 16-bit registers.
fn decode_read(resp: &[u8], func: u8, count: usize) -> Vec<u16> {
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

/// Spawn a dedicated background task for one poll config.
/// `poll_id` is assigned by the frontend (must be unique per active poll).
/// `func` selects the function code ("01"|"02"|"03"|"04").
/// Returns `Err` if a poll with the same id is already running.
#[tauri::command]
pub async fn start_poll(
    state: State<'_, AppState>,
    app: AppHandle,
    poll_id: u32,
    poll_name: String,
    func: String,
    addr: u16,
    count: u16,
    interval_ms: u64,
    unit_id: u8,
) -> Result<(), String> {
    {
        let map = state.poll_tasks.lock().unwrap();
        if map.contains_key(&poll_id) {
            return Err(format!("poll {poll_id} already running"));
        }
    }

    let fc: u8 = func.parse().unwrap_or(0x03);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let client = state.client.clone();
    let rtu = state.rtu.clone();
    let udp = state.udp.clone();
    let conn = state.conn_type.clone();
    let stop_inner = stop.clone();
    let pdu = build_read_pdu(fc, addr, count);
    let app_inner = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        while !stop_inner.load(ORDERING) {
            let outcome: Option<PollOutcome> = match conn.load(ORDERING) {
                CONN_TCP => {
                    let mut guard = client.lock().await;
                    if let Some(c) = guard.as_mut() {
                        match c.request_frame(unit_id, &pdu).await {
                            Ok((tx, rx, rtt, resp)) => Some(Ok((
                                decode_read(&resp, fc, count as usize),
                                hex_encode(&tx),
                                hex_encode(&rx),
                                rtt.as_millis() as u64,
                            ))),
                            Err(e) => Some(Err(e.to_string())),
                        }
                    } else {
                        None
                    }
                }
                CONN_RTU => {
                    let mut guard = rtu.lock().unwrap();
                    if let Some(r) = guard.as_mut() {
                        // RTU frames stamp the per-request unit_id.
                        match r.request_frame_for(unit_id, &pdu) {
                            Ok((tx, rx, rtt, resp)) => Some(Ok((
                                decode_read(&resp, fc, count as usize),
                                hex_encode(&tx),
                                hex_encode(&rx),
                                rtt.as_millis() as u64,
                            ))),
                            Err(e) => Some(Err(e.to_string())),
                        }
                    } else {
                        None
                    }
                }
                CONN_UDP => {
                    let mut guard = udp.lock().await;
                    if let Some(u) = guard.as_mut() {
                        match u.request_frame(unit_id, &pdu).await {
                            Ok((tx, rx, rtt, resp)) => Some(Ok((
                                decode_read(&resp, fc, count as usize),
                                hex_encode(&tx),
                                hex_encode(&rx),
                                rtt.as_millis() as u64,
                            ))),
                            Err(e) => Some(Err(e.to_string())),
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };

            match outcome {
                Some(Ok((regs, tx, rx, rtt))) => {
                    let _ = app_inner.emit(
                        "poll-frame",
                        serde_json::json!({
                            "pollId": poll_id, "pollName": poll_name,
                            "addr": addr, "regs": regs,
                            "tx": tx, "rx": rx, "rttMs": rtt,
                        }),
                    );
                }
                Some(Err(e)) => {
                    let _ = app_inner.emit(
                        "poll-frame",
                        serde_json::json!({
                            "pollId": poll_id, "pollName": poll_name,
                            "addr": addr, "regs": null,
                            "tx": "", "rx": "", "rttMs": null,
                            "error": e,
                        }),
                    );
                }
                None => {
                    // Disconnected while running — exit the loop.
                    break;
                }
            }
            sleep(Duration::from_millis(interval_ms)).await;
        }
    });

    state.poll_tasks.lock().unwrap().insert(
        poll_id,
        crate::state::PollTask { stop: stop.clone(), handle },
    );
    state.poll_running.store(true, ORDERING);
    Ok(())
}

/// Stop a single poll by id.
#[tauri::command]
pub async fn stop_poll(state: State<'_, AppState>, poll_id: u32) -> Result<(), String> {
    if let Some(task) = state.poll_tasks.lock().unwrap().remove(&poll_id) {
        task.stop.store(true, ORDERING);
        task.handle.abort();
    }
    if state.poll_tasks.lock().unwrap().is_empty() {
        state.poll_running.store(false, ORDERING);
    }
    Ok(())
}

/// Stop every active poll (used on disconnect).
#[tauri::command]
pub async fn stop_all_polls(state: State<'_, AppState>) -> Result<(), String> {
    let tasks: Vec<crate::state::PollTask> =
        state.poll_tasks.lock().unwrap().drain().map(|(_, v)| v).collect();
    for t in tasks {
        t.stop.store(true, ORDERING);
        t.handle.abort();
    }
    state.poll_running.store(false, ORDERING);
    Ok(())
}

// ══════════════════════════════════════════════════════════════════
// 自动变化（每寄存器独立）—— 常驻循环
// ══════════════════════════════════════════════════════════════════

/// 启动常驻自动变化循环：每 500ms 遍历所有 Unit，按各寄存器的 `vary`
/// 配置（sine/random/increment）驱动对应保持寄存器，有变化才广播快照。
/// 自动变化在「编辑寄存器」里按寄存器设置，无需手动开关。
pub fn spawn_vary_loop(app: &AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut tick: u64 = 0;
        loop {
            tick = tick.wrapping_add(1);
            sleep(Duration::from_millis(500)).await;
            let state = handle.state::<AppState>();
            
            // 如果没有启动任何模拟模式，则完全停止数据变化
            if state.slave_mode.load(crate::state::ORDERING) == 0 {
                continue;
            }
            
            let units = state.slave_units.clone();
            let sim = state.simulator.clone();
            let guard = units.lock().unwrap();
            for u in guard.iter() {
                // 保持寄存器与输入寄存器都支持自动变化：按区域分别收集。
                let (holding_varies, input_varies): (Vec<(u16, VaryMode)>, Vec<(u16, VaryMode)>) = {
                    let regs = u.regs.lock().unwrap();
                    regs.iter()
                        .filter(|d| d.vary != "off")
                        .fold(
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
                let mut s = sim.lock().unwrap();
                let mut img = u.image.lock().unwrap();
                s.step_on(&mut img, &holding_varies, &input_varies);
                let snap = server::snapshot_from(&img);
                drop(img);
                drop(s);
                let _ = handle.emit(
                    "slave-values",
                    serde_json::json!({
                        "unitId": u.unit_id,
                        "coils": snap.coils,
                        "discreteInputs": snap.discrete_inputs,
                        "inputRegisters": snap.input_registers,
                        "holdingRegisters": snap.holding_registers,
                    }),
                );
            }
        }
    });
}

// ══════════════════════════════════════════════════════════════════
// Built-in slave (simulator as a real device)
// ══════════════════════════════════════════════════════════════════

/// 构建「主站写入」回调：写成功即推送该 Unit 的 `slave-values` 快照，
/// 让模拟器寄存器显示页与主站写入实时同步。
fn on_write_snapshot(app: &AppHandle, state: &AppState) -> server::OnWrite {
    let app_inner = app.clone();
    let unit_table = state.slave_units.clone();
    Arc::new(move |unit_id| {
        let found = unit_table
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.unit_id == unit_id)
            .map(|u| u.image.clone());
        if let Some(img) = found {
            let snap = server::snapshot(&img);
            let _ = app_inner.emit(
                "slave-values",
                serde_json::json!({
                    "unitId": unit_id,
                    "coils": snap.coils,
                    "discreteInputs": snap.discrete_inputs,
                    "inputRegisters": snap.input_registers,
                    "holdingRegisters": snap.holding_registers,
                }),
            );
        }
    })
}

/// 启动内置从站：根据 `mode` 启用一种运行模式（5 种之一）。
/// - `tcp`       Modbus TCP（MBAP 帧，绑定 `bind:port`）
/// - `udp`       Modbus UDP（MBAP 帧承载于 UDP 数据报）
/// - `rtu_tcp`   Modbus RTU over TCP/IP（RTU 帧承载于 TCP 流，无 MBAP 头）
/// - `rtu_udp`   Modbus RTU over UDP/IP（RTU 帧承载于 UDP 数据报，无 MBAP 头）
/// - `rtu`       Modbus RTU（串口）
///
/// 各模式共享同一份 Unit 镜像，可同时运行多个模式；同一模式重复启动会先停后起
/// （配置即时生效，无需重启）。所有模式的寄存器空间完全一致，运行时增删寄存器
/// 立即对所有在线模式生效。
#[derive(Debug, Deserialize, serde::Serialize)]
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

fn set_slave_bit(state: &AppState, bit: u8) {
    state.slave_mode.fetch_or(bit, ORDERING);
}

fn clear_slave_bit(state: &AppState, bit: u8) {
    state.slave_mode.fetch_and(!bit, ORDERING);
}

/// 停止某一模式（幂等），不影响其它运行中的模式。
fn sim_slave_stop_inner(state: &AppState, mode: &str) {
    match mode {
        "tcp" => {
            let _ = state.slave_tcp.shutdown.send(());
            if let Some(h) = state.slave_tcp.handle.lock().unwrap().take() {
                h.abort();
            }
            state.slave_tcp.conns.store(0, ORDERING);
            clear_slave_bit(state, SLAVE_TCP);
        }
        "udp" => {
            let _ = state.slave_udp.shutdown.send(());
            if let Some(h) = state.slave_udp.handle.lock().unwrap().take() {
                h.abort();
            }
            clear_slave_bit(state, SLAVE_UDP);
        }
        "rtu_tcp" => {
            let _ = state.slave_rtu_tcp.shutdown.send(());
            if let Some(h) = state.slave_rtu_tcp.handle.lock().unwrap().take() {
                h.abort();
            }
            clear_slave_bit(state, SLAVE_RTU_TCP);
        }
        "rtu_udp" => {
            let _ = state.slave_rtu_udp.shutdown.send(());
            if let Some(h) = state.slave_rtu_udp.handle.lock().unwrap().take() {
                h.abort();
            }
            clear_slave_bit(state, SLAVE_RTU_UDP);
        }
        "rtu" => {
            state.slave_rtu_serial.stop.store(true, ORDERING);
            clear_slave_bit(state, SLAVE_RTU);
        }
        _ => {}
    }
}

#[tauri::command]
pub async fn sim_slave_start(
    state: State<'_, AppState>,
    app: AppHandle,
    req: SlaveStartReq,
) -> Result<(), String> {
    // 先停掉同一模式（幂等：便于「重启」即时生效）
    sim_slave_stop_inner(&state, &req.mode);

    let units = state.unit_images.clone();
    let on_write = on_write_snapshot(&app, &state);
    if let Ok(cfg) = serde_json::to_value(&req) {
        state.slave_configs.lock().unwrap().insert(req.mode.clone(), cfg);
    }

    match req.mode.as_str() {
        "tcp" => {
            let bind = req.bind.clone().unwrap_or_else(|| "0.0.0.0".into());
            let port = req.port.unwrap_or(502);
            let rx = state.slave_tcp.shutdown.subscribe();
            let conns = state.slave_tcp.conns.clone();
            let handle = tauri::async_runtime::spawn(async move {
                let _ = server::run_tcp_slave(units, on_write, &bind, port, rx, conns).await;
            });
            *state.slave_tcp.handle.lock().unwrap() = Some(handle);
            set_slave_bit(&state, SLAVE_TCP);
            Ok(())
        }
        "udp" => {
            let bind = req.bind.clone().unwrap_or_else(|| "0.0.0.0".into());
            let port = req.port.unwrap_or(502);
            let rx = state.slave_udp.shutdown.subscribe();
            let handle = tauri::async_runtime::spawn(async move {
                let _ = server::run_udp_slave(units, on_write, &bind, port, rx).await;
            });
            *state.slave_udp.handle.lock().unwrap() = Some(handle);
            set_slave_bit(&state, SLAVE_UDP);
            Ok(())
        }
        "rtu_tcp" => {
            let bind = req.bind.clone().unwrap_or_else(|| "0.0.0.0".into());
            let port = req.port.unwrap_or(502);
            let rx = state.slave_rtu_tcp.shutdown.subscribe();
            let conns = state.slave_rtu_tcp.conns.clone();
            let handle = tauri::async_runtime::spawn(async move {
                let _ = server::run_rtu_over_tcp_slave(units, on_write, &bind, port, rx, conns).await;
            });
            *state.slave_rtu_tcp.handle.lock().unwrap() = Some(handle);
            set_slave_bit(&state, SLAVE_RTU_TCP);
            Ok(())
        }
        "rtu_udp" => {
            let bind = req.bind.clone().unwrap_or_else(|| "0.0.0.0".into());
            let port = req.port.unwrap_or(502);
            let rx = state.slave_rtu_udp.shutdown.subscribe();
            let handle = tauri::async_runtime::spawn(async move {
                let _ = server::run_rtu_over_udp_slave(units, on_write, &bind, port, rx).await;
            });
            *state.slave_rtu_udp.handle.lock().unwrap() = Some(handle);
            set_slave_bit(&state, SLAVE_RTU_UDP);
            Ok(())
        }
        "rtu" => {
            let port_name = req.port_name.clone().ok_or("请选择串口")?;
            let baud = req.baud_rate.unwrap_or(9600);
            let parity = req.parity.clone().unwrap_or_else(|| "none".into());
            let data_bits = req.data_bits.unwrap_or(8);
            let stop_bits = req.stop_bits.unwrap_or(1);
            let inter = req.inter_frame_ms.unwrap_or(5);
            // 先在命令侧打开串口：打开失败（端口不存在 / 权限不足 / 被占用）
            // 直接返回错误给前端，而不是像以前那样吞掉错误却仍显示「运行中」。
            let port = server::open_rtu_port(&port_name, baud, data_bits, stop_bits, &parity)
                .map_err(|e| e.to_string())?;
            // 复位停止标志：sim_slave_stop_inner 已将其置 true（幂等停止旧线程），
            // 新线程必须以 false 启动，否则会在循环顶部立即 break 退出 —— 表现就是
            // 「从站显示了运行中却不应答主站」（这正是停止后再启动 RTU 失效的根因）。
            state.slave_rtu_serial.stop.store(false, ORDERING);
            let units = units.clone();
            let on_write = on_write.clone();
            let stop = state.slave_rtu_serial.stop.clone();
            let handle = thread::spawn(move || {
                let _ = server::run_rtu_slave_on_port(port, units, on_write, inter, stop);
            });
            *state.slave_rtu_serial.handle.lock().unwrap() = Some(handle);
            set_slave_bit(&state, SLAVE_RTU);
            Ok(())
        }
        _ => Err(format!("未知模拟模式：{}", req.mode)),
    }
}

/// 停止某一运行模式（不影响其它模式）。
#[tauri::command]
pub async fn sim_slave_stop(state: State<'_, AppState>, mode: String) -> Result<(), String> {
    sim_slave_stop_inner(&state, &mode);
    Ok(())
}

/// 停止全部运行中的从站模式。
#[tauri::command]
pub async fn sim_slave_stop_all(state: State<'_, AppState>) -> Result<(), String> {
    let m = state.slave_mode.load(ORDERING);
    if (m & SLAVE_TCP) != 0 {
        sim_slave_stop_inner(&state, "tcp");
    }
    if (m & SLAVE_UDP) != 0 {
        sim_slave_stop_inner(&state, "udp");
    }
    if (m & SLAVE_RTU_TCP) != 0 {
        sim_slave_stop_inner(&state, "rtu_tcp");
    }
    if (m & SLAVE_RTU_UDP) != 0 {
        sim_slave_stop_inner(&state, "rtu_udp");
    }
    if (m & SLAVE_RTU) != 0 {
        sim_slave_stop_inner(&state, "rtu");
    }
    Ok(())
}

/// Emit the current snapshot of a given Unit to the UI (`slave-values` event),
/// so the simulator editor stays in sync after every UI write.
fn emit_slave_snapshot(app: &AppHandle, state: &AppState, unit_id: u8) {
    let Ok(img) = unit_image(state, unit_id) else { return; };
    let snap = server::snapshot(&img);
    let _ = app.emit(
        "slave-values",
        serde_json::json!({
            "unitId": unit_id,
            "coils": snap.coils,
            "discreteInputs": snap.discrete_inputs,
            "inputRegisters": snap.input_registers,
            "holdingRegisters": snap.holding_registers,
        }),
    );
}

/// Set a single holding register in a Unit's image (UI editor write).
#[tauri::command]
pub async fn sim_set_register(
    state: State<'_, AppState>,
    app: AppHandle,
    addr: usize,
    value: u16,
    unit_id: Option<u8>,
) -> Result<(), String> {
    let uid = unit_id.unwrap_or(1);
    let img = unit_image(&state, uid)?;
    img.lock().unwrap().set_holding(addr, value);
    emit_slave_snapshot(&app, &state, uid);
    Ok(())
}

/// Set a single input register in a Unit's image (UI editor write).
#[tauri::command]
pub async fn sim_set_input(
    state: State<'_, AppState>,
    app: AppHandle,
    addr: usize,
    value: u16,
    unit_id: Option<u8>,
) -> Result<(), String> {
    let uid = unit_id.unwrap_or(1);
    let img = unit_image(&state, uid)?;
    img.lock().unwrap().set_input(addr, value);
    emit_slave_snapshot(&app, &state, uid);
    Ok(())
}

/// Set a single coil in a Unit's image (UI editor write).
#[tauri::command]
pub async fn sim_set_coil(
    state: State<'_, AppState>,
    app: AppHandle,
    addr: usize,
    on: bool,
    unit_id: Option<u8>,
) -> Result<(), String> {
    let uid = unit_id.unwrap_or(1);
    let img = unit_image(&state, uid)?;
    img.lock().unwrap().set_coil(addr, on);
    emit_slave_snapshot(&app, &state, uid);
    Ok(())
}

/// Set a single discrete input in a Unit's image (UI editor write).
#[tauri::command]
pub async fn sim_set_discrete(
    state: State<'_, AppState>,
    app: AppHandle,
    addr: usize,
    on: bool,
    unit_id: Option<u8>,
) -> Result<(), String> {
    let uid = unit_id.unwrap_or(1);
    let img = unit_image(&state, uid)?;
    img.lock().unwrap().set_discrete(addr, on);
    emit_slave_snapshot(&app, &state, uid);
    Ok(())
}

/// Reset every table of the given Unit's image to zero.
#[tauri::command]
pub async fn sim_reset(state: State<'_, AppState>, unit_id: Option<u8>) -> Result<(), String> {
    let uid = unit_id.unwrap_or(1);
    let img = unit_image(&state, uid)?;
    img.lock().unwrap().reset();
    Ok(())
}

/// Return a snapshot of the given Unit's image for the UI data tables.
#[tauri::command]
pub async fn sim_snapshot(
    state: State<'_, AppState>,
    unit_id: Option<u8>,
) -> Result<ImageSnapshot, String> {
    let uid = unit_id.unwrap_or(1);
    let img = unit_image(&state, uid)?;
    Ok(server::snapshot(&img))
}

/// 返回各从站模式的运行状态 + TCP 连接数 + 最近启动配置（供对话框回填）。
#[tauri::command]
pub async fn sim_slave_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let m = state.slave_mode.load(ORDERING);
    let mut modes = std::collections::HashMap::new();
    modes.insert("tcp".to_string(), (m & SLAVE_TCP) != 0);
    modes.insert("udp".to_string(), (m & SLAVE_UDP) != 0);
    modes.insert("rtu_tcp".to_string(), (m & SLAVE_RTU_TCP) != 0);
    modes.insert("rtu_udp".to_string(), (m & SLAVE_RTU_UDP) != 0);
    modes.insert("rtu".to_string(), (m & SLAVE_RTU) != 0);
    let tcp_conns = state.slave_tcp.conns.load(ORDERING);
    let configs = state.slave_configs.lock().unwrap().clone();
    Ok(serde_json::json!({
        "modes": modes,
        "tcpConns": tcp_conns,
        "configs": configs,
    }))
}

// ══════════════════════════════════════════════════════════════════
// Config
// ══════════════════════════════════════════════════════════════════

#[tauri::command]
pub async fn save_config(state: State<'_, AppState>, path: String) -> Result<(), String> {
    state
        .config
        .lock()
        .unwrap()
        .save(std::path::Path::new(&path))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn load_config(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let ws = Workspace::load(std::path::Path::new(&path)).map_err(|e| e.to_string())?;
    *state.config.lock().unwrap() = ws;
    Ok(())
}

// ══════════════════════════════════════════════════════════════════
// ══════════════════════════════════════════════════════════════════
// Serial port list (for RTU connection dialog)
// ══════════════════════════════════════════════════════════════════

/// List available serial ports (for RTU connection picker).
/// On macOS, filters to `cu.*` devices (calling-unit, appropriate for master mode)
/// to avoid duplicating each physical port as both `cu.*` and `tty.*`.
#[tauri::command]
pub async fn list_serial_ports() -> Result<Vec<String>, String> {
    let ports = serial2::SerialPort::available_ports().map_err(|e| e.to_string())?;
    let mut names: Vec<String> = ports
        .iter()
        .map(|p| p.display().to_string())
        .filter(|n| !n.contains("/dev/tty."))
        .collect();
    if names.is_empty() {
        // Fallback: show all ports (non-macOS or no cu.* devices).
        names = ports.iter().map(|p| p.display().to_string()).collect();
    }
    names.sort();
    Ok(names)
}

// ---- hex helpers (UI sends/reads human hex strings) ----

/// Translate serial-open errors into actionable hints.
///
/// On macOS/Linux a serial device opened by another process (browser Web
/// Serial, Arduino IDE monitor, minicom, screen, …) returns EBUSY — the
/// common `Resource busy (os error 16)`. Detect it and tell the user what
/// usually holds the port instead of returning a bare OS error.
fn friendly_serial_error(msg: String) -> String {
    if msg.contains("Resource busy") || msg.contains("os error 16") {
        format!(
            "{msg}\n\n提示：串口已被其他程序独占用，请先关闭占用该串口的程序，或重启该串口设备后重试。"
        )
    } else {
        msg
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
    b.iter()
        .map(|x| format!("{:02X}", x))
        .collect::<Vec<_>>()
        .join(" ")
}

// ══════════════════════════════════════════════════════════════════
// 通信日志导出
// ══════════════════════════════════════════════════════════════════

/// 弹出「另存为」对话框，将通信日志文本写入用户选择的 .txt 文件。
/// 返回保存路径（用户取消则返回空字符串）。
#[tauri::command]
pub async fn export_log_txt(app: AppHandle, content: String) -> Result<String, String> {
    let path = app
        .dialog()
        .file()
        .add_filter("文本文件", &["txt"])
        .set_file_name("modbus_log.txt")
        .blocking_save_file();

    let path = match path {
        Some(p) => p.to_string(),
        None => return Ok(String::new()), // 用户取消
    };

    std::fs::write(&path, &content).map_err(|e| format!("写入文件失败：{e}"))?;
    Ok(path)
}

// ══════════════════════════════════════════════════════════════════
// 项目配置导入与导出
// ══════════════════════════════════════════════════════════════════

/// 弹出「另存为」对话框，将项目配置 JSON 字符串保存到文件。
/// 返回保存路径（用户取消则返回空字符串）。
#[tauri::command]
pub async fn save_project_file(app: AppHandle, content: String) -> Result<String, String> {
    let path = app
        .dialog()
        .file()
        .add_filter("Modbus Tool 项目文件", &["json", "mbproj"])
        .add_filter("所有文件", &["*"])
        .set_file_name("modbus_project.json")
        .blocking_save_file();

    let path = match path {
        Some(p) => p.to_string(),
        None => return Ok(String::new()), // 用户取消
    };

    std::fs::write(&path, &content).map_err(|e| format!("保存项目文件失败：{e}"))?;
    Ok(path)
}

/// 弹出「打开文件」对话框，读取项目配置文件并返回文件内容。
/// 若用户取消则返回空字符串。
#[tauri::command]
pub async fn import_project_file(app: AppHandle) -> Result<String, String> {
    let path = app
        .dialog()
        .file()
        .add_filter("Modbus Tool 项目文件", &["json", "mbproj"])
        .add_filter("所有文件", &["*"])
        .blocking_pick_file();

    let path = match path {
        Some(p) => p.to_string(),
        None => return Ok(String::new()), // 用户取消
    };

    let content = std::fs::read_to_string(&path).map_err(|e| format!("读取项目文件失败：{e}"))?;
    Ok(content)
}

// ══════════════════════════════════════════════════════════════════
// 多 Unit 模拟：Unit ID 管理（增删）
// ══════════════════════════════════════════════════════════════════

/// 列出当前所有被模拟的 Unit（id + 寄存器数量）。
#[tauri::command]
pub async fn unit_list(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let units = state.slave_units.lock().unwrap();
    Ok(units
        .iter()
        .map(|u| {
            serde_json::json!({
                "unit_id": u.unit_id,
                "reg_count": u.regs.lock().unwrap().len(),
            })
        })
        .collect())
}

/// 新增一个被模拟的 Unit ID（自带少量示例寄存器，独立数据空间）。
#[tauri::command]
pub async fn unit_add(state: State<'_, AppState>, unit_id: u8) -> Result<(), String> {
    if unit_id == 0 || unit_id > 247 {
        return Err("Unit ID 须在 1 ~ 247".into());
    }
    let mut units = state.slave_units.lock().unwrap();
    if units.iter().any(|u| u.unit_id == unit_id) {
        return Err(format!("Unit ID {unit_id} 已存在"));
    }
    units.push(new_sim_unit(unit_id));
    drop(units);
    state.rebuild_unit_images();
    Ok(())
}

/// 移除一个被模拟的 Unit ID（至少保留一个）。
#[tauri::command]
pub async fn unit_remove(state: State<'_, AppState>, unit_id: u8) -> Result<(), String> {
    let mut units = state.slave_units.lock().unwrap();
    if units.len() <= 1 {
        return Err("至少保留一个 Unit ID".into());
    }
    let before = units.len();
    units.retain(|u| u.unit_id != unit_id);
    if units.len() == before {
        return Err(format!("Unit ID {unit_id} 不存在"));
    }
    drop(units);
    state.rebuild_unit_images();
    Ok(())
}

// ══════════════════════════════════════════════════════════════════
// 模拟器寄存器注册表（按 Unit 独立，按需注册）
// ══════════════════════════════════════════════════════════════════

/// 向所有窗口广播 `sim-regs-updated`（指定 Unit 的完整定义列表 + 数值快照）。
fn emit_regs(app: &AppHandle, state: &AppState, unit_id: u8) {
    let Ok(regs) = unit_regs(state, unit_id) else { return; };
    let Ok(img) = unit_image(state, unit_id) else { return; };
    let resp = RegListResp {
        unit_id,
        defs: regs.lock().unwrap().clone(),
        snapshot: server::snapshot(&img),
    };
    let _ = app.emit("sim-regs-updated", resp);
}

/// 查询指定 Unit 的寄存器列表。`keyword` 可选：按名称 / 区域 / 地址过滤。
#[tauri::command]
pub async fn sim_reg_list(
    state: State<'_, AppState>,
    keyword: Option<String>,
    unit_id: Option<u8>,
) -> Result<RegListResp, String> {
    let uid = unit_id.unwrap_or(1);
    let regs = unit_regs(&state, uid)?;
    let img = unit_image(&state, uid)?;
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
    Ok(RegListResp {
        unit_id: uid,
        defs: filtered,
        snapshot: server::snapshot(&img),
    })
}

/// 按需新增一个寄存器到指定 Unit：注册定义并立即在内核镜像创建对应槽位。
#[tauri::command]
pub async fn sim_reg_add(
    app: AppHandle,
    state: State<'_, AppState>,
    input: RegInput,
    unit_id: Option<u8>,
) -> Result<RegisterDef, String> {
    let uid = unit_id.unwrap_or(1);
    let regs = unit_regs(&state, uid)?;
    let img = unit_image(&state, uid)?;
    let mut defs = regs.lock().unwrap();
    if defs
        .iter()
        .any(|d| d.area == input.area && d.addr == input.addr)
    {
        // 引导式提示：告知已存在 + 给出替换/编辑两个出口（前端会先拦截并弹替换确认）。
        return Err(format!(
            "{} 0x{:04X} 已存在。如需修改该地址的寄存器，请使用「编辑」，\
             或在弹出提示中选择「替换」用新配置覆盖。",
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
    emit_regs(&app, &state, uid);
    Ok(def)
}

/// 修改指定 Unit 的寄存器配置。地址或区域移动时同步清除旧槽位。
#[tauri::command]
pub async fn sim_reg_update(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    input: RegInput,
    unit_id: Option<u8>,
) -> Result<(), String> {
    let uid = unit_id.unwrap_or(1);
    let regs = unit_regs(&state, uid)?;
    let img = unit_image(&state, uid)?;
    let mut defs = regs.lock().unwrap();
    if defs
        .iter()
        .any(|d| d.id != id && d.area == input.area && d.addr == input.addr)
    {
        return Err(format!(
            "{} 0x{:04X} 已被其他寄存器占用，请换个地址",
            area_label(&input.area),
            input.addr
        ));
    }
    let def = defs
        .iter_mut()
        .find(|d| d.id == id)
        .ok_or("寄存器不存在")?;
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
    emit_regs(&app, &state, uid);
    Ok(())
}

/// 删除指定 Unit 的寄存器：移除定义并清除内核镜像对应槽位。
#[tauri::command]
pub async fn sim_reg_delete(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    unit_id: Option<u8>,
) -> Result<(), String> {
    let uid = unit_id.unwrap_or(1);
    let regs = unit_regs(&state, uid)?;
    let img = unit_image(&state, uid)?;
    let mut defs = regs.lock().unwrap();
    let idx = defs
        .iter()
        .position(|d| d.id == id)
        .ok_or("寄存器不存在")?;
    let d = defs.remove(idx);
    img.lock().unwrap().clear_slot(&d.area, d.addr);
    drop(defs);
    emit_regs(&app, &state, uid);
    Ok(())
}

/// 恢复指定 Unit 为初始「少量示例寄存器」（清空当前列表后重建种子）。
#[tauri::command]
pub async fn sim_reg_seed(
    app: AppHandle,
    state: State<'_, AppState>,
    unit_id: Option<u8>,
) -> Result<(), String> {
    let uid = unit_id.unwrap_or(1);
    let regs = unit_regs(&state, uid)?;
    let img = unit_image(&state, uid)?;
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
    emit_regs(&app, &state, uid);
    Ok(())
}

// ── 批量新增寄存器 ──────────────────────────────────────────────────
/// 从 `start_addr` 开始，连续新增 `count` 个相同属性的寄存器。
/// 跳过地址已存在的条目（不报错，只跳过）。
/// 返回实际成功新增的数量。
#[tauri::command]
pub async fn sim_reg_add_batch(
    app: AppHandle,
    state: State<'_, AppState>,
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
    let regs = unit_regs(&state, uid)?;
    let img  = unit_image(&state, uid)?;
    let mut defs = regs.lock().unwrap();
    let mut added: u16 = 0;
    for i in 0..count {
        let addr = start_addr.saturating_add(i);
        // 跳过已存在的地址
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
    emit_regs(&app, &state, uid);
    Ok(added)
}

// ── Excel 导出 ──────────────────────────────────────────────────────
/// 弹出「另存为」对话框，把当前 Unit 所有寄存器导出为 xlsx。
#[tauri::command]
pub async fn sim_reg_export_xlsx(
    app: AppHandle,
    state: State<'_, AppState>,
    unit_id: Option<u8>,
) -> Result<String, String> {
    let uid = unit_id.unwrap_or(1);
    let regs = unit_regs(&state, uid)?;
    let img  = unit_image(&state, uid)?;

    // 收集数据（释放锁再操作 IO）
    let defs: Vec<RegisterDef>;
    let snap: modbus_core::server::ImageSnapshot;
    {
        defs = regs.lock().unwrap().clone();
        snap = modbus_core::server::snapshot_from(&img.lock().unwrap());
    }

    // 弹出保存对话框
    let path = app
        .dialog()
        .file()
        .add_filter("Excel 工作簿", &["xlsx"])
        .set_file_name(format!("modbus_unit{uid}_regs.xlsx"))
        .blocking_save_file();

    let path = match path {
        Some(p) => p.to_string(),
        None => return Ok(String::new()), // 用户取消
    };

    // 构建 xlsx
    let mut wb  = Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("寄存器列表").map_err(|e| e.to_string())?;

    // 样式
    let hdr = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0x1d1d1f))
        .set_font_color(Color::White);
    let headers = ["区域", "地址(DEC)", "地址(HEX)", "名称", "数据类型", "访问", "自动变化", "当前值"];
    for (c, h) in headers.iter().enumerate() {
        ws.write_with_format(0, c as u16, *h, &hdr).map_err(|e| e.to_string())?;
    }

    // 设置列宽
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
            "input"   => snap.input_registers.get(&def.addr).copied().unwrap_or(0) as f64,
            "coil"    => if snap.coils.get(&def.addr).copied().unwrap_or(false) { 1.0 } else { 0.0 },
            _         => if snap.discrete_inputs.get(&def.addr).copied().unwrap_or(false) { 1.0 } else { 0.0 },
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

// ── Excel 导入 ──────────────────────────────────────────────────────
/// 弹出「打开文件」对话框，读取 xlsx 并导入寄存器（追加模式：跳过重复地址）。
/// 返回 (成功导入数量, 跳过数量)。
#[tauri::command]
pub async fn sim_reg_import_xlsx(
    app: AppHandle,
    state: State<'_, AppState>,
    unit_id: Option<u8>,
    replace: bool,
) -> Result<(u32, u32), String> {
    let uid = unit_id.unwrap_or(1);

    // 弹出文件选择对话框
    let path = app
        .dialog()
        .file()
        .add_filter("Excel 工作簿", &["xlsx", "xls", "ods"])
        .blocking_pick_file();

    let path = match path {
        Some(p) => p.to_string(),
        None => return Ok((0, 0)), // 用户取消
    };

    // 读取 Excel（在阻塞线程里做）
    let rows: Vec<Vec<String>> = tokio::task::spawn_blocking(move || {
        let mut xl = open_workbook_auto(&path).map_err(|e| format!("打开文件失败：{e}"))?;
        let sheet = xl.worksheet_range_at(0)
            .ok_or_else(|| "文件没有工作表".to_string())?
            .map_err(|e| format!("读取工作表失败：{e}"))?;
        let mut rows = Vec::new();
        for row in sheet.rows().skip(1) { // skip header
            let cells: Vec<String> = row.iter().map(|c| c.to_string()).collect();
            if cells.len() >= 7 { rows.push(cells); }
        }
        Ok::<_, String>(rows)
    }).await.map_err(|e| e.to_string())??;

    let regs = unit_regs(&state, uid)?;
    let img  = unit_image(&state, uid)?;
    let mut defs = regs.lock().unwrap();
    let mut ok_count: u32 = 0;
    let mut skip_count: u32 = 0;

    for cells in &rows {
        let area  = cells.first().map(|s| s.trim().to_lowercase()).unwrap_or_default();
        let addr_s = cells.get(1).map(|s| s.trim().to_string()).unwrap_or_default();
        let addr: u16 = match addr_s.parse() {
            Ok(v) => v,
            Err(_) => { skip_count += 1; continue; }
        };
        if !matches!(area.as_str(), "holding"|"input"|"coil"|"discrete") {
            skip_count += 1;
            continue;
        }
        let name   = cells.get(3).cloned().unwrap_or_default();
        let dtype  = cells.get(4).cloned().unwrap_or_else(|| "u16".into());
        let access = cells.get(5).cloned().unwrap_or_else(|| "rw".into());
        let vary   = cells.get(6).cloned().unwrap_or_else(|| "off".into());
        let value: f64 = cells.get(7).and_then(|s| s.parse().ok()).unwrap_or(0.0);

        if let Some(pos) = defs.iter().position(|d| d.area == area && d.addr == addr) {
            if replace {
                // 替换模式：更新已有定义
                defs[pos].name   = name.clone();
                defs[pos].dtype  = dtype.clone();
                defs[pos].access = access.clone();
                defs[pos].vary   = vary.clone();
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
    emit_regs(&app, &state, uid);
    Ok((ok_count, skip_count))
}
