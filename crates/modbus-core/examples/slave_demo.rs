//! 端到端演示：内置从站（Modbus TCP）独立运行验证。
//! 启动一个 TCP 从站（0.0.0.0:1503），用内置 TCP 客户端发起
//! FC03/FC06/FC16/FC01/FC05 请求，验证「读/写/异常/响应所有 Unit ID」
//! 以及数据共享（写入后回读一致）。
//!
//! 用法：cargo run -p modbus-core --example slave_demo

use modbus_core::server::{run_tcp_slave_on, new_shared_image, SharedImage};
use modbus_core::framing;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

async fn client_request(
    stream: &mut TcpStream,
    unit_id: u8,
    tid: u16,
    pdu: &[u8],
) -> Vec<u8> {
    let req = framing::encode_tcp(unit_id, tid, pdu);
    stream.write_all(&req).await.unwrap();
    let mut hdr = [0u8; 7];
    stream.read_exact(&mut hdr).await.unwrap();
    let len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
    let mut body = vec![0u8; len - 1];
    stream.read_exact(&mut body).await.unwrap();
    body
}

#[tokio::main]
async fn main() {
    // 1) 共享数据镜像（稀疏模型：按需注册，无预分配），挂到 Unit ID 1 下
    let image: SharedImage = new_shared_image();
    let mut units = modbus_core::server::UnitImages::new();
    units.insert(1, image);
    let units = std::sync::Arc::new(std::sync::Mutex::new(units));

    // 2) 绑定 0.0.0.0:1503 并启动从站
    let listener = TcpListener::bind(("0.0.0.0", 1503)).await.unwrap();
    println!("==> 内置 TCP 从站已启动：0.0.0.0:1503（Unit 1）");
    let (tx, rx) = broadcast::channel(2);
    let on_write = std::sync::Arc::new(|_unit_id: u8| {});
    tokio::spawn(run_tcp_slave_on(listener, units, on_write, rx, Default::default()));

    // 3) 主站连接
    let mut c = TcpStream::connect("127.0.0.1:1503").await.unwrap();
    println!("==> 主站已连接");

    // FC03 读保持寄存器 @0..3（初始 0）
    let r = client_request(&mut c, 1, 1, &[0x03, 0x00, 0x00, 0x00, 0x03]).await;
    println!("FC03 读 3 保持寄存器 -> {r:?}  (应为 03 06 00 00 00 00)");

    // FC06 写保持寄存器 @1 = 0x1234
    let r = client_request(&mut c, 1, 2, &[0x06, 0x00, 0x01, 0x12, 0x34]).await;
    println!("FC06 写 @1=0x1234   -> {r:?}  (回声)");

    // FC16 写多寄存器 @2..4 = [10, 20]
    let r = client_request(&mut c, 1, 3, &[0x10, 0x00, 0x02, 0x00, 0x02, 0x04, 0x00, 0x0A, 0x00, 0x14]).await;
    println!("FC16 写 @2..4        -> {r:?}  (回声)");

    // 回读 @0..4 验证数据共享（写入后读到一致值）
    let r = client_request(&mut c, 7, 4, &[0x03, 0x00, 0x00, 0x00, 0x04]).await;
    // 注意 unit_id=7：从站应照常响应（响应所有 Unit ID）
    println!("FC03 回读 @0..4 (UnitID=7) -> {r:?}  (含 0x1234 / 0x000A / 0x0014)");

    // FC01 读线圈 @0..8
    let r = client_request(&mut c, 1, 5, &[0x01, 0x00, 0x00, 0x00, 0x08]).await;
    println!("FC01 读 8 线圈      -> {r:?}");

    // FC05 写线圈 @0 = ON
    let r = client_request(&mut c, 1, 6, &[0x05, 0x00, 0x00, 0xFF, 0x00]).await;
    println!("FC05 写线圈 @0=ON   -> {r:?}  (回声)");

    // 未注册地址读 -> 隐式零（稀疏模型），跨越 0xFFFF 的读才是非法地址
    let r = client_request(&mut c, 1, 7, &[0x03, 0x00, 0xFF, 0x00, 0x01]).await;
    println!("FC03 未注册地址读    -> {r:?}  (应为 03 02 00 00 隐式零)");

    println!("==> 演示完成，停止从站");
    let _ = tx.send(());
}
