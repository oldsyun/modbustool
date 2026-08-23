//! Modbus function codes (PDU function field), per the Modbus Application
//! Protocol Specification v1.1b. Mirrors the constants nmw keeps in its core.

/// Read Coils (FC01)
pub const READ_COILS: u8 = 0x01;
/// Read Discrete Inputs (FC02)
pub const READ_DISCRETE_INPUTS: u8 = 0x02;
/// Read Holding Registers (FC03)
pub const READ_HOLDING_REGISTERS: u8 = 0x03;
/// Read Input Registers (FC04)
pub const READ_INPUT_REGISTERS: u8 = 0x04;
/// Write Single Coil (FC05)
pub const WRITE_SINGLE_COIL: u8 = 0x05;
/// Write Single Register (FC06)
pub const WRITE_SINGLE_REGISTER: u8 = 0x06;
/// Write Multiple Coils (FC15)
pub const WRITE_MULTIPLE_COILS: u8 = 0x0F;
/// Write Multiple Registers (FC16)
pub const WRITE_MULTIPLE_REGISTERS: u8 = 0x10;

/// Mask Write Register (FC22) — occasionally used; listed for completeness.
pub const MASK_WRITE_REGISTER: u8 = 0x16;

/// Whether a function code is a standard read function (FC01..FC04).
pub fn is_read(fc: u8) -> bool {
    matches!(
        fc,
        READ_COILS | READ_DISCRETE_INPUTS | READ_HOLDING_REGISTERS | READ_INPUT_REGISTERS
    )
}

/// Whether a function code is a write function.
pub fn is_write(fc: u8) -> bool {
    matches!(
        fc,
        WRITE_SINGLE_COIL
            | WRITE_SINGLE_REGISTER
            | WRITE_MULTIPLE_COILS
            | WRITE_MULTIPLE_REGISTERS
            | MASK_WRITE_REGISTER
    )
}
