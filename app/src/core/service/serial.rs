//! Pure serial-port debugging service (independent of Modbus RTU framing).
//!
//! Opens a serial port in raw mode with a cloned writer handle and a blocking
//! read thread that pushes received bytes as `Event::SerialRx`. TX/RX byte
//! counters are tracked atomically. Serial ports are mutually exclusive with
//! the Modbus master/RTU connection — only one may own a given device at once.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serial2::{CharSize, FlowControl, Parity, SerialPort, StopBits};

use super::{Event, Service, ORDERING};

/// Serial connection parameters.
#[derive(Clone, Debug)]
pub struct SerialConfig {
    pub port: String,
    pub baud: u32,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: String, // "none" | "odd" | "even"
    pub flow: String,   // "none" | "rtscts" | "xonxoff"
    pub dtr: bool,
    pub rts: bool,
    pub read_timeout_ms: u64,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            port: String::new(),
            baud: 115200,
            data_bits: 8,
            stop_bits: 1,
            parity: "none".into(),
            flow: "none".into(),
            dtr: true,
            rts: false,
            read_timeout_ms: 50,
        }
    }
}

pub struct SerialSession {
    writer: SerialPort,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    tx_bytes: Arc<AtomicU64>,
    rx_bytes: Arc<AtomicU64>,
}

/// Snapshot of the serial terminal status for the UI.
#[derive(Clone, Debug)]
pub struct SerialStatus {
    pub open: bool,
    pub port: String,
    pub baud: u32,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Service {
    /// Open the serial debugging port. Fails if the port is already owned by
    /// a Modbus connection or another serial session.
    pub async fn serial_open(&self, cfg: SerialConfig) -> Result<(), String> {
        if self.serial.lock().unwrap().is_some() {
            return Err("串口调试已连接，请先断开".into());
        }
        // Serial ports are mutually exclusive with the Modbus master connection.
        if self.conn_type.load(ORDERING) != super::CONN_NONE {
            return Err("Modbus 主站已连接串口/网络，请先断开 Modbus 连接".into());
        }

        let parity = match cfg.parity.to_ascii_lowercase().as_str() {
            "odd" => Parity::Odd,
            "even" => Parity::Even,
            _ => Parity::None,
        };
        let char_size = match cfg.data_bits {
            5 => CharSize::Bits5,
            6 => CharSize::Bits6,
            7 => CharSize::Bits7,
            _ => CharSize::Bits8,
        };
        let stop_bits = if cfg.stop_bits == 2 { StopBits::Two } else { StopBits::One };
        let flow = match cfg.flow.to_ascii_lowercase().as_str() {
            "rtscts" => FlowControl::RtsCts,
            "xonxoff" => FlowControl::XonXoff,
            _ => FlowControl::None,
        };
        let (dtr, rts) = (cfg.dtr, cfg.rts);

        let mut port = SerialPort::open(&cfg.port, move |mut settings: serial2::Settings| {
            settings.set_raw();
            settings.set_baud_rate(cfg.baud)?;
            settings.set_char_size(char_size);
            settings.set_stop_bits(stop_bits);
            settings.set_parity(parity);
            settings.set_flow_control(flow);
            Ok(settings)
        })
        .map_err(|e| super::friendly_serial_error(format!("打开串口失败 {}: {e}", cfg.port)))?;

        port.set_read_timeout(Duration::from_millis(cfg.read_timeout_ms.max(10)))
            .map_err(|e| e.to_string())?;
        port.set_write_timeout(Duration::from_millis(1000))
            .map_err(|e| e.to_string())?;
        // Best-effort DTR/RTS (some platforms/devices reject these).
        let _ = port.set_dtr(dtr);
        let _ = port.set_rts(rts);

        let writer = port.try_clone().map_err(|e| e.to_string())?;

        // Read thread: push received bytes as events until stopped.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let tx_bytes = Arc::new(AtomicU64::new(0));
        let rx_bytes = Arc::new(AtomicU64::new(0));
        let rx_t = rx_bytes.clone();
        let events = self.events.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                if stop_t.load(ORDERING) {
                    break;
                }
                match port.read(&mut buf) {
                    Ok(0) => continue,
                    Ok(n) => {
                        rx_t.fetch_add(n as u64, ORDERING);
                        let _ = events.send(Event::SerialRx {
                            bytes: buf[..n].to_vec(),
                            ts_ms: now_ms(),
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                    Err(_) => {
                        let _ = events.send(Event::SerialState {
                            open: false,
                            error: Some("串口读取失败，连接已断开".into()),
                        });
                        break;
                    }
                }
            }
        });

        *self.serial_meta.lock().unwrap() = (cfg.port.clone(), cfg.baud);
        *self.serial.lock().unwrap() = Some(SerialSession {
            writer,
            stop,
            reader: Some(reader),
            tx_bytes,
            rx_bytes,
        });
        self.emit(Event::SerialState { open: true, error: None });
        Ok(())
    }

    /// Close the serial debugging port.
    pub async fn serial_close(&self) {
        let session = self.serial.lock().unwrap().take();
        if let Some(mut s) = session {
            s.stop.store(true, ORDERING);
            if let Some(h) = s.reader.take() {
                let _ = h.join();
            }
            drop(s);
            self.emit(Event::SerialState { open: false, error: None });
        }
    }

    /// Send raw bytes over the serial port (UI builds the exact bytes incl.
    /// line ending / HEX decoding).
    pub async fn serial_send(&self, bytes: Vec<u8>) -> Result<(), String> {
        let mut guard = self.serial.lock().unwrap();
        let s = guard.as_mut().ok_or("串口未连接")?;
        s.writer
            .write_all(&bytes)
            .map_err(|e| format!("发送失败: {e}"))?;
        s.tx_bytes.fetch_add(bytes.len() as u64, ORDERING);
        Ok(())
    }

    /// Current serial status (open, port, counters).
    pub fn serial_status(&self) -> SerialStatus {
        let guard = self.serial.lock().unwrap();
        match guard.as_ref() {
            Some(s) => {
                let rm = self.serial_meta.lock().unwrap();
                SerialStatus {
                    open: true,
                    port: rm.0.clone(),
                    baud: rm.1,
                    tx_bytes: s.tx_bytes.load(ORDERING),
                    rx_bytes: s.rx_bytes.load(ORDERING),
                }
            }
            None => SerialStatus {
                open: false,
                port: String::new(),
                baud: 0,
                tx_bytes: 0,
                rx_bytes: 0,
            },
        }
    }
}
