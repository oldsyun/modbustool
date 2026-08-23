use thiserror::Error;

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("Invalid CRC")]
    InvalidCrc,
    #[error("Incomplete frame")]
    IncompleteFrame,
    #[error("Invalid MBAP Header")]
    InvalidMbap,
}

/// Calculate Modbus CRC16
pub fn calc_crc(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if (crc & 0x0001) != 0 {
                crc >>= 1;
                crc ^= 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Append CRC to a mutable buffer
pub fn append_crc(buf: &mut Vec<u8>) {
    let crc = calc_crc(buf);
    buf.push((crc & 0xFF) as u8);        // Low byte
    buf.push((crc >> 8) as u8);          // High byte
}

/// Verify if a buffer has a valid CRC
pub fn verify_crc(data: &[u8]) -> bool {
    if data.len() < 2 {
        return false;
    }
    let expected = calc_crc(&data[..data.len() - 2]);
    let actual = (data[data.len() - 2] as u16) | ((data[data.len() - 1] as u16) << 8);
    expected == actual
}

/// MBAP Header used for Modbus TCP
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MbapHeader {
    pub transaction_id: u16,
    pub protocol_id: u16,
    pub length: u16,
    pub unit_id: u8,
}

impl MbapHeader {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.transaction_id.to_be_bytes());
        out.extend_from_slice(&self.protocol_id.to_be_bytes());
        out.extend_from_slice(&self.length.to_be_bytes());
        out.push(self.unit_id);
    }

    pub fn decode(data: &[u8]) -> Result<Self, FramingError> {
        if data.len() < 7 {
            return Err(FramingError::IncompleteFrame);
        }
        let transaction_id = u16::from_be_bytes([data[0], data[1]]);
        let protocol_id = u16::from_be_bytes([data[2], data[3]]);
        let length = u16::from_be_bytes([data[4], data[5]]);
        let unit_id = data[6];

        Ok(Self {
            transaction_id,
            protocol_id,
            length,
            unit_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc() {
        let msg = [0x01, 0x03, 0x00, 0x00, 0x00, 0x02];
        let crc = calc_crc(&msg);
        assert_eq!(crc, 0x0BC4); // CRC of 01 03 00 00 00 02 is C4 0B

        let mut msg_vec = msg.to_vec();
        append_crc(&mut msg_vec);
        assert_eq!(msg_vec, [0x01, 0x03, 0x00, 0x00, 0x00, 0x02, 0xC4, 0x0B]);
        assert!(verify_crc(&msg_vec));
    }
}

/// Build a Modbus TCP ADU: MBAP header (with `unit_id` and `transaction_id`)
/// followed by the raw PDU. The MBAP `length` field covers `unit_id + pdu`.
pub fn encode_tcp(unit_id: u8, transaction_id: u16, pdu: &[u8]) -> Vec<u8> {
    let length = (1 + pdu.len()) as u16;
    let mut out = Vec::with_capacity(7 + pdu.len());
    out.extend_from_slice(&transaction_id.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // protocol id (always 0)
    out.extend_from_slice(&length.to_be_bytes());
    out.push(unit_id);
    out.extend_from_slice(pdu);
    out
}

/// Parse a Modbus TCP ADU into `(transaction_id, unit_id, pdu)`.
pub fn parse_tcp(adu: &[u8]) -> Result<(u16, u8, Vec<u8>), FramingError> {
    if adu.len() < 7 {
        return Err(FramingError::IncompleteFrame);
    }
    let transaction_id = u16::from_be_bytes([adu[0], adu[1]]);
    let protocol_id = u16::from_be_bytes([adu[2], adu[3]]);
    if protocol_id != 0 {
        return Err(FramingError::InvalidMbap);
    }
    let length = u16::from_be_bytes([adu[4], adu[5]]) as usize;
    let unit_id = adu[6];
    let expected_total = 6 + length;
    if adu.len() < expected_total {
        return Err(FramingError::IncompleteFrame);
    }
    let pdu = adu[7..expected_total].to_vec();
    Ok((transaction_id, unit_id, pdu))
}

/// Build a Modbus RTU ADU: address field + PDU + CRC16 (low byte then high).
pub fn encode_rtu(unit_id: u8, pdu: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + pdu.len() + 2);
    out.push(unit_id);
    out.extend_from_slice(pdu);
    append_crc(&mut out);
    out
}

/// Parse a Modbus RTU ADU into `(unit_id, pdu)` after validating the CRC.
pub fn parse_rtu(adu: &[u8]) -> Result<(u8, Vec<u8>), FramingError> {
    if adu.len() < 5 {
        return Err(FramingError::IncompleteFrame);
    }
    if !verify_crc(adu) {
        return Err(FramingError::InvalidCrc);
    }
    let unit_id = adu[0];
    let pdu = adu[1..adu.len() - 2].to_vec();
    Ok((unit_id, pdu))
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn tcp_roundtrip() {
        let pdu = [0x03u8, 0x00, 0x00, 0x00, 0x0A];
        let adu = encode_tcp(1, 0x1234, &pdu);
        // 7-byte MBAP + 5-byte PDU
        assert_eq!(adu.len(), 12);
        let (tid, unit, back) = parse_tcp(&adu).unwrap();
        assert_eq!(tid, 0x1234);
        assert_eq!(unit, 1);
        assert_eq!(back, pdu);
    }

    #[test]
    fn rtu_roundtrip() {
        let pdu = [0x03u8, 0x00, 0x00, 0x00, 0x0A];
        let adu = encode_rtu(1, &pdu);
        // 1 + 5 + 2 (CRC) = 8
        assert_eq!(adu.len(), 8);
        let (unit, back) = parse_rtu(&adu).unwrap();
        assert_eq!(unit, 1);
        assert_eq!(back, pdu);
    }

    #[test]
    fn rtu_bad_crc() {
        let mut adu = encode_rtu(1, &[0x03, 0x00, 0x00, 0x00, 0x0A]);
        adu[7] ^= 0xFF; // corrupt CRC
        assert!(parse_rtu(&adu).is_err());
    }
}

