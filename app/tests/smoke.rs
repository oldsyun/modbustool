//! End-to-end smoke test of the service layer: start the built-in TCP slave,
//! connect the master to it, write a holding register via the slave image, and
//! read it back through the master to prove the whole stack works headlessly.

use std::sync::Arc;
use std::time::Duration;

use modbus_tool_gpui::core::service::{Service, SlaveStartReq};

#[tokio::test]
async fn slave_master_roundtrip() {
    let svc: Arc<Service> = Service::new();

    svc.sim_slave_start(SlaveStartReq {
        mode: "tcp".into(),
        bind: Some("127.0.0.1".into()),
        port: Some(15020),
        port_name: None,
        baud_rate: None,
        data_bits: None,
        stop_bits: None,
        parity: None,
        inter_frame_ms: None,
    })
    .await
    .expect("start slave");

    tokio::time::sleep(Duration::from_millis(200)).await;

    svc.sim_set_register(7, 0x1234, Some(1)).await.expect("set reg");

    svc.connect("127.0.0.1".into(), 15020, 1000, 2, None)
        .await
        .expect("connect master");

    let res = svc.read_points(1, "03".into(), 7, 1).await.expect("read");
    assert_eq!(res.regs, vec![0x1234], "roundtrip value mismatch");

    svc.write_point(1, "06".into(), 8, vec![0xABCD]).await.expect("write");
    let res2 = svc.read_points(1, "03".into(), 8, 1).await.expect("read2");
    assert_eq!(res2.regs, vec![0xABCD], "write/read mismatch");

    svc.disconnect().await.ok();
    svc.sim_slave_stop_all().await.ok();
}
