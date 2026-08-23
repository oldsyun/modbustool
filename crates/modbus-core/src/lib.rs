pub mod client;
pub mod crc;
pub mod data;
pub mod error;
pub mod framing;
pub mod function_code;
pub mod server;
pub mod simulator;
pub mod slave;
pub mod transport;
pub mod workspace;

/// One round-trip on the wire: (TX ADU, RX ADU, RTT, response PDU).
/// Returned by transports' `request_frame` so the UI can show the full
/// traffic while still receiving the decoded payload.
pub type Frame = (
    Vec<u8>,
    Vec<u8>,
    std::time::Duration,
    Vec<u8>,
);
