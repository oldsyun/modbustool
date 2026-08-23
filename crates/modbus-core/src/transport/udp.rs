//! Modbus UDP transport (Modbus/UDP — RFC 7252 之外的无连接变体):
//! 帧格式与 Modbus TCP 完全相同（MBAP header + PDU），但承载在 UDP
//! 数据报上，没有连接与流边界：一个请求 = 一个数据报，一个响应 =
//! 一个数据报。
//!
//! 与 TCP 的差异仅在于传输语义，因此复用 `framing::encode_tcp` /
//! `framing::parse_tcp` 做帧编解码。
//!
//! 也支持 RTU-over-UDP（常见于串口转以太网网关的无连接模式）：
//! 帧格式为串口 RTU（unit_id + PDU + CRC16），无 MBAP 头，每个
//! 数据报恰好一个 RTU 帧。

use crate::error::ModbusError;
use crate::framing;
use crate::Frame;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// Framing mode for a UDP socket.
/// - `Tcp`: standard Modbus UDP (MBAP header + PDU).
/// - `Rtu`: RTU-over-UDP — serial RTU frame (unit_id + PDU + CRC16) carried
///   in one datagram, no MBAP header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpMode {
    Tcp,
    Rtu,
}

pub struct UdpTransport {
    socket: UdpSocket,
    transaction: u16,
    timeout: Duration,
    mode: UdpMode,
}

impl UdpTransport {
    /// 创建并"连接"一个 Modbus UDP 通道（标准 MBAP 帧）。
    ///
    /// UDP 的 connect 只设置默认对端（不产生任何网络流量），后续
    /// send/recv 即发送/接收该对端的数据报。随机绑定本地端口。
    pub async fn connect(host: &str, port: u16, timeout: Duration) -> Result<Self, ModbusError> {
        Self::connect_with_mode(host, port, timeout, UdpMode::Tcp).await
    }

    /// 创建通道并选择帧模式（`Tcp` = MBAP 帧；`Rtu` = RTU 帧 over UDP）。
    pub async fn connect_with_mode(
        host: &str,
        port: u16,
        timeout: Duration,
        mode: UdpMode,
    ) -> Result<Self, ModbusError> {
        let socket = tokio::time::timeout(timeout, UdpSocket::bind(("0.0.0.0", 0)))
            .await
            .map_err(|_| ModbusError::Other("udp bind timeout".into()))?
            .map_err(ModbusError::Io)?;
        tokio::time::timeout(timeout, socket.connect((host, port)))
            .await
            .map_err(|_| ModbusError::Other("udp connect timeout".into()))?
            .map_err(ModbusError::Io)?;
        Ok(Self {
            socket,
            transaction: 0,
            timeout,
            mode,
        })
    }

    /// Send a PDU and return the response PDU (exception checked).
    pub async fn request(&mut self, unit_id: u8, pdu: &[u8]) -> Result<Vec<u8>, ModbusError> {
        let (_tx, _rx, _rtt, resp_pdu) = self.request_frame(unit_id, pdu).await?;
        Ok(resp_pdu)
    }

    /// Like [`request`] but also returns the full ADUs and the measured RTT
    /// (same `Frame` shape as the TCP/RTU transports so callers are uniform).
    pub async fn request_frame(
        &mut self,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<Frame, ModbusError> {
        match self.mode {
            UdpMode::Tcp => self.request_frame_mbap(unit_id, pdu).await,
            UdpMode::Rtu => self.request_frame_rtu(unit_id, pdu).await,
        }
    }

    /// 标准 Modbus/UDP：MBAP 帧，TID 校验丢弃乱序/重传数据报。
    async fn request_frame_mbap(
        &mut self,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<Frame, ModbusError> {
        let tid = self.transaction;
        self.transaction = self.transaction.wrapping_add(1);
        let tx_adu = framing::encode_tcp(unit_id, tid, pdu);

        tokio::time::timeout(self.timeout, self.socket.send(&tx_adu))
            .await
            .map_err(|_| ModbusError::Other("udp send timeout".into()))?
            .map_err(ModbusError::Io)?;

        // 接收：UDP 可能乱序 / 重传 / 混杂广播数据报，逐个解析并校验
        // TID 与协议 ID，丢弃不匹配的数据报，直到拿到目标响应或超时。
        let t0 = Instant::now();
        let deadline = t0 + self.timeout;
        let mut buf = [0u8; 1024];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(ModbusError::Other("read body timeout".into()));
            }
            match tokio::time::timeout(
                deadline.saturating_duration_since(now),
                self.socket.recv(&mut buf),
            )
            .await
            {
                Ok(Ok(0)) => continue, // 空数据报：忽略
                Ok(Ok(n)) => {
                    let (rtid, _unit, resp_pdu) = match framing::parse_tcp(&buf[..n]) {
                        Ok(v) => v,
                        Err(_) => continue, // 非法帧：忽略
                    };
                    if rtid != tid {
                        continue; // 旧事务 / 乱序数据报：丢弃
                    }
                    let rx_adu = buf[..n].to_vec();
                    let rtt = t0.elapsed();
                    if resp_pdu.len() >= 2 && (resp_pdu[0] & 0x80) != 0 {
                        return Err(ModbusError::Exception(resp_pdu[1]));
                    }
                    return Ok((tx_adu, rx_adu, rtt, resp_pdu));
                }
                Ok(Err(e)) => return Err(ModbusError::Io(e)),
                Err(_) => return Err(ModbusError::Other("read body timeout".into())),
            }
        }
    }

    /// RTU-over-UDP：一个数据报一个 RTU 帧（unit_id + PDU + CRC16），
    /// 无 MBAP 头。丢弃 CRC 校验失败 / 站号不匹配的数据报。
    async fn request_frame_rtu(
        &mut self,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<Frame, ModbusError> {
        let tx_adu = framing::encode_rtu(unit_id, pdu);

        tokio::time::timeout(self.timeout, self.socket.send(&tx_adu))
            .await
            .map_err(|_| ModbusError::Other("udp send timeout".into()))?
            .map_err(ModbusError::Io)?;

        let t0 = Instant::now();
        let deadline = t0 + self.timeout;
        let mut buf = [0u8; 1024];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(ModbusError::Other("read body timeout".into()));
            }
            match tokio::time::timeout(
                deadline.saturating_duration_since(now),
                self.socket.recv(&mut buf),
            )
            .await
            {
                Ok(Ok(0)) => continue, // 空数据报：忽略
                Ok(Ok(n)) => {
                    // 校验 CRC + 站号；杂散/广播数据报丢弃
                    let (runit, resp_pdu) = match framing::parse_rtu(&buf[..n]) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if runit != unit_id {
                        continue;
                    }
                    let rx_adu = framing::encode_rtu(runit, &resp_pdu);
                    let rtt = t0.elapsed();
                    if resp_pdu.len() >= 2 && (resp_pdu[0] & 0x80) != 0 {
                        return Err(ModbusError::Exception(resp_pdu[1]));
                    }
                    return Ok((tx_adu, rx_adu, rtt, resp_pdu));
                }
                Ok(Err(e)) => return Err(ModbusError::Io(e)),
                Err(_) => return Err(ModbusError::Other("read body timeout".into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 基本往返：模拟标准 Modbus UDP 从站，回 29 字节 FC03 响应。
    #[tokio::test]
    async fn udp_roundtrip() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let saddr = server.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 12); // 请求帧 12B
            let tid = u16::from_be_bytes([buf[0], buf[1]]);
            let mut pdu = vec![0x03, 0x14];
            pdu.extend_from_slice(&[0u8; 20]);
            let resp = framing::encode_tcp(1, tid, &pdu);
            server.send_to(&resp, from).await.unwrap();
        });

        let mut tr =
            UdpTransport::connect("127.0.0.1", saddr.port(), Duration::from_secs(2))
                .await
                .unwrap();
        let (tx, rx, _rtt, resp_pdu) = tr
            .request_frame(1, &[0x03, 0x00, 0x00, 0x00, 0x0A])
            .await
            .expect("udp request_frame 不应超时");
        server_task.await.unwrap();

        assert_eq!(tx.len(), 12);
        assert_eq!(rx.len(), 29);
        assert_eq!(resp_pdu.len(), 22);
        assert_eq!(resp_pdu[0], 0x03);
        assert_eq!(resp_pdu[1], 0x14);
    }

    /// 乱序/重传：从站先发一帧 TID 不匹配的旧数据报，再发真响应。
    /// 客户端必须丢弃旧数据报、取到 TID 匹配的响应。
    #[tokio::test]
    async fn udp_skips_stale_datagram() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let saddr = server.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 12);
            let tid = u16::from_be_bytes([buf[0], buf[1]]);
            // 先发一帧旧事务数据报（TID 不匹配）
            let stale = framing::encode_tcp(1, 0xFFFF, &[0x03, 0x00, 0x00, 0x00, 0x02]);
            server.send_to(&stale, from).await.unwrap();
            // 再发真响应
            let mut pdu = vec![0x03, 0x14];
            pdu.extend_from_slice(&[0u8; 20]);
            let resp = framing::encode_tcp(1, tid, &pdu);
            server.send_to(&resp, from).await.unwrap();
        });

        let mut tr =
            UdpTransport::connect("127.0.0.1", saddr.port(), Duration::from_secs(2))
                .await
                .unwrap();
        let (_tx, rx, _rtt, resp_pdu) = tr
            .request_frame(1, &[0x03, 0x00, 0x00, 0x00, 0x0A])
            .await
            .expect("应丢弃旧数据报并取到真响应");
        server_task.await.unwrap();

        assert_eq!(rx.len(), 29);
        assert_eq!(resp_pdu.len(), 22);
        assert_eq!(resp_pdu[0], 0x03);
        assert_eq!(resp_pdu[1], 0x14);
    }

    /// RTU-over-UDP 基本往返：数据报 = RTU 帧（unit + PDU + CRC16）。
    #[tokio::test]
    async fn udp_rtu_roundtrip() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let saddr = server.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            // 请求为 RTU 帧：01 03 00 00 00 0A + CRC(2) = 8B
            assert_eq!(n, 8);
            // 回 RTU 响应：01 03 14 + 20 字节 + CRC
            let mut pdu = vec![0x03, 0x14];
            pdu.extend_from_slice(&[0u8; 20]);
            let resp = framing::encode_rtu(1, &pdu);
            server.send_to(&resp, from).await.unwrap();
        });

        let mut tr =
            UdpTransport::connect_with_mode("127.0.0.1", saddr.port(), Duration::from_secs(2), UdpMode::Rtu)
                .await
                .unwrap();
        let (tx, rx, _rtt, resp_pdu) = tr
            .request_frame(1, &[0x03, 0x00, 0x00, 0x00, 0x0A])
            .await
            .expect("rtu-over-udp request_frame 不应超时");
        server_task.await.unwrap();

        // TX RTU 帧 8B；RX RTU 帧 25B（unit + PDU 22 + CRC 2）
        assert_eq!(tx.len(), 8);
        assert_eq!(rx.len(), 25);
        assert_eq!(resp_pdu.len(), 22);
        assert_eq!(resp_pdu[0], 0x03);
        assert_eq!(resp_pdu[1], 0x14);
    }

    /// RTU-over-UDP：站号不匹配的数据报应被丢弃。
    #[tokio::test]
    async fn udp_rtu_skips_wrong_unit() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let saddr = server.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 8);
            // 先发一帧站号=2 的杂散数据报（CRC 有效但站号不匹配）
            let mut pdu = vec![0x03, 0x14];
            pdu.extend_from_slice(&[0u8; 20]);
            let stale = framing::encode_rtu(2, &pdu);
            server.send_to(&stale, from).await.unwrap();
            // 再发站号=1 的真响应
            let resp = framing::encode_rtu(1, &pdu);
            server.send_to(&resp, from).await.unwrap();
        });

        let mut tr =
            UdpTransport::connect_with_mode("127.0.0.1", saddr.port(), Duration::from_secs(2), UdpMode::Rtu)
                .await
                .unwrap();
        let (_tx, rx, _rtt, resp_pdu) = tr
            .request_frame(1, &[0x03, 0x00, 0x00, 0x00, 0x0A])
            .await
            .expect("应丢弃站号不匹配的数据报并取到真响应");
        server_task.await.unwrap();

        assert_eq!(rx.len(), 25);
        assert_eq!(resp_pdu.len(), 22);
        assert_eq!(resp_pdu[0], 0x03);
        assert_eq!(resp_pdu[1], 0x14);
    }
}
