//! Async Modbus master (client). Wraps a [`TcpTransport`] with retries and
//! PDU helpers, exposing the API the Tauri command layer consumes. The RTU
//! transport can be added behind the `rtu` feature using the same shape.

use crate::error::ModbusError;
use crate::transport::tcp::{TcpMode, TcpTransport};
use crate::Frame;
use std::time::Duration;

/// A connected master over a TCP transport.
pub struct ModbusClient {
    transport: TcpTransport,
    unit_id: u8,
    retries: u32,
}

impl ModbusClient {
    /// Open a TCP connection to a Modbus slave.
    pub async fn connect_tcp(
        host: &str,
        port: u16,
        unit_id: u8,
        timeout: Duration,
        retries: u32,
    ) -> Result<Self, ModbusError> {
        let transport = TcpTransport::connect(host, port, timeout).await?;
        Ok(Self {
            transport,
            unit_id,
            retries,
        })
    }

    /// Open a TCP connection carrying RTU frames (no MBAP header).
    /// `inter_frame` is the 3.5-char inter-frame gap used to delimit frames.
    pub async fn connect_rtu_over_tcp(
        host: &str,
        port: u16,
        unit_id: u8,
        timeout: Duration,
        retries: u32,
        inter_frame: Duration,
    ) -> Result<Self, ModbusError> {
        let transport =
            TcpTransport::connect_with_mode(host, port, timeout, TcpMode::Rtu, inter_frame)
                .await?;
        Ok(Self {
            transport,
            unit_id,
            retries,
        })
    }

    async fn do_request(&mut self, pdu: &[u8]) -> Result<Vec<u8>, ModbusError> {
        let mut last = None;
        for _ in 0..=self.retries.max(1) {
            match self.transport.request(self.unit_id, pdu).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    last = Some(e);
                    if matches!(last, Some(ModbusError::Exception(_))) {
                        break;
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| ModbusError::Other("no attempt".into())))
    }

    /// Read holding registers (FC03). Returns raw register values.
    pub async fn read_holding_registers(
        &mut self,
        addr: u16,
        count: u16,
    ) -> Result<Vec<u16>, ModbusError> {
        let pdu = [
            0x03u8,
            (addr >> 8) as u8,
            addr as u8,
            (count >> 8) as u8,
            count as u8,
        ];
        let resp = self.do_request(&pdu).await?;
        if resp.len() < 2 || resp[0] != 0x03 {
            return Err(ModbusError::Other("unexpected read response".into()));
        }
        let byte_count = resp[1] as usize;
        if resp.len() < 2 + byte_count {
            return Err(ModbusError::Other("read response truncated".into()));
        }
        let mut data = Vec::with_capacity(byte_count / 2);
        for i in (0..byte_count).step_by(2) {
            data.push(u16::from_be_bytes([resp[2 + i], resp[3 + i]]));
        }
        Ok(data)
    }

    /// Read input registers (FC04).
    pub async fn read_input_registers(
        &mut self,
        addr: u16,
        count: u16,
    ) -> Result<Vec<u16>, ModbusError> {
        let pdu = [
            0x04u8,
            (addr >> 8) as u8,
            addr as u8,
            (count >> 8) as u8,
            count as u8,
        ];
        let resp = self.do_request(&pdu).await?;
        if resp.len() < 2 || resp[0] != 0x04 {
            return Err(ModbusError::Other("unexpected read-input response".into()));
        }
        let byte_count = resp[1] as usize;
        let mut data = Vec::with_capacity(byte_count / 2);
        for i in (0..byte_count).step_by(2) {
            data.push(u16::from_be_bytes([resp[2 + i], resp[3 + i]]));
        }
        Ok(data)
    }

    /// Write a single holding register (FC06).
    pub async fn write_single_register(
        &mut self,
        addr: u16,
        value: u16,
    ) -> Result<(), ModbusError> {
        let pdu = [
            0x06u8,
            (addr >> 8) as u8,
            addr as u8,
            (value >> 8) as u8,
            value as u8,
        ];
        self.do_request(&pdu).await?;
        Ok(())
    }

    /// Write multiple holding registers (FC16).
    pub async fn write_multiple_registers(
        &mut self,
        addr: u16,
        values: &[u16],
    ) -> Result<(), ModbusError> {
        let mut pdu = Vec::with_capacity(5 + values.len() * 2 + 1);
        pdu.push(0x10);
        pdu.extend_from_slice(&addr.to_be_bytes());
        pdu.extend_from_slice(&(values.len() as u16).to_be_bytes());
        pdu.push((values.len() * 2) as u8);
        for v in values {
            pdu.extend_from_slice(&v.to_be_bytes());
        }
        self.do_request(&pdu).await?;
        Ok(())
    }

    /// Send a raw PDU (e.g. built by the frame constructor) and return the
    /// response PDU — used by the manual "send raw" panel. `unit_id` overrides
    /// the connection's configured station for this one transaction.
    pub async fn request_raw(
        &mut self,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<Vec<u8>, ModbusError> {
        self.transport.request(unit_id, pdu).await
    }

    /// Like [`request_raw`] but also returns the wire-level TX/RX ADUs and the
    /// measured round-trip time so the UI can render a traffic log.
    pub async fn request_frame(
        &mut self,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<Frame, ModbusError> {
        self.transport.request_frame(unit_id, pdu).await
    }
}
