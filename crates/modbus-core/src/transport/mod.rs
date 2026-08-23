//! Transport layer: byte pipes for Modbus TCP/UDP and (feature `rtu`) serial RTU.

pub mod tcp;
pub mod udp;

#[cfg(feature = "rtu")]
pub mod rtu;
