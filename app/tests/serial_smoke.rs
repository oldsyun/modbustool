//! Unit tests for the serial terminal service that don't need real hardware.

use std::sync::Arc;

use modbus_tool_gpui::core::service::serial::SerialConfig;
use modbus_tool_gpui::core::service::Service;

#[test]
fn config_defaults() {
    let c = SerialConfig::default();
    assert_eq!(c.data_bits, 8);
    assert_eq!(c.stop_bits, 1);
    assert_eq!(c.parity, "none");
    assert_eq!(c.flow, "none");
}

#[tokio::test]
async fn send_without_connection_errors() {
    let svc = Service::new();
    let r = svc.serial_send(vec![0x01, 0x02]).await;
    assert!(r.is_err(), "send without open should fail");
}

#[tokio::test]
async fn status_when_closed() {
    let svc = Service::new();
    let st = svc.serial_status();
    assert!(!st.open);
    assert_eq!(st.tx_bytes, 0);
    assert_eq!(st.rx_bytes, 0);
}

#[tokio::test]
async fn open_nonexistent_port_fails_gracefully() {
    let svc: Arc<Service> = Service::new();
    let r = svc
        .serial_open(SerialConfig {
            port: "/tmp/definitely-not-a-serial-device".into(),
            ..SerialConfig::default()
        })
        .await;
    assert!(r.is_err(), "opening a nonexistent device must fail");
    assert!(!svc.serial_status().open);
}

#[tokio::test]
async fn serial_mutually_exclusive_with_modbus_connection() {
    // Start the built-in Modbus TCP slave and connect the master so the
    // service reports a live Modbus connection; serial_open must then refuse.
    use modbus_tool_gpui::core::service::SlaveStartReq;
    let svc: Arc<Service> = Service::new();
    svc.sim_slave_start(SlaveStartReq {
        mode: "tcp".into(),
        bind: Some("127.0.0.1".into()),
        port: Some(15021),
        port_name: None,
        baud_rate: None,
        data_bits: None,
        stop_bits: None,
        parity: None,
        inter_frame_ms: None,
    })
    .await
    .expect("start slave");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    svc.connect("127.0.0.1".into(), 15021, 800, 1, None)
        .await
        .expect("connect modbus");

    let r = svc
        .serial_open(SerialConfig {
            port: "/tmp/whatever".into(),
            ..SerialConfig::default()
        })
        .await;
    assert!(r.is_err(), "serial must be blocked while Modbus is connected");

    svc.disconnect().await.ok();
    svc.sim_slave_stop_all().await.ok();
}
