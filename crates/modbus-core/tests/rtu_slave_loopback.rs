//! RTU 串口从站端到端验证：用 `UnixStream::pair()` 创建一对互联的双向字节流，
//! 一端跑真实从站循环 `run_rtu_slave_on_rw`（即 `run_rtu_slave_blocking` /
//! `run_rtu_slave_on_port` 委托的生产核心），另一端用手动 RTU 帧（模拟真实主站）
//! 发起 FC03/FC06 请求，验证从站能否正确应答。
//!
//! 为什么用 `UnixStream` 而非 `SerialPort::pair()`：在 macOS 上 `serial2` 的
//! `set_configuration` 会下发 `IOSSIOSPEED` ioctl 设置波特率，而伪终端（pty）不支持
//! 该 ioctl（返回 ENOTTY），导致 `SerialPort::pair()`/`open()` 在任何 pty 上都打不开。
//! `UnixStream` 是普通双向字节流，没有终端行规 / 波特率概念，既能跑通从站循环，又可
//! 在 macOS 上稳定执行。串口特有的「raw 模式」修复（`set_raw()` / cfmakeraw）已由
//! `SerialPort::open` 在打开时应用，并经 pty 探针（见提交说明）验证：未设 raw 时
//! ONLCR 会把 `0x0A` 改写成 `0x0D 0x0A`，设 raw 后不再改写。
//!
//! 运行：cargo test -p modbus-core --features rtu --test rtu_slave_loopback

#![cfg(all(unix, feature = "rtu"))]

use modbus_core::framing;
use modbus_core::server::{new_shared_image, run_rtu_slave_on_rw, UnitImages};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 主站侧：写 RTU 请求帧并读回应答（手动 inter-frame 定界 + CRC 校验）。
/// 直接用 `UnixStream`（std::io::Read/Write）收发，非阻塞读 + 帧间定界。
fn master_request(master: &mut UnixStream, unit_id: u8, pdu: &[u8]) -> Vec<u8> {
    let adu = framing::encode_rtu(unit_id, pdu);
    master.write_all(&adu).expect("master write");
    master.flush().expect("master flush");
    eprintln!("[master] wrote req {adu:?}");

    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    let mut last: Option<Instant> = None;
    let start = Instant::now();
    let inter = Duration::from_millis(5);
    loop {
        match master.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                last = Some(Instant::now());
                eprintln!("[master] got byte {:#04x} (buf len {})", byte[0], buf.len());
            }
            Ok(0) => panic!("master read EOF; partial buf={buf:?}"),
            _ => {
                // 暂无可读（WouldBlock）：检查 inter-frame 定界
                if let Some(t) = last {
                    if t.elapsed() >= inter && buf.len() >= 5 && framing::verify_crc(&buf) {
                        break;
                    }
                }
                if start.elapsed() > Duration::from_millis(1500) {
                    panic!("master timeout waiting for slave response; buf={buf:?}");
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    let (_u, resp_pdu) = framing::parse_rtu(&buf).expect("slave response crc");
    resp_pdu
}

#[test]
fn rtu_serial_slave_answers_master() {
    // 一对互联的双向字节流：slave 端交给从站循环，master 端由测试线程驱动。
    let (slave_sock, mut master_sock) = UnixStream::pair().expect("UnixStream::pair");
    // 两端非阻塞：从站循环靠 read 的 WouldBlock 做帧间定界，主站读同理。
    slave_sock
        .set_nonblocking(true)
        .expect("slave nonblocking");
    master_sock
        .set_nonblocking(true)
        .expect("master nonblocking");

    let mut units_map = UnitImages::new();
    let img = new_shared_image();
    img.lock().unwrap().set_holding(0, 0x1234);
    img.lock().unwrap().set_holding(3, 0x00AB);
    units_map.insert(1, img);
    let units = Arc::new(Mutex::new(units_map));
    let on_write: Arc<dyn Fn(u8) + Send + Sync> = Arc::new(|_: u8| {});
    let stop = Arc::new(AtomicBool::new(false));

    let units_t = units.clone();
    let on_write_t = on_write.clone();
    let stop_t = stop.clone();
    let handle = std::thread::spawn(move || {
        match run_rtu_slave_on_rw(slave_sock, units_t, on_write_t, 5, stop_t) {
            Ok(()) => eprintln!("[slave] exited ok"),
            Err(e) => eprintln!("[slave] ERROR: {e}"),
        }
    });

    // 让从站线程完成端口配置后再发请求。
    std::thread::sleep(Duration::from_millis(300));

    // FC03 读 @0
    let resp = master_request(&mut master_sock, 1, &[0x03, 0x00, 0x00, 0x00, 0x01]);
    eprintln!("[master] FC03 @0 -> {resp:?}");
    assert_eq!(resp, vec![0x03, 0x02, 0x12, 0x34], "FC03 @0 应答应为 0x1234");

    // FC03 读 @3
    let resp = master_request(&mut master_sock, 1, &[0x03, 0x00, 0x03, 0x00, 0x01]);
    eprintln!("[master] FC03 @3 -> {resp:?}");
    assert_eq!(resp, vec![0x03, 0x02, 0x00, 0xAB], "FC03 @3 应答应为 0x00AB");

    // FC06 写 @5 = 0xBEEF，再回读
    master_request(&mut master_sock, 1, &[0x06, 0x00, 0x05, 0xBE, 0xEF]);
    let resp = master_request(&mut master_sock, 1, &[0x03, 0x00, 0x05, 0x00, 0x01]);
    eprintln!("[master] FC03 @5 -> {resp:?}");
    assert_eq!(
        resp,
        vec![0x03, 0x02, 0xBE, 0xEF],
        "FC06 写入后 FC03 回读应为 0xBEEF"
    );

    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}
