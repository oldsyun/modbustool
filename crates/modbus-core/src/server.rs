//! Built-in Modbus **slave** (server) engine — turns the app into a real
//! device a master can talk to, with zero hardware.
//!
//! - [`TcpSlaveServer`] binds `0.0.0.0`, accepts many concurrent masters, and
//!   answers every Unit ID (it echoes back the request's Unit ID).
//! - [`RtuSlaveServer`] (feature `rtu`) answers on a serial port with RTU
//!   frames (CRC validated). It also answers every Unit ID (promiscuous).
//!
//! Both servers share one [`SharedImage`], so a value written over TCP is
//! immediately visible to an RTU master and vice-versa, and to the UI editor.

use crate::error::exception;
use crate::error::ModbusError;
use crate::framing;
use crate::slave::DataImage;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A data image shared between the TCP slave, the RTU slave and the UI
/// editor. `std::sync::Mutex` (not tokio) so it can be locked from the async
/// TCP tasks *and* the blocking serial thread.
pub type SharedImage = Arc<Mutex<DataImage>>;

/// 多 Unit ID 模拟：Unit ID → 该从站的独立数据镜像。
/// 每个 Unit ID 拥有各自的寄存器空间（多从站共线）。
pub type UnitImages = HashMap<u8, SharedImage>;

/// Construct a fresh **sparse** shared image. Tables start empty and grow
/// on demand (register written) — nothing is pre-allocated.
pub fn new_shared_image() -> SharedImage {
    Arc::new(Mutex::new(DataImage::new(0, 0, 0, 0)))
}

/// 主站写入通知回调：`Fn(unit_id)`，在主站通过从站写入数据（FC05/06/0F/10）
/// 成功后触发，供应用层推送 `slave-values` 快照给 UI。
pub type OnWrite = Arc<dyn Fn(u8) + Send + Sync>;

/// Process a request PDU against the data image of `unit_id` and return the
/// response PDU (a Modbus exception PDU on error).
///
/// Returns `None` when the Unit ID is **not configured** — the caller decides
/// what to do: RTU stays silent (realistic multi-drop behavior), TCP answers
/// a gateway exception.
///
/// When the request is a *write* (FC05/06/0F/10) and it succeeds, `on_write`
/// is invoked with the unit id so the app layer can push a live snapshot to
/// the UI, keeping the simulator page in sync with master writes.
fn respond_for(
    units: &Arc<Mutex<UnitImages>>,
    unit_id: u8,
    req_pdu: &[u8],
    on_write: &dyn Fn(u8),
) -> Option<Vec<u8>> {
    // 1. 局部锁定 units 获取 SharedImage 的 Arc 克隆，然后立即释放 units 锁
    let img = {
        let map = units.lock().unwrap();
        map.get(&unit_id).cloned()
    };
    let img = img?;

    // 2. 局部锁定 img 执行请求处理，然后立即释放 img 锁
    let is_write = matches!(req_pdu.first(), Some(0x05) | Some(0x06) | Some(0x0F) | Some(0x10));
    let res = {
        let mut guard = img.lock().unwrap();
        guard.handle_request(req_pdu)
    };

    // 3. 此时所有锁均已释放，可以安全执行 on_write 回调，彻底避免重入死锁
    match res {
        Ok(resp) => {
            if is_write {
                on_write(unit_id);
            }
            Some(resp)
        }
        Err(exc) => {
            let fc = req_pdu.first().copied().unwrap_or(0);
            Some(vec![fc | 0x80, exc])
        }
    }
}

// ══════════════════════════════════════════════════════════════════
// Modbus TCP slave
// ══════════════════════════════════════════════════════════════════

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::broadcast;

/// Run a TCP slave bound to `bind:port`, accepting concurrent connections
/// until `shutdown` fires. Bind `0.0.0.0` to listen on all interfaces.
/// Responds per configured Unit ID; unknown units get a gateway exception.
/// `on_write` is invoked after any successful master write to a configured
/// unit (so the app can sync the UI).
pub async fn run_tcp_slave(
    units: Arc<Mutex<UnitImages>>,
    on_write: OnWrite,
    bind: &str,
    port: u16,
    shutdown: broadcast::Receiver<()>,
    conns: Arc<AtomicUsize>,
) -> Result<(), ModbusError> {
    let listener = TcpListener::bind((bind, port))
        .await
        .map_err(|e| ModbusError::Other(format!("bind {bind}:{port}: {e}")))?;
    run_tcp_slave_on(listener, units, on_write, shutdown, conns).await
}

/// Like [`run_tcp_slave`] but the caller supplies the already-bound listener
/// (used in tests to learn the ephemeral port).
pub async fn run_tcp_slave_on(
    listener: TcpListener,
    units: Arc<Mutex<UnitImages>>,
    on_write: OnWrite,
    mut shutdown: broadcast::Receiver<()>,
    conns: Arc<AtomicUsize>,
) -> Result<(), ModbusError> {
    loop {
        tokio::select! {
            // A shutdown broadcast ends the whole server.
            _ = shutdown.recv() => break,
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let units = units.clone();
                        let on_write = on_write.clone();
                        let conns = conns.clone();
                        let mut sd = shutdown.resubscribe();
                        tokio::spawn(async move {
                            conns.fetch_add(1, AtomicOrdering::SeqCst);
                            handle_tcp_conn(units, on_write, stream, &mut sd).await;
                            conns.fetch_sub(1, AtomicOrdering::SeqCst);
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    }
    Ok(())
}

async fn handle_tcp_conn(
    units: Arc<Mutex<UnitImages>>,
    on_write: OnWrite,
    mut stream: TcpStream,
    shutdown: &mut broadcast::Receiver<()>,
) {
    let mut header = [0u8; 7];
    loop {
        // Read the 7-byte MBAP header, or bail on shutdown / disconnect.
        tokio::select! {
            _ = shutdown.recv() => return,
            r = stream.read_exact(&mut header) => {
                if r.is_err() { return; }
            }
        }
        // MBAP length = unit_id(1) + PDU. The 7-byte header already includes
        // unit_id, so the remaining body is len - 1 bytes.
        let len = u16::from_be_bytes([header[4], header[5]]) as usize;
        if len < 1 {
            continue; // malformed; wait for next frame
        }
        let mut rest = vec![0u8; len - 1];
        tokio::select! {
            _ = shutdown.recv() => return,
            r = stream.read_exact(&mut rest) => {
                if r.is_err() { return; }
            }
        }

        let unit_id = header[6];
        let tid = [header[0], header[1]];
        // PDU is the bytes after the 7-byte header.
        let req_pdu = &rest[..];
        // 已配置的 Unit ID → 用该从站自己的数据镜像响应；
        // 未配置的 Unit ID → 网关异常 0x0B（目标设备无法响应）。
        let resp_pdu = match respond_for(&units, unit_id, req_pdu, &*on_write) {
            Some(r) => r,
            None => {
                let fc = req_pdu.first().copied().unwrap_or(0);
                vec![fc | 0x80, exception::GATEWAY_TARGET_FAILED]
            }
        };
        let adu = framing::encode_tcp(unit_id, u16::from_be_bytes(tid), &resp_pdu);

        tokio::select! {
            _ = shutdown.recv() => return,
            r = stream.write_all(&adu) => {
                if r.is_err() { return; }
            }
        }
        let _ = stream.flush().await;
    }
}

// ══════════════════════════════════════════════════════════════════
// Modbus RTU (serial) slave  — feature `rtu`
// ══════════════════════════════════════════════════════════════════

#[cfg(feature = "rtu")]
mod rtu_slave {
    use super::*;
    use serial2::{CharSize, Parity, SerialPort, StopBits};
    use std::sync::atomic::AtomicBool;

    /// Run a Modbus RTU slave on the given serial port until `stop` is set.
    /// This blocks the calling thread (spawn it on a dedicated OS thread).
    /// It answers the **configured Unit IDs**, each with its own data image
    /// (multi-drop simulation). Requests to unconfigured units stay silent -
    /// exactly like a real serial bus where the slave is absent.
    /// `on_write` is invoked after any successful master write.
    /// Run a Modbus RTU slave on the given serial port until `stop` is set.
    /// This blocks the calling thread (spawn it on a dedicated OS thread).
    /// It answers the **configured Unit IDs**, each with its own data image
    /// (multi-drop simulation). Requests to unconfigured units stay silent -
    /// exactly like a real serial bus where the slave is absent.
    /// `on_write` is invoked after any successful master write.
    ///
    /// Open a serial port for RTU slave use: raw mode (cfmakeraw, i.e. no
    /// echo / canonical mode / output processing - OPOST·ONLCR) plus the
    /// caller's baud / char-size / stop-bits / parity. Returns the configured
    /// port. Errors propagate so the caller can surface them instead of
    /// failing silently (which would make the slave look "dead" in the UI).
    pub fn open_rtu_port(
        port_name: &str,
        baud: u32,
        data_bits: u8,
        stop_bits: u8,
        parity: &str,
    ) -> Result<SerialPort, ModbusError> {
        SerialPort::open(port_name, |mut s: serial2::Settings| {
            // 关键：串口必须设为 raw 模式（cfmakeraw），关闭 echo / 规范模式 /
            // 信号与输出处理（OPOST/ONLCR）。否则二进制 Modbus 帧会被终端行规
            // 篡改（ONLCR 把 0x0A→0x0D 0x0A、控制字符被解释），主站收到坏帧而
            // 拒绝应答——典型表现就是「模拟器串口从站不应答主站」。
            s.set_raw();
            s.set_baud_rate(baud)?;
            s.set_char_size(match data_bits {
                5 => CharSize::Bits5,
                6 => CharSize::Bits6,
                7 => CharSize::Bits7,
                _ => CharSize::Bits8,
            });
            s.set_stop_bits(match stop_bits {
                2 => StopBits::Two,
                _ => StopBits::One,
            });
            s.set_parity(match parity.to_ascii_lowercase().as_str() {
                "odd"  => Parity::Odd,
                "even" => Parity::Even,
                _      => Parity::None,
            });
            Ok(s)
        })
        .map_err(|e| ModbusError::Other(format!("open {port_name}: {e}")))
    }

    /// Run a Modbus RTU slave on the given serial port until `stop` is set.
    /// This blocks the calling thread (spawn it on a dedicated OS thread).
    /// It answers the **configured Unit IDs**, each with its own data image
    /// (multi-drop simulation). Requests to unconfigured units stay silent -
    /// exactly like a real serial bus where the slave is absent.
    /// `on_write` is invoked after any successful master write.
    ///
    /// Opens the port by name (the production path used by the simulator).
    /// For an already-open port (e.g. a `SerialPort::pair()` in tests) use
    /// [`run_rtu_slave_on_port`].
    pub fn run_rtu_slave_blocking(
        units: Arc<Mutex<UnitImages>>,
        on_write: OnWrite,
        port_name: &str,
        baud: u32,
        data_bits: u8,
        stop_bits: u8,
        parity: &str,
        inter_frame_ms: u64,
        stop: Arc<AtomicBool>,
    ) -> Result<(), ModbusError> {
        let port = open_rtu_port(port_name, baud, data_bits, stop_bits, parity)?;
        run_rtu_slave_on_port(port, units, on_write, inter_frame_ms, stop)
    }

    /// Like [`run_rtu_slave_blocking`] but operates on an **already-open**
    /// [`SerialPort`]. The caller configures the port (baud/raw mode etc.);
    /// this function sets the read/write timeouts and runs the RTU
    /// request/response loop. Exposed so tests can drive the real slave loop
    /// over a real serial port (e.g. a `SerialPort::pair()` on Linux).
    pub fn run_rtu_slave_on_port(
        mut port: SerialPort,
        units: Arc<Mutex<UnitImages>>,
        on_write: OnWrite,
        inter_frame_ms: u64,
        stop: Arc<AtomicBool>,
    ) -> Result<(), ModbusError> {
        // Short poll timeout: let read() return quickly so Instant-based
        // inter-frame timing is accurate regardless of baud rate.
        port.set_read_timeout(Duration::from_millis(5))
            .map_err(|e| ModbusError::Other(e.to_string()))?;
        port.set_write_timeout(Duration::from_millis(200))
            .map_err(|e| ModbusError::Other(e.to_string()))?;
        run_rtu_slave_on_rw(port, units, on_write, inter_frame_ms, stop)
    }

    /// Generic RTU slave loop over **any** `Read + Write` byte stream (a real
    /// `SerialPort`, a `UnixStream`, an in-memory pipe, …). The caller is
    /// responsible for the stream's read semantics: the loop expects `read`
    /// to return `WouldBlock`/`TimedOut` (or otherwise fail fast) when no
    /// data is available, so it can detect the 3.5-char inter-frame gap.
    ///
    /// This is the production request/response core; `run_rtu_slave_on_port`
    /// and `run_rtu_slave_blocking` only configure a real serial port and
    /// delegate here. Kept generic so it can be exercised end-to-end in tests
    /// without a real tty — `serial2`'s macOS `set_configuration` issues the
    /// `IOSSIOSPEED` ioctl that pseudo-terminals reject with ENOTTY, so a pty
    /// cannot be opened through `serial2` on macOS at all.
    pub fn run_rtu_slave_on_rw<RW: std::io::Read + std::io::Write>(
        mut port: RW,
        units: Arc<Mutex<UnitImages>>,
        on_write: OnWrite,
        inter_frame_ms: u64,
        stop: Arc<AtomicBool>,
    ) -> Result<(), ModbusError> {
        // RTU inter-frame gap: at least 3.5 char times.
        // At 9600 baud, one char = ~1.04 ms, so 3.5 chars ~ 3.6 ms. Default
        // to 5 ms which is safe for all standard baud rates up to 115200.
        let inter_frame = Duration::from_millis(inter_frame_ms.max(2));

        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        // Timestamp of the last received byte; None = idle bus.
        let mut last_byte_at: Option<Instant> = None;

        loop {
            if stop.load(AtomicOrdering::SeqCst) {
                break;
            }

            match port.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    last_byte_at = Some(Instant::now());
                }
                // WouldBlock / TimedOut / EOF - no new data. Check inter-frame gap.
                _ => {
                    if let Some(t) = last_byte_at {
                        if t.elapsed() >= inter_frame && !buf.is_empty() {
                            last_byte_at = None;

                            // Minimum valid RTU frame: addr(1) + FC(1) + data(>=1) + CRC(2) = 5
                            if buf.len() >= 5 && framing::verify_crc(&buf) {
                                let unit_id = buf[0];
                                let req_pdu = buf[1..buf.len() - 2].to_vec();
                                // Only respond to configured Unit IDs; others stay silent.
                                if let Some(resp_pdu) = respond_for(&units, unit_id, &req_pdu, &*on_write) {
                                    let mut resp = Vec::with_capacity(1 + resp_pdu.len() + 2);
                                    resp.push(unit_id);
                                    resp.extend_from_slice(&resp_pdu);
                                    framing::append_crc(&mut resp);
                                    let _ = port.write_all(&resp);
                                    let _ = port.flush();
                                }
                            }
                            buf.clear();
                        }
                        // else: inter-frame gap not yet elapsed, keep waiting
                    }
                    // else: idle bus, nothing to do
                }
            }
        }
        Ok(())
    }
}

#[cfg(feature = "rtu")]
pub use rtu_slave::{open_rtu_port, run_rtu_slave_blocking, run_rtu_slave_on_port, run_rtu_slave_on_rw};

// ══════════════════════════════════════════════════════════════════
// Modbus UDP slave (MBAP framing over UDP datagrams)
// ══════════════════════════════════════════════════════════════════

/// Run a Modbus UDP slave bound to `bind:port`. Each datagram is a full MBAP
/// (Modbus/TCP-style) frame, so we reuse the TCP request handler semantics:
/// configured Unit IDs are answered with their own data image, and unknown
/// units get a gateway exception (UDP has no "silent" notion like serial).
/// One response datagram is sent back to the request's source address.
pub async fn run_udp_slave(
    units: Arc<Mutex<UnitImages>>,
    on_write: OnWrite,
    bind: &str,
    port: u16,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), ModbusError> {
    let socket = UdpSocket::bind((bind, port))
        .await
        .map_err(|e| ModbusError::Other(format!("bind udp {bind}:{port}: {e}")))?;
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            res = socket.recv_from(&mut buf) => {
                match res {
                    Ok((n, from)) => {
                        let (tid, unit_id, pdu) = match framing::parse_tcp(&buf[..n]) {
                            Ok(v) => v,
                            Err(_) => continue, // 非法 MBAP 帧：忽略
                        };
                        let resp_pdu = match respond_for(&units, unit_id, &pdu, &*on_write) {
                            Some(r) => r,
                            None => {
                                let fc = pdu.first().copied().unwrap_or(0);
                                vec![fc | 0x80, exception::GATEWAY_TARGET_FAILED]
                            }
                        };
                        let adu = framing::encode_tcp(unit_id, tid, &resp_pdu);
                        let _ = socket.send_to(&adu, from).await;
                    }
                    Err(_) => break,
                }
            }
        }
    }
    Ok(())
}

// ══════════════════════════════════════════════════════════════════
// Modbus RTU over UDP slave (RTU frame per datagram, no MBAP header)
// ══════════════════════════════════════════════════════════════════

/// Run a Modbus RTU-over-UDP slave bound to `bind:port`. Each datagram carries
/// exactly one serial RTU frame (unit_id + PDU + CRC16) with no MBAP header.
/// Configured Unit IDs are answered; unconfigured units stay **silent** — the
/// same multi-drop behavior as the serial RTU slave.
pub async fn run_rtu_over_udp_slave(
    units: Arc<Mutex<UnitImages>>,
    on_write: OnWrite,
    bind: &str,
    port: u16,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), ModbusError> {
    let socket = UdpSocket::bind((bind, port))
        .await
        .map_err(|e| ModbusError::Other(format!("bind udp {bind}:{port}: {e}")))?;
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            res = socket.recv_from(&mut buf) => {
                match res {
                    Ok((n, from)) => {
                        let (unit_id, pdu) = match framing::parse_rtu(&buf[..n]) {
                            Ok(v) => v,
                            Err(_) => continue, // CRC 校验失败 / 非法帧：忽略
                        };
                        if let Some(resp_pdu) = respond_for(&units, unit_id, &pdu, &*on_write) {
                            let mut resp = Vec::with_capacity(1 + resp_pdu.len() + 2);
                            resp.push(unit_id);
                            resp.extend_from_slice(&resp_pdu);
                            framing::append_crc(&mut resp);
                            let _ = socket.send_to(&resp, from).await;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    Ok(())
}

// ══════════════════════════════════════════════════════════════════
// Modbus RTU over TCP slave (RTU frame over a TCP stream, no MBAP header)
// ══════════════════════════════════════════════════════════════════

/// Run a Modbus RTU-over-TCP slave bound to `bind:port`. Each master connection
/// is a TCP stream that carries serial-style RTU frames (unit_id + PDU + CRC16)
/// with no MBAP header. A single connection may serve many transactions; frames
/// are delimited by an inter-frame idle timeout (RTU framing has no length
/// prefix on TCP). Configured Unit IDs are answered with their own image;
/// unconfigured units stay **silent** (multi-drop behavior).
pub async fn run_rtu_over_tcp_slave(
    units: Arc<Mutex<UnitImages>>,
    on_write: OnWrite,
    bind: &str,
    port: u16,
    mut shutdown: broadcast::Receiver<()>,
    conns: Arc<AtomicUsize>,
) -> Result<(), ModbusError> {
    let listener = TcpListener::bind((bind, port))
        .await
        .map_err(|e| ModbusError::Other(format!("bind {bind}:{port}: {e}")))?;
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let units = units.clone();
                        let on_write = on_write.clone();
                        let conns = conns.clone();
                        let mut sd = shutdown.resubscribe();
                        tokio::spawn(async move {
                            conns.fetch_add(1, AtomicOrdering::SeqCst);
                            handle_rtu_over_tcp_conn(units, on_write, stream, &mut sd).await;
                            conns.fetch_sub(1, AtomicOrdering::SeqCst);
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    }
    Ok(())
}

/// Per-connection RTU-over-TCP frame loop. Bytes are accumulated until an
/// inter-frame idle gap, at which point the buffer is treated as one RTU frame.
/// The inter-frame timeout mirrors the serial `inter_frame_ms` semantics.
async fn handle_rtu_over_tcp_conn(
    units: Arc<Mutex<UnitImages>>,
    on_write: OnWrite,
    mut stream: TcpStream,
    shutdown: &mut broadcast::Receiver<()>,
) {
    // RTU over TCP 没有真实串口定时，用固定帧间空闲（20ms）作为帧边界。
    let inter_frame = Duration::from_millis(20);
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            r = tokio::time::timeout(inter_frame, stream.read(&mut byte)) => {
                match r {
                    Ok(Ok(1)) => {
                        buf.push(byte[0]);
                    }
                    Ok(Ok(_)) => return, // EOF
                    Ok(Err(_)) => return,
                    Err(_) => {
                        // 帧间超时 → 视为一帧结束，校验 CRC 并应答
                        if buf.len() >= 5 && framing::verify_crc(&buf) {
                            let unit_id = buf[0];
                            let pdu = buf[1..buf.len() - 2].to_vec();
                            if let Some(resp_pdu) = respond_for(&units, unit_id, &pdu, &*on_write) {
                                let mut resp = Vec::with_capacity(1 + resp_pdu.len() + 2);
                                resp.push(unit_id);
                                resp.extend_from_slice(&resp_pdu);
                                framing::append_crc(&mut resp);
                                let _ = stream.write_all(&resp).await;
                                let _ = stream.flush().await;
                            }
                        }
                        buf.clear();
                    }
                }
            }
        }
    }
}

// ══════════════════════════════════════════════════════════════════
// Snapshot used by the UI
// ══════════════════════════════════════════════════════════════════

/// Serializable snapshot of the shared image for the UI data tables.
/// Addresses are sparse: a map key that is absent simply means the address
/// was never written (reads it as implicit zero).
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct ImageSnapshot {
    pub coils: HashMap<u16, bool>,
    pub discrete_inputs: HashMap<u16, bool>,
    pub input_registers: HashMap<u16, u16>,
    pub holding_registers: HashMap<u16, u16>,
}

/// Take a snapshot of the current values in the shared image (for the UI).
pub fn snapshot(image: &SharedImage) -> ImageSnapshot {
    let img = image.lock().unwrap();
    snapshot_from(&img)
}

/// Build a snapshot from an already-locked image (avoids double-locking).
pub fn snapshot_from(img: &DataImage) -> ImageSnapshot {
    ImageSnapshot {
        coils: img.coils.clone(),
        discrete_inputs: img.discrete_inputs.clone(),
        input_registers: img.input_registers.clone(),
        holding_registers: img.holding_registers.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream as ClientStream;

    /// 构造单 Unit ID(1) 的单元表（测试默认）。
    fn units_single() -> Arc<Mutex<UnitImages>> {
        let mut m = UnitImages::new();
        m.insert(1, new_shared_image());
        Arc::new(Mutex::new(m))
    }

    /// 空写入回调（大多数测试不需要通知）。
    fn noop_write() -> OnWrite {
        Arc::new(|_: u8| {})
    }

    #[tokio::test]
    async fn tcp_slave_reads_and_writes_shared_image() {
        let units = units_single();
        let conns = Arc::new(AtomicUsize::new(0));
        let wrote = Arc::new(AtomicUsize::new(0));
        let on_write: OnWrite = {
            let w = wrote.clone();
            Arc::new(move |unit_id| {
                assert_eq!(unit_id, 1);
                w.fetch_add(1, AtomicOrdering::SeqCst);
            })
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = broadcast::channel(2);

        tokio::spawn(run_tcp_slave_on(
            listener,
            units.clone(),
            on_write,
            rx,
            conns.clone(),
        ));

        // ── FC03 read holding @0..2 (initially 0) ──
        let mut c = ClientStream::connect(addr).await.unwrap();
        let req = framing::encode_tcp(1, 7, &[0x03, 0x00, 0x00, 0x00, 0x02]);
        c.write_all(&req).await.unwrap();
        let mut hdr = [0u8; 7];
        c.read_exact(&mut hdr).await.unwrap();
        let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len - 1];
        c.read_exact(&mut body).await.unwrap();
        assert_eq!(body[0], 0x03);
        assert_eq!(body[1], 4); // 2 regs
        assert_eq!(&body[2..], &[0xFF, 0xFF, 0xFF, 0xFF]);

        // ── FC06 write holding @1 = 0xABCD, then read it back ──
        let w = framing::encode_tcp(1, 8, &[0x06, 0x00, 0x01, 0xAB, 0xCD]);
        c.write_all(&w).await.unwrap();
        let mut hdr = [0u8; 7];
        c.read_exact(&mut hdr).await.unwrap();
        let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len - 1];
        c.read_exact(&mut body).await.unwrap();
        assert_eq!(body, vec![0x06, 0x00, 0x01, 0xAB, 0xCD]);
        // 主站写入后 on_write 应被触发一次（读取不触发）
        assert_eq!(wrote.load(AtomicOrdering::SeqCst), 1);

        let r = framing::encode_tcp(1, 9, &[0x03, 0x00, 0x01, 0x00, 0x01]);
        c.write_all(&r).await.unwrap();
        let mut hdr = [0u8; 7];
        c.read_exact(&mut hdr).await.unwrap();
        let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len - 1];
        c.read_exact(&mut body).await.unwrap();
        assert_eq!(&body[2..], &[0xAB, 0xCD]);

        // connection counter reflects the open client
        assert_eq!(conns.load(AtomicOrdering::SeqCst), 1);

        drop(c);
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn tcp_slave_multiple_units_each_own_image() {
        // 两个 Unit：1 和 42，各自独立的数据镜像
        let mut m = UnitImages::new();
        let img1 = new_shared_image();
        m.insert(1, img1.clone());
        m.insert(42, new_shared_image());
        let units = Arc::new(Mutex::new(m));

        // 往 Unit 1 写入一个值
        img1.lock().unwrap().set_holding(0, 0x1234);

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_tx, rx) = broadcast::channel(2);
        tokio::spawn(run_tcp_slave_on(listener, units, noop_write(), rx, Arc::new(AtomicUsize::new(0))));

        // Unit 42 可被应答（echo），但读不到 Unit 1 的数据（独立空间）
        let mut c = ClientStream::connect(addr).await.unwrap();
        let req = framing::encode_tcp(42, 1, &[0x03, 0x00, 0x00, 0x00, 0x01]);
        c.write_all(&req).await.unwrap();
        let mut hdr = [0u8; 7];
        c.read_exact(&mut hdr).await.unwrap();
        assert_eq!(hdr[6], 42); // echoed unit id
        let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len - 1];
        c.read_exact(&mut body).await.unwrap();
        assert_eq!(body[0], 0x03);
        assert_eq!(&body[2..], &[0xFF, 0xFF]); // Unit 42 独立空间，读到默认 0xFFFF

        // Unit 1 读同一地址 → 0x1234
        let req = framing::encode_tcp(1, 2, &[0x03, 0x00, 0x00, 0x00, 0x01]);
        c.write_all(&req).await.unwrap();
        let mut hdr = [0u8; 7];
        c.read_exact(&mut hdr).await.unwrap();
        let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len - 1];
        c.read_exact(&mut body).await.unwrap();
        assert_eq!(&body[2..], &[0x12, 0x34]);
    }

    #[tokio::test]
    async fn tcp_slave_unknown_unit_gets_gateway_exception() {
        // 仅配置 Unit 1；对 Unit 99 的请求 → 网关异常 0x0B
        let units = units_single();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_tx, rx) = broadcast::channel(2);
        tokio::spawn(run_tcp_slave_on(listener, units, noop_write(), rx, Arc::new(AtomicUsize::new(0))));

        let mut c = ClientStream::connect(addr).await.unwrap();
        let req = framing::encode_tcp(99, 1, &[0x03, 0x00, 0x00, 0x00, 0x01]);
        c.write_all(&req).await.unwrap();
        let mut hdr = [0u8; 7];
        c.read_exact(&mut hdr).await.unwrap();
        let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len - 1];
        c.read_exact(&mut body).await.unwrap();
        assert_eq!(body[0], 0x83); // FC03 | 0x80
        assert_eq!(body[1], 0x0B); // gateway target failed (unit not configured)
    }

    #[tokio::test]
    async fn udp_slave_responds_per_unit() {
        // UDP 从站：MBAP 帧承载在 UDP 数据报上，未知 Unit → 网关异常。
        let units = units_single();
        // 先占用一个空闲端口，交给从站绑定（测试环境端口冲突概率极低）。
        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_port = probe.local_addr().unwrap().port();
        drop(probe);
        let (_tx, rx) = broadcast::channel(2);
        tokio::spawn(run_udp_slave(units, noop_write(), "127.0.0.1", srv_port, rx));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let cli = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // 读 holding @0（隐式 0）
        let req = framing::encode_tcp(1, 5, &[0x03, 0x00, 0x00, 0x00, 0x01]);
        cli.send_to(&req, ("127.0.0.1", srv_port)).await.unwrap();
        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), cli.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let (tid, unit, pdu) = framing::parse_tcp(&buf[..n]).unwrap();
        assert_eq!(tid, 5);
        assert_eq!(unit, 1);
        assert_eq!(pdu[0], 0x03);
        assert_eq!(&pdu[2..], &[0xFF, 0xFF]);

        // 未配置 Unit 99 → 网关异常 0x0B
        let req = framing::encode_tcp(99, 6, &[0x03, 0x00, 0x00, 0x00, 0x01]);
        cli.send_to(&req, ("127.0.0.1", srv_port)).await.unwrap();
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), cli.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let (_, unit, pdu) = framing::parse_tcp(&buf[..n]).unwrap();
        assert_eq!(unit, 99);
        assert_eq!(pdu[0], 0x83);
        assert_eq!(pdu[1], 0x0B);
    }

    #[tokio::test]
    async fn tcp_slave_implicit_zero_for_unregistered() {
        let units = units_single();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_tx, rx) = broadcast::channel(2);
        tokio::spawn(run_tcp_slave_on(listener, units, noop_write(), rx, Arc::new(AtomicUsize::new(0))));

        let mut c = ClientStream::connect(addr).await.unwrap();
        // Read holding @9999 (never written) → implicit zero, not an exception.
        let req = framing::encode_tcp(1, 1, &[0x03, 0x27, 0x0F, 0x00, 0x01]);
        c.write_all(&req).await.unwrap();
        let mut hdr = [0u8; 7];
        c.read_exact(&mut hdr).await.unwrap();
        let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len - 1];
        c.read_exact(&mut body).await.unwrap();
        assert_eq!(body[0], 0x03);
        assert_eq!(body[1], 2);
        assert_eq!(&body[2..], &[0xFF, 0xFF]);

        // A range that crosses the 16-bit address space → illegal data address.
        let req = framing::encode_tcp(1, 2, &[0x03, 0xFF, 0xFF, 0x00, 0x02]);
        c.write_all(&req).await.unwrap();
        let mut hdr = [0u8; 7];
        c.read_exact(&mut hdr).await.unwrap();
        let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
        let mut body = vec![0u8; len - 1];
        c.read_exact(&mut body).await.unwrap();
        assert_eq!(body[0], 0x83); // FC03 | 0x80
        assert_eq!(body[1], 0x02); // illegal data address
    }
}
