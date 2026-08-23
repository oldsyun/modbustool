//! Regression test for the RTU master read loop (the "first single-send times
//! out, retry succeeds" bug).
//!
//! The read loop must wait the FULL transaction `timeout` for the first
//! response byte. Seeding the inter-frame timer with the start time made a
//! single ~20ms empty read look like a completed frame gap, so the loop bailed
//! early — any slave whose first response exceeded one read window got a false
//! "no response". We reproduce it over a `UnixStream` (no real serial port;
//! macOS pseudo-terminals reject serial2's baud-rate ioctl).

#![cfg(all(unix, feature = "rtu"))]

use modbus_core::framing;
use modbus_core::transport::rtu::exchange_rtu;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Spawns a slave on `slave_end` that, on the Nth transaction, delays
/// `delay` before echoing a valid FC03 response.
fn spawn_slave(slave_end: UnixStream, delay: Duration, first_only: bool) {
    std::thread::spawn(move || {
        let mut s = slave_end;
        let mut req = [0u8; 256];
        let mut count: usize = 0;
        loop {
            let _n = match s.read(&mut req) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            count += 1;
            if !first_only || count == 1 {
                std::thread::sleep(delay);
            }
            // Build a valid FC03 response: byte count 0x14 (20) + 10 registers.
            let mut resp_pdu = vec![0x03u8, 0x14];
            for i in 0..10u16 {
                resp_pdu.extend_from_slice(&i.to_be_bytes());
            }
            let adu = framing::encode_rtu(1, &resp_pdu);
            let _ = s.write_all(&adu);
            let _ = s.flush();
        }
    });
}

#[test]
fn master_waits_full_timeout_for_slow_first_response() {
    let (master, slave) = UnixStream::pair().unwrap();
    // Mimic the production read_probe (≤20ms) so the test exercises the exact
    // timing the bug depended on.
    master.set_read_timeout(Some(Duration::from_millis(20))).unwrap();

    // First transaction's slave is 100ms late — far beyond one 20ms read window.
    spawn_slave(slave, Duration::from_millis(100), true);

    let pdu = [0x03u8, 0x00, 0x00, 0x00, 0x0A]; // FC03 @0 ×10
    let t0 = Instant::now();
    let mut m = master;
    let frame = exchange_rtu(
        &mut m,
        1,
        &pdu,
        Duration::from_millis(5),
        Duration::from_millis(1000),
    )
    .expect("master should wait past the 20ms read window for a slow first response");
    let elapsed = t0.elapsed();

    // Response must be the valid FC03 frame we built.
    assert_eq!(frame.3[0], 0x03);
    assert_eq!(frame.3.len(), 2 + 20);
    // The round trip must have taken ~100ms (slave delay), proving the master
    // did NOT bail at 20ms.
    assert!(
        elapsed >= Duration::from_millis(90),
        "RTT too short ({elapsed:?}) — master did not wait for the slow response"
    );
    assert!(elapsed < Duration::from_millis(400));
}

#[test]
fn master_reports_no_response_when_slave_silent() {
    let (master, _slave) = UnixStream::pair().unwrap();
    master.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    // Slave side is dropped immediately -> no bytes ever arrive.

    let pdu = [0x03u8, 0x00, 0x00, 0x00, 0x0A];
    let t0 = Instant::now();
    let mut m = master;
    let err = exchange_rtu(
        &mut m,
        1,
        &pdu,
        Duration::from_millis(5),
        Duration::from_millis(300),
    )
    .err()
    .expect("silent slave must yield an error");
    let elapsed = t0.elapsed();

    assert!(format!("{err}").contains("no response"), "unexpected error: {err}");
    // Should give up near the timeout, not immediately.
    assert!(
        elapsed >= Duration::from_millis(250),
        "gave up too early ({elapsed:?})"
    );
}
