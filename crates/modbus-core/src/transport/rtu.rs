//! Modbus RTU serial transport (feature `rtu`).
//!
//! Handles the 3.5-character inter-frame silence that defines RTU framing and
//! validates the trailing CRC16. Pair a macOS master with a slave using a
//! virtual serial pair (e.g. `socat PTY,raw,echo=0 PTY,raw,echo=0`).

use crate::error::ModbusError;
use crate::framing;
use crate::Frame;
use serial2::{CharSize, Parity, SerialPort, StopBits};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

pub struct RtuTransport {
    port: SerialPort,
    unit_id: u8,
    inter_frame: Duration,
    timeout: Duration,
}

impl RtuTransport {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        port_name: &str,
        baud_rate: u32,
        data_bits: u8,
        stop_bits: u8,
        parity: &str,
        unit_id: u8,
        inter_frame_ms: u64,
        timeout_ms: u64,
    ) -> Result<Self, ModbusError> {
        let mut port = SerialPort::open(port_name, |mut settings: serial2::Settings| {
            // 关键：串口必须设为 raw 模式（cfmakeraw），关闭 echo / 规范模式 /
            // 信号与输出处理（OPOST/ONLCR），否则二进制 Modbus 帧会被终端行规
            // 篡改，主站收发的帧损坏。set_raw 会重置字符/停止/校验为 8/1/None，
            // 故需在其后重新设置用户的参数。
            settings.set_raw();
            settings.set_baud_rate(baud_rate)?;
            settings.set_char_size(match data_bits {
                5 => CharSize::Bits5,
                6 => CharSize::Bits6,
                7 => CharSize::Bits7,
                _ => CharSize::Bits8,
            });
            settings.set_stop_bits(match stop_bits {
                2 => StopBits::Two,
                _ => StopBits::One,
            });
            settings.set_parity(match parity.to_ascii_lowercase().as_str() {
                "odd" => Parity::Odd,
                "even" => Parity::Even,
                _ => Parity::None,
            });
            Ok(settings)
        })
        .map_err(|e| ModbusError::Other(format!("open {port_name}: {e}")))?;

        let timeout = Duration::from_millis(timeout_ms);
        // Read timeout is a small "frame-gap probe" (must exceed the 3.5-char
        // inter-frame silence) so the RX loop exits ~immediately after the
        // last byte instead of blocking for the full overall timeout — this
        // both keeps poll latency low and makes the measured RTT accurate.
        let read_probe = Duration::from_millis(timeout_ms.min(20));
        port.set_read_timeout(read_probe)
            .map_err(|e| ModbusError::Other(e.to_string()))?;
        port.set_write_timeout(timeout)
            .map_err(|e| ModbusError::Other(e.to_string()))?;

        Ok(Self {
            port,
            unit_id,
            inter_frame: Duration::from_millis(inter_frame_ms),
            timeout,
        })
    }

    /// Send a PDU and read back the RTU response frame (CRC validated).
    /// Uses the transport's configured unit_id.
    pub fn request(&mut self, pdu: &[u8]) -> Result<Vec<u8>, ModbusError> {
        let (_tx, _rx_adu, _rtt, resp_pdu) = self.request_frame(pdu)?;
        Ok(resp_pdu)
    }

    /// Like [`request`] but also returns the TX ADU (unit_id + PDU + CRC),
    /// the RX ADU (unit_id + response PDU + CRC) and the measured RTT.
    /// The transport's `unit_id` is stamped into both ADUs.
    ///
    /// RTT is measured from the moment the TX frame is written until the
    /// moment the *last* response byte arrives — the idle `read()` wait at
    /// the end of the frame (up to `inter_frame`) is *not* counted, so the
    /// reported value matches the real on-wire round trip (typically a few ms).
    pub fn request_frame(&mut self, pdu: &[u8]) -> Result<Frame, ModbusError> {
        self.request_frame_for(self.unit_id, pdu)
    }

    /// Like [`request_frame`] but the slave address is taken from the caller
    /// (`unit_id`), so multiple polls targeting different stations can share
    /// one serial connection. The transport's own `unit_id` is untouched.
    pub fn request_frame_for(&mut self, unit_id: u8, pdu: &[u8]) -> Result<Frame, ModbusError> {
        exchange_rtu(
            &mut self.port,
            unit_id,
            pdu,
            self.inter_frame,
            self.timeout,
        )
    }

    /// Configured Modbus unit (slave) ID stamped into outbound ADUs.
    pub fn unit_id(&self) -> u8 {
        self.unit_id
    }
}

/// Performs one RTU request/response exchange over **any** `Read + Write`
/// stream. Extracted from [`RtuTransport::request_frame_for`] so the exact
/// framing/timing logic can be unit-tested over a `UnixStream` (no real serial
/// port needed) and reused by other transports.
///
/// Timing contract (the part that previously broke):
/// - While **no** response byte has arrived yet, the loop waits the full
///   `timeout` for the first byte. The inter-frame gap is irrelevant before a
///   frame has started, so it must NOT be allowed to end the read early.
/// - Once bytes are arriving, the frame ends on a 3.5-char (`inter_frame`)
///   silence; the overall `timeout` is still a hard cap in case the slave
///   stalls mid-frame.
pub fn exchange_rtu<RW: Read + Write>(
    rw: &mut RW,
    unit_id: u8,
    pdu: &[u8],
    inter_frame: Duration,
    timeout: Duration,
) -> Result<Frame, ModbusError> {
    let tx_adu = framing::encode_rtu(unit_id, pdu);

    // t0 before writing: RTT covers the full TX → RX round trip.
    let t0 = Instant::now();
    rw.write_all(&tx_adu)
        .map_err(|e| ModbusError::Other(e.to_string()))?;
    rw.flush().map_err(|e| ModbusError::Other(e.to_string()))?;

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    // `last` is set when the **first** response byte arrives; until then the
    // loop waits the full `timeout` for any byte.
    let mut last: Option<Instant> = None;
    loop {
        match rw.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                last = Some(Instant::now());
            }
            _ => match last {
                // No byte received yet: keep waiting up to the full timeout.
                None => {
                    if t0.elapsed() >= timeout {
                        break;
                    }
                }
                // Frame in progress: end on inter-frame silence, capped by timeout.
                Some(l) => {
                    if l.elapsed() >= inter_frame {
                        break;
                    }
                    if t0.elapsed() >= timeout {
                        break;
                    }
                }
            },
        }
    }

    // RTT = arrival time of the last response byte − TX write start (the final
    // idle wait is excluded, so the value matches the real on-wire round trip).
    let rtt = last.map_or(Duration::ZERO, |l| l.saturating_duration_since(t0));
    if buf.is_empty() {
        return Err(ModbusError::Other("no response (timeout)".into()));
    }
    let (unit, resp_pdu) = framing::parse_rtu(&buf)?;
    // Re-assemble the RX ADU so the UI can show the full on-wire frame.
    let rx_adu = framing::encode_rtu(unit, &resp_pdu);
    if resp_pdu.len() >= 2 && (resp_pdu[0] & 0x80) != 0 {
        return Err(ModbusError::Exception(resp_pdu[1]));
    }
    Ok((tx_adu, rx_adu, rtt, resp_pdu))
}
