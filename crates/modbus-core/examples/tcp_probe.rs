//! 诊断工具：连接任意 Modbus TCP/UDP 从站，发起一次 FC03 读请求，
//! 打印发送帧 / 接收帧 / 解析结果与耗时。用于验证 modbus-core 的
//! TCP（MBAP 长度、分片、粘包、回显）、UDP 与 RTU-over-IP 解析是否正常。
//!
//! 用法：cargo run -p modbus-core --example tcp_probe -- [tcp|udp] [rtu] [host] [port] [addr] [count]
//! `rtu` 为可选标志：使用 RTU 帧（无 MBAP）而非标准 MBAP 帧。
//! 默认：tcp 127.0.0.1 1502 0 10

use modbus_core::transport::tcp::{TcpMode, TcpTransport};
use modbus_core::transport::udp::{UdpMode, UdpTransport};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let transport = args.next().unwrap_or_else(|| "tcp".into());
    // 可选第二个参数 "rtu"：RTU 帧模式（无 MBAP 头）
    let mut rtu = false;
    let host = match args.next() {
        Some(s) if s == "rtu" => {
            rtu = true;
            args.next().unwrap_or_else(|| "127.0.0.1".into())
        }
        Some(s) => s,
        None => "127.0.0.1".into(),
    };
    let port: u16 = args
        .next()
        .map(|s| s.parse().unwrap_or(1502))
        .unwrap_or(1502);
    let addr: u16 = args
        .next()
        .map(|s| s.parse().unwrap_or(0))
        .unwrap_or(0);
    let count: u16 = args
        .next()
        .map(|s| s.parse().unwrap_or(10))
        .unwrap_or(10);

    let proto = if transport.eq_ignore_ascii_case("udp") { "UDP" } else { "TCP" };
    let framing = if rtu { "RTU 帧" } else { "MBAP 帧" };
    println!(
        "==> 连接 {proto} {host}:{port}（{framing}），FC03 读 addr=0x{addr:04X} count={count} ..."
    );

    let pdu = [
        0x03u8,
        (addr >> 8) as u8,
        addr as u8,
        (count >> 8) as u8,
        count as u8,
    ];
    let (tx, rx, rtt, resp) = if proto == "UDP" {
        if rtu {
            let mut tr =
                UdpTransport::connect_with_mode(&host, port, Duration::from_secs(3), UdpMode::Rtu)
                    .await?;
            println!("==> RTU-over-UDP 通道已就绪");
            tr.request_frame(1, &pdu).await?
        } else {
            let mut tr = UdpTransport::connect(&host, port, Duration::from_secs(3)).await?;
            println!("==> UDP 通道已就绪");
            tr.request_frame(1, &pdu).await?
        }
    } else if rtu {
        let mut tr = TcpTransport::connect_with_mode(
            &host,
            port,
            Duration::from_secs(3),
            TcpMode::Rtu,
            Duration::from_millis(5),
        )
        .await?;
        println!("==> RTU-over-TCP 连接成功");
        tr.request_frame(1, &pdu).await?
    } else {
        let mut tr = TcpTransport::connect(&host, port, Duration::from_secs(3)).await?;
        println!("==> TCP 连接成功");
        tr.request_frame(1, &pdu).await?
    };

    println!("==> 发送帧 ({}B): {}", tx.len(), hex(&tx));
    println!("==> 接收帧 ({}B): {}", rx.len(), hex(&rx));
    println!("==> 响应 PDU ({}B): {}", resp.len(), hex(&resp));
    println!("==> RTT: {:.1} ms", rtt.as_secs_f64() * 1000.0);

    if resp.len() >= 2 && resp[0] == 0x03 {
        let bc = resp[1] as usize;
        let mut regs = Vec::with_capacity(bc / 2);
        for i in (0..bc).step_by(2) {
            if i + 1 < resp.len() {
                regs.push(u16::from_be_bytes([resp[2 + i], resp[3 + i]]));
            }
        }
        println!("==> 寄存器: {regs:?}");
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}
