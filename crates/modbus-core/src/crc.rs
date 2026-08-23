//! Modbus CRC-16 (poly 0x8005, init 0xFFFF, reflected, no final xor).
//! Used by RTU framing. The register value is transmitted low-byte first.

/// Compute the CRC-16/MODBUS of `data`.
pub fn crc16_modbus(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 0x0001 != 0 {
                crc >>= 1;
                crc ^= 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// CRC as the two on-wire bytes (low byte first), as appended to an RTU frame.
pub fn crc16_modbus_bytes(data: &[u8]) -> [u8; 2] {
    let c = crc16_modbus(data);
    [(c & 0xFF) as u8, (c >> 8) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // Classic reference vector used across Modbus tooling.
        let frame = [0x01, 0x03, 0x00, 0x00, 0x00, 0x01];
        assert_eq!(crc16_modbus(&frame), 0x0A84);
        assert_eq!(crc16_modbus_bytes(&frame), [0x84, 0x0A]);
    }

    #[test]
    fn empty_is_ffff() {
        assert_eq!(crc16_modbus(&[]), 0xFFFF);
    }

    #[test]
    fn roundtrip_property() {
        let data = b"Modbus Workbench";
        let crc = crc16_modbus_bytes(data);
        let mut framed = data.to_vec();
        framed.extend_from_slice(&crc);
        // Recompute over the framed body (excluding the appended CRC) -> same.
        assert_eq!(crc16_modbus_bytes(data), crc);
        // Flipping a byte breaks it.
        let mut bad = framed.clone();
        bad[0] ^= 0xFF;
        assert_ne!(crc16_modbus_bytes(&bad[..bad.len() - 2]), [bad[bad.len() - 2], bad[bad.len() - 1]]);
    }
}
