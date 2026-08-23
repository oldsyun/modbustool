//! Modbus TCP transport: a single connection that exchanges ADU byte frames.
//! Supports both standard Modbus TCP (MBAP) and RTU-over-TCP (RTU frames
//! carried over a TCP stream — common with serial-to-Ethernet gateways).

use crate::error::ModbusError;
use crate::framing;
use crate::Frame;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Framing mode for a TCP connection.
/// - `Tcp`: standard Modbus TCP (MBAP header + PDU).
/// - `Rtu`: RTU-over-TCP — same on-wire frame as serial RTU (unit_id + PDU +
///   CRC16), carried over a TCP stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpMode {
    Tcp,
    Rtu,
}

pub struct TcpTransport {
    stream: TcpStream,
    transaction: u16,
    timeout: std::time::Duration,
    mode: TcpMode,
    /// Inter-frame gap used by the RTU mode to delimit frames.
    inter_frame: Duration,
    /// TCP 是字节流：一次 read 可能包含半帧 / 多帧 / 杂散残留数据。
    /// 该缓冲累积未消费字节，按 MBAP 头扫描出完整且 TID 匹配的帧，
    /// 从而容忍分片、粘包、乱序与网关回显。
    rx_buf: Vec<u8>,
}

impl TcpTransport {
    pub async fn connect(
        host: &str,
        port: u16,
        timeout: std::time::Duration,
    ) -> Result<Self, ModbusError> {
        Self::connect_with_mode(host, port, timeout, TcpMode::Tcp, Duration::from_millis(5)).await
    }

    /// Open a TCP connection with a chosen framing mode.
    /// `inter_frame` is only used in `Rtu` mode (frame-gap probe).
    pub async fn connect_with_mode(
        host: &str,
        port: u16,
        timeout: std::time::Duration,
        mode: TcpMode,
        inter_frame: Duration,
    ) -> Result<Self, ModbusError> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| ModbusError::Other("connect timeout".into()))??;
        Ok(Self {
            stream,
            transaction: 0,
            timeout,
            mode,
            inter_frame,
            rx_buf: Vec::new(),
        })
    }

    /// Send a PDU and return the response PDU (exception checked).
    pub async fn request(&mut self, unit_id: u8, pdu: &[u8]) -> Result<Vec<u8>, ModbusError> {
        let (_tx, _rx, _rtt, resp_pdu) = self.request_frame(unit_id, pdu).await?;
        Ok(resp_pdu)
    }

    /// Like [`request`] but also returns the full ADUs on both directions and
    /// the measured RTT. In RTU mode the ADU is `[unit_id, ...PDU, CRC16]`;
    /// in TCP mode it is the standard MBAP frame.
    pub async fn request_frame(
        &mut self,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<Frame, ModbusError> {
        match self.mode {
            TcpMode::Tcp => self.request_frame_tcp(unit_id, pdu).await,
            TcpMode::Rtu => self.request_frame_rtu(unit_id, pdu).await,
        }
    }

    async fn request_frame_tcp(
        &mut self,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<Frame, ModbusError> {
        let tid = self.transaction;
        self.transaction = self.transaction.wrapping_add(1);
        let tx_adu = framing::encode_tcp(unit_id, tid, pdu);

        tokio::time::timeout(self.timeout, self.stream.write_all(&tx_adu))
            .await
            .map_err(|_| ModbusError::Other("write timeout".into()))??;

        // 读取模型：不用「定长 read_exact」猜帧边界（分片/粘包/残留/回显都会
        // 导致错位或阻塞超时），而是把数据累积进 rx_buf，按 MBAP 头扫描出
        // 完整且 TID 匹配的帧；数据不足就继续读，直到超时。
        let t0 = Instant::now();
        let deadline = t0 + self.timeout;
        loop {
            if let Some((rx_adu, resp_pdu)) = self.take_frame(&tx_adu, tid) {
                let rtt = t0.elapsed();
                if resp_pdu.len() >= 2 && (resp_pdu[0] & 0x80) != 0 {
                    return Err(ModbusError::Exception(resp_pdu[1]));
                }
                return Ok((tx_adu, rx_adu, rtt, resp_pdu));
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(ModbusError::Other("read body timeout".into()));
            }
            let mut chunk = [0u8; 256];
            match tokio::time::timeout(
                deadline.saturating_duration_since(now),
                self.stream.read(&mut chunk),
            )
            .await
            {
                Ok(Ok(0)) => return Err(ModbusError::Other("connection closed by peer".into())),
                Ok(Ok(n)) => self.rx_buf.extend_from_slice(&chunk[..n]),
                Ok(Err(e)) => return Err(ModbusError::Other(format!("read error: {e}"))),
                Err(_) => return Err(ModbusError::Other("read body timeout".into())),
            }
        }
    }

    /// 从接收缓冲中提取一个完整且 TID 匹配的 MBAP 帧，返回 (完整帧, 响应 PDU)。
    ///
    /// 帧长度 = 6 + MBAP length（length 含 unit_id，7 字节头已读 unit_id，
    /// 故剩余 body 为 length - 1 字节——两者等价，此处以完整帧为单位扫描）。
    /// 逐帧跳过：非法 length、协议 ID != 0、TID 不匹配的杂散数据；
    /// 以及「与请求帧完全相同」的帧（串口网关回显），但仅当后面还跟着
    /// 数据时才跳过——FC05/06/16 的合法响应 PDU 本身就等于请求 PDU。
    fn take_frame(&mut self, tx_adu: &[u8], want_tid: u16) -> Option<(Vec<u8>, Vec<u8>)> {
        let mut i = 0;
        while self.rx_buf.len() >= i + 7 {
            let len = u16::from_be_bytes([self.rx_buf[i + 4], self.rx_buf[i + 5]]) as usize;
            if !(1..=254).contains(&len) {
                i += 1; // 非法长度：滑动 1 字节重新同步
                continue;
            }
            let total = 6 + len;
            if self.rx_buf.len() < i + total {
                break; // 帧不完整，等更多数据
            }
            let proto = u16::from_be_bytes([self.rx_buf[i + 2], self.rx_buf[i + 3]]);
            let tid_rx = u16::from_be_bytes([self.rx_buf[i], self.rx_buf[i + 1]]);
            if proto == 0 && tid_rx == want_tid {
                let frame = self.rx_buf[i..i + total].to_vec();
                let pdu = frame[7..].to_vec();
                let echo = frame[7..] == tx_adu[7..] && self.rx_buf.len() >= i + total + 7;
                if !echo {
                    self.rx_buf.drain(..i + total);
                    return Some((frame, pdu));
                }
                i += total; // 回显帧，跳过继续找真响应
                continue;
            }
            i += total; // 非目标帧（旧响应/乱序数据），跳过
        }
        if i > 0 {
            // 丢弃已确认的垃圾前缀，防止缓冲无限膨胀
            self.rx_buf.drain(..i);
        }
        None
    }

    /// RTU-over-TCP: write a serial-style RTU frame and read back the
    /// response delimited by the configured inter-frame gap (no MBAP).
    async fn request_frame_rtu(
        &mut self,
        unit_id: u8,
        pdu: &[u8],
    ) -> Result<Frame, ModbusError> {
        let tx_adu = framing::encode_rtu(unit_id, pdu);
        tokio::time::timeout(self.timeout, self.stream.write_all(&tx_adu))
            .await
            .map_err(|_| ModbusError::Other("write timeout".into()))??;

        let t0 = Instant::now();
        // Small read probe (must exceed the inter-frame gap) so the loop
        // exits ~immediately after the last byte — same fix as serial RTU.
        let probe = self.inter_frame.max(Duration::from_millis(20));
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        let mut last = Instant::now();
        loop {
            match tokio::time::timeout(probe, self.stream.read(&mut byte)).await {
                Ok(Ok(1)) => {
                    buf.push(byte[0]);
                    last = Instant::now();
                }
                _ => {
                    if last.elapsed() >= self.inter_frame {
                        break;
                    }
                    if last.elapsed() > self.timeout {
                        break;
                    }
                }
            }
        }
        let rtt = last.saturating_duration_since(t0);
        if buf.is_empty() {
            return Err(ModbusError::Other("no response (timeout)".into()));
        }
        let (unit, resp_pdu) = framing::parse_rtu(&buf)?;
        let rx_adu = framing::encode_rtu(unit, &resp_pdu);
        if resp_pdu.len() >= 2 && (resp_pdu[0] & 0x80) != 0 {
            return Err(ModbusError::Exception(resp_pdu[1]));
        }
        Ok((tx_adu, rx_adu, rtt, resp_pdu))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试：模拟标准 Modbus TCP 从站，返回真实 FC03 响应帧
    /// （29 字节 = 7 字节 MBAP + 22 字节 PDU）。
    ///
    /// 曾因 MBAP length 字段（23，含 unit_id）被当作剩余 body 长度读取，
    /// 读完 7 字节头后多读 1 字节导致永久阻塞 → "read body timeout"。
    #[tokio::test]
    async fn tcp_read_fc03_response_roundtrip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // 模拟从站：读完整请求后回标准 FC03 响应。
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut hdr = [0u8; 7];
            sock.read_exact(&mut hdr).await.unwrap();
            let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
            let mut req = vec![0u8; len - 1];
            sock.read_exact(&mut req).await.unwrap();
            let tid_req = u16::from_be_bytes([hdr[0], hdr[1]]);

            // 读 10 个保持寄存器：PDU = [03, 14, 20 字节数据]
            let mut pdu = vec![0x03, 0x14];
            pdu.extend_from_slice(&[0u8; 20]);
            let resp = framing::encode_tcp(1, tid_req, &pdu);
            sock.write_all(&resp).await.unwrap();
        });

        let mut tr =
            TcpTransport::connect("127.0.0.1", addr.port(), Duration::from_secs(2))
                .await
                .unwrap();
        let (tx, rx, _rtt, resp_pdu) = tr
            .request_frame(1, &[0x03, 0x00, 0x00, 0x00, 0x0A])
            .await
            .expect("request_frame 不应超时");
        server.await.unwrap();

        // 请求帧：MBAP(len=6) + FC03 PDU(5 字节) = 12 字节
        assert_eq!(tx.len(), 12);
        // 响应帧完整 29 字节，而非缺 1 字节
        assert_eq!(rx.len(), 29);
        // 响应 PDU: [03, 14, 20 字节数据]
        assert_eq!(resp_pdu.len(), 22);
        assert_eq!(resp_pdu[0], 0x03);
        assert_eq!(resp_pdu[1], 0x14);
        assert_eq!(&resp_pdu[2..], &[0u8; 20][..]);
    }

    /// 粘包 + 杂散数据：一次 write 里先塞一帧「旧事务」数据（TID 不匹配）
    /// 再拼真响应。客户端必须跳过杂散帧、按 TID 取出目标响应。
    #[tokio::test]
    async fn tcp_sticky_garbage_then_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut hdr = [0u8; 7];
            sock.read_exact(&mut hdr).await.unwrap();
            let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
            let mut req = vec![0u8; len - 1];
            sock.read_exact(&mut req).await.unwrap();
            let tid_req = u16::from_be_bytes([hdr[0], hdr[1]]);

            let mut pdu = vec![0x03, 0x14];
            pdu.extend_from_slice(&[0u8; 20]);
            let resp = framing::encode_tcp(1, tid_req, &pdu);
            let stale = framing::encode_tcp(1, 0xFFFF, &[0x03, 0x00, 0x00, 0x00, 0x02]);
            let mut blob = stale.clone();
            blob.extend_from_slice(&resp);
            sock.write_all(&blob).await.unwrap();
        });

        let mut tr =
            TcpTransport::connect("127.0.0.1", addr.port(), Duration::from_secs(2))
                .await
                .unwrap();
        let (_tx, rx, _rtt, resp_pdu) = tr
            .request_frame(1, &[0x03, 0x00, 0x00, 0x00, 0x0A])
            .await
            .expect("应从粘包数据中取出 TID 匹配的响应");
        server.await.unwrap();

        assert_eq!(rx.len(), 29);
        assert_eq!(resp_pdu.len(), 22);
        assert_eq!(resp_pdu[0], 0x03);
        assert_eq!(resp_pdu[1], 0x14);
    }

    /// 分片：响应帧被拆成「7 字节头」与「body」两次 write 到达，
    /// 中间间隔 30ms。客户端必须能跨 read 拼接出完整帧。
    #[tokio::test]
    async fn tcp_fragmented_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut hdr = [0u8; 7];
            sock.read_exact(&mut hdr).await.unwrap();
            let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
            let mut req = vec![0u8; len - 1];
            sock.read_exact(&mut req).await.unwrap();
            let tid_req = u16::from_be_bytes([hdr[0], hdr[1]]);

            let mut pdu = vec![0x03, 0x14];
            pdu.extend_from_slice(&[0u8; 20]);
            let resp = framing::encode_tcp(1, tid_req, &pdu);
            sock.write_all(&resp[..7]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            sock.write_all(&resp[7..]).await.unwrap();
        });

        let mut tr =
            TcpTransport::connect("127.0.0.1", addr.port(), Duration::from_secs(2))
                .await
                .unwrap();
        let (_tx, rx, _rtt, resp_pdu) = tr
            .request_frame(1, &[0x03, 0x00, 0x00, 0x00, 0x0A])
            .await
            .expect("分片响应应能拼接读取");
        server.await.unwrap();

        assert_eq!(rx.len(), 29);
        assert_eq!(resp_pdu.len(), 22);
        assert_eq!(resp_pdu[0], 0x03);
    }

    /// 网关回显：从站先把收到的请求帧原样发回（echo），随后再发真响应。
    /// 客户端必须跳过回显帧、取到真正的响应（读功能码场景）。
    #[tokio::test]
    async fn tcp_echo_then_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut hdr = [0u8; 7];
            sock.read_exact(&mut hdr).await.unwrap();
            let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
            let mut req = vec![0u8; len - 1];
            sock.read_exact(&mut req).await.unwrap();
            let tid_req = u16::from_be_bytes([hdr[0], hdr[1]]);

            // 回显请求帧（TID 与请求一致，PDU 与请求完全相同）
            let echo = framing::encode_tcp(1, tid_req, &req);
            let mut pdu = vec![0x03, 0x14];
            pdu.extend_from_slice(&[0u8; 20]);
            let resp = framing::encode_tcp(1, tid_req, &pdu);
            let mut blob = echo.clone();
            blob.extend_from_slice(&resp);
            sock.write_all(&blob).await.unwrap();
        });

        let mut tr =
            TcpTransport::connect("127.0.0.1", addr.port(), Duration::from_secs(2))
                .await
                .unwrap();
        let (_tx, rx, _rtt, resp_pdu) = tr
            .request_frame(1, &[0x03, 0x00, 0x00, 0x00, 0x0A])
            .await
            .expect("应跳过回显帧并取到真响应");
        server.await.unwrap();

        assert_eq!(rx.len(), 29);
        assert_eq!(resp_pdu.len(), 22);
        assert_eq!(resp_pdu[0], 0x03);
        assert_eq!(resp_pdu[1], 0x14);
    }
}