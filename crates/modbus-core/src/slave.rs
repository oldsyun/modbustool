//! Slave (server) data image and request handler — the engine behind the
//! built-in simulator. Same struct can back a TCP slave or an RTU slave.
//!
//! **Sparse on-demand model**: tables start empty and grow only when a
//! register/coil is written (by the simulator UI or by a master). No memory
//! is pre-allocated for "default registers" — a register exists only once it
//! has been written, and reads of addresses that were never written return
//! zero (implicit zero). Memory therefore stays proportional to the number of
//! registers actually in use, instead of a fixed 1000-register array.

use crate::error::exception;
use crate::function_code as fc;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct DataImage {
    pub coils: HashMap<u16, bool>,
    pub discrete_inputs: HashMap<u16, bool>,
    pub input_registers: HashMap<u16, u16>,
    pub holding_registers: HashMap<u16, u16>,
}

/// Clamp an f64 value to the u16 register range (used when the UI writes a
/// value that may be signed / float).
fn clamp_u16(v: f64) -> u16 {
    if v.is_nan() {
        return 0;
    }
    if v <= 0.0 {
        return 0;
    }
    if v >= 65535.0 {
        return 65535;
    }
    v.round() as u16
}

impl DataImage {
    /// Create an empty (sparse) image. The size arguments are accepted for
    /// API compatibility but **ignored** — nothing is pre-allocated.
    pub fn new(_coils: usize, _discrete: usize, _inputs: usize, _holdings: usize) -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.coils.clear();
        self.discrete_inputs.clear();
        self.input_registers.clear();
        self.holding_registers.clear();
    }

    /// Handle a request PDU and return a response PDU, or `Err(exception_code)`.
    /// Writes actually mutate the store (FC05/FC06/FC15/FC16).
    pub fn handle_request(&mut self, pdu: &[u8]) -> Result<Vec<u8>, u8> {
        if pdu.is_empty() {
            return Err(exception::ILLEGAL_FUNCTION);
        }
        let code = pdu[0];
        match code {
            fc::READ_HOLDING_REGISTERS | fc::READ_INPUT_REGISTERS => self.read_registers(code, pdu),
            fc::READ_COILS | fc::READ_DISCRETE_INPUTS => self.read_bits(code, pdu),
            fc::WRITE_SINGLE_COIL => self.write_single_coil(pdu),
            fc::WRITE_SINGLE_REGISTER => self.write_single_register(pdu),
            fc::WRITE_MULTIPLE_COILS => self.write_multiple_coils(pdu),
            fc::WRITE_MULTIPLE_REGISTERS => self.write_multiple_registers(pdu),
            _ => Err(exception::ILLEGAL_FUNCTION),
        }
    }

    /// Read holding/input registers. Unregistered addresses read as implicit
    /// zero; a range that crosses the 16-bit address space is illegal.
    fn read_registers(&self, code: u8, pdu: &[u8]) -> Result<Vec<u8>, u8> {
        if pdu.len() < 5 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
        let count = u16::from_be_bytes([pdu[3], pdu[4]]) as usize;
        if count == 0 || count > 125 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        if addr as usize + count > 0x10000 {
            return Err(exception::ILLEGAL_DATA_ADDRESS);
        }
        let store = if code == fc::READ_HOLDING_REGISTERS {
            &self.holding_registers
        } else {
            &self.input_registers
        };
        let mut resp = vec![code, (count * 2) as u8];
        for i in 0..count {
            let v = store.get(&(addr + i as u16)).copied().unwrap_or(0xFFFF);
            resp.extend_from_slice(&v.to_be_bytes());
        }
        Ok(resp)
    }

    /// Read coils / discrete inputs. Same implicit-zero semantics.
    fn read_bits(&self, code: u8, pdu: &[u8]) -> Result<Vec<u8>, u8> {
        if pdu.len() < 5 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
        let count = u16::from_be_bytes([pdu[3], pdu[4]]) as usize;
        if count == 0 || count > 2000 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        if addr as usize + count > 0x10000 {
            return Err(exception::ILLEGAL_DATA_ADDRESS);
        }
        let store = if code == fc::READ_COILS {
            &self.coils
        } else {
            &self.discrete_inputs
        };
        let bytes = count.div_ceil(8);
        let mut resp = vec![code, bytes as u8];
        resp.extend(vec![0u8; bytes]);
        for i in 0..count {
            if store.get(&(addr + i as u16)).copied().unwrap_or(false) {
                resp[2 + i / 8] |= 1 << (i % 8);
            }
        }
        Ok(resp)
    }

    fn write_single_coil(&mut self, pdu: &[u8]) -> Result<Vec<u8>, u8> {
        if pdu.len() < 5 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
        let raw = u16::from_be_bytes([pdu[3], pdu[4]]);
        let on = match raw {
            0xFF00 => true,
            0x0000 => false,
            _ => return Err(exception::ILLEGAL_DATA_VALUE),
        };
        self.coils.insert(addr, on);
        // Echo the request (per spec FC05 response == request).
        Ok(pdu.to_vec())
    }

    fn write_single_register(&mut self, pdu: &[u8]) -> Result<Vec<u8>, u8> {
        if pdu.len() < 5 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
        let val = u16::from_be_bytes([pdu[3], pdu[4]]);
        self.holding_registers.insert(addr, val);
        // Echo the request (per spec FC06 response == request).
        Ok(pdu.to_vec())
    }

    fn write_multiple_coils(&mut self, pdu: &[u8]) -> Result<Vec<u8>, u8> {
        if pdu.len() < 6 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
        let count = u16::from_be_bytes([pdu[3], pdu[4]]) as usize;
        if count == 0 || count > 1968 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let byte_count = pdu[5] as usize;
        if addr as usize + count > 0x10000 {
            return Err(exception::ILLEGAL_DATA_ADDRESS);
        }
        // Sanity: byte count must cover the bits.
        if byte_count != count.div_ceil(8) || pdu.len() < 6 + byte_count {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let data = &pdu[6..6 + byte_count];
        for i in 0..count {
            let bit = (data[i / 8] >> (i % 8)) & 0x01 == 1;
            self.coils.insert(addr + i as u16, bit);
        }
        // Response: [0x0F, addr_hi, addr_lo, qty_hi, qty_lo]
        Ok(vec![fc::WRITE_MULTIPLE_COILS, pdu[1], pdu[2], pdu[3], pdu[4]])
    }

    fn write_multiple_registers(&mut self, pdu: &[u8]) -> Result<Vec<u8>, u8> {
        if pdu.len() < 6 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let addr = u16::from_be_bytes([pdu[1], pdu[2]]);
        let count = u16::from_be_bytes([pdu[3], pdu[4]]) as usize;
        if count == 0 || count > 123 {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let byte_count = pdu[5] as usize;
        if addr as usize + count > 0x10000 {
            return Err(exception::ILLEGAL_DATA_ADDRESS);
        }
        if byte_count != count * 2 || pdu.len() < 6 + byte_count {
            return Err(exception::ILLEGAL_DATA_VALUE);
        }
        let data = &pdu[6..6 + byte_count];
        for i in 0..count {
            let v = u16::from_be_bytes([data[i * 2], data[i * 2 + 1]]);
            self.holding_registers.insert(addr + i as u16, v);
        }
        // Response: [0x10, addr_hi, addr_lo, qty_hi, qty_lo]
        Ok(vec![fc::WRITE_MULTIPLE_REGISTERS, pdu[1], pdu[2], pdu[3], pdu[4]])
    }

    // ── UI / external editing helpers ───────────────────────────────

    /// Set a single holding register (creates the slot on demand).
    pub fn set_holding(&mut self, addr: usize, val: u16) {
        self.holding_registers.insert(addr as u16, val);
    }

    /// Set a single coil (creates the slot on demand).
    pub fn set_coil(&mut self, addr: usize, on: bool) {
        self.coils.insert(addr as u16, on);
    }

    /// Set a single input register (read-only area, but the simulator UI can
    /// still seed values so a master can read realistic input data).
    pub fn set_input(&mut self, addr: usize, val: u16) {
        self.input_registers.insert(addr as u16, val);
    }

    /// Set a single discrete input (read-only area, simulator-seeded).
    pub fn set_discrete(&mut self, addr: usize, on: bool) {
        self.discrete_inputs.insert(addr as u16, on);
    }

    /// Write a value into the given area slot (used by the register registry).
    pub fn write_slot(&mut self, area: &str, addr: u16, value: f64) {
        match area {
            "holding" => {
                self.holding_registers.insert(addr, clamp_u16(value));
            }
            "input" => {
                self.input_registers.insert(addr, clamp_u16(value));
            }
            "coil" => {
                self.coils.insert(addr, value != 0.0);
            }
            "discrete" => {
                self.discrete_inputs.insert(addr, value != 0.0);
            }
            _ => {}
        }
    }

    /// Remove a slot from the given area (used when a register def is deleted
    /// or moved). Reading it afterwards returns implicit zero.
    pub fn clear_slot(&mut self, area: &str, addr: u16) {
        match area {
            "holding" => {
                self.holding_registers.remove(&addr);
            }
            "input" => {
                self.input_registers.remove(&addr);
            }
            "coil" => {
                self.coils.remove(&addr);
            }
            "discrete" => {
                self.discrete_inputs.remove(&addr);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_holding_implicit_zero_and_oob() {
        let mut img = DataImage::new(0, 0, 0, 1000);
        let resp = img
            .handle_request(&[0x03, 0x00, 0x00, 0x00, 0x02])
            .unwrap();
        assert_eq!(resp[0], 0x03);
        assert_eq!(resp[1], 4); // 2 regs * 2 bytes
        assert_eq!(&resp[2..], &[0xFF, 0xFF, 0xFF, 0xFF]); // unregistered → implicit 0xFFFF
        // a range that crosses the 16-bit address space is illegal
        assert_eq!(
            img.handle_request(&[0x03, 0xFF, 0xFF, 0x00, 0x02]),
            Err(exception::ILLEGAL_DATA_ADDRESS)
        );
    }

    #[test]
    fn full_write_then_read_roundtrip() {
        let mut img = DataImage::new(16, 16, 8, 16);
        // FC06 write holding @5 = 0x1234
        let r = img.handle_request(&[0x06, 0x00, 0x05, 0x12, 0x34]).unwrap();
        assert_eq!(r, vec![0x06, 0x00, 0x05, 0x12, 0x34]);
        let got = img.handle_request(&[0x03, 0x00, 0x05, 0x00, 0x01]).unwrap();
        assert_eq!(got, vec![0x03, 0x02, 0x12, 0x34]);

        // FC16 write multiple holdings @0..2 = [10,20,30]
        let r = img
            .handle_request(&[0x10, 0x00, 0x00, 0x00, 0x03, 0x06, 0x00, 0x0A, 0x00, 0x14, 0x00, 0x1E])
            .unwrap();
        assert_eq!(r, vec![0x10, 0x00, 0x00, 0x00, 0x03]);
        let got = img.handle_request(&[0x03, 0x00, 0x00, 0x00, 0x03]).unwrap();
        assert_eq!(got, vec![0x03, 0x06, 0x00, 0x0A, 0x00, 0x14, 0x00, 0x1E]);

        // FC05 write coil @3 ON
        let r = img.handle_request(&[0x05, 0x00, 0x03, 0xFF, 0x00]).unwrap();
        assert_eq!(r, vec![0x05, 0x00, 0x03, 0xFF, 0x00]);
        let got = img.handle_request(&[0x01, 0x00, 0x00, 0x00, 0x08]).unwrap();
        assert_eq!(got[0], 0x01);
        // bit 3 set in byte 0
        assert_eq!(got[2] & 0x08, 0x08);

        // FC15 write multiple coils @0..4 = [1,0,1,1,0]
        let r = img
            .handle_request(&[0x0F, 0x00, 0x00, 0x00, 0x04, 0x01, 0x0D])
            .unwrap();
        assert_eq!(r, vec![0x0F, 0x00, 0x00, 0x00, 0x04]);

        // illegal function
        assert_eq!(
            img.handle_request(&[0x07, 0x00, 0x00, 0x00, 0x01]),
            Err(exception::ILLEGAL_FUNCTION)
        );
    }

    #[test]
    fn write_slot_and_clear_slot() {
        let mut img = DataImage::default();
        img.write_slot("holding", 0x0100, -5.0); // clamps to 0
        img.write_slot("holding", 0x0101, 70000.0); // clamps to 65535
        img.write_slot("coil", 3, 1.0);
        assert_eq!(img.holding_registers.get(&0x0100), Some(&0));
        assert_eq!(img.holding_registers.get(&0x0101), Some(&65535));
        assert_eq!(img.coils.get(&3), Some(&true));
        img.clear_slot("holding", 0x0100);
        assert_eq!(img.holding_registers.get(&0x0100), None);
        // clearing an unregistered slot is a no-op
        img.clear_slot("input", 99);
    }
}
