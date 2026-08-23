//! Register value formatting and word-order handling (mirrors nmw's Data layer).

/// Byte/word order for 32/64-bit values spanning two 16-bit registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WordOrder {
    /// Most-significant word first (w1=hi, w2=lo), big-endian bytes.
    #[default]
    Abcd,
    /// Word-swapped (w2=hi, w1=lo).
    Cdab,
    /// Bytes within each word swapped.
    Badc,
    /// Least-significant word first, little-endian bytes.
    Dcba,
}

/// Combine two 16-bit registers into a 32-bit float according to `order`.
pub fn words_to_f32(w1: u16, w2: u16, order: WordOrder) -> f32 {
    let b1 = w1.to_be_bytes();
    let b2 = w2.to_be_bytes();
    let bytes: [u8; 4] = match order {
        WordOrder::Abcd => [b1[0], b1[1], b2[0], b2[1]],
        WordOrder::Cdab => [b2[0], b2[1], b1[0], b1[1]],
        WordOrder::Badc => [b1[1], b1[0], b2[0], b2[1]],
        WordOrder::Dcba => [b1[0], b1[1], b2[0], b2[1]],
    };
    match order {
        WordOrder::Dcba => f32::from_le_bytes(bytes),
        _ => f32::from_be_bytes(bytes),
    }
}

/// Reinterpret a u16 as signed.
pub fn u16_to_i16(v: u16) -> i16 {
    v as i16
}

/// Format a single u16 for display given a representation.
pub fn format_u16(v: u16, repr: &str) -> String {
    match repr.to_ascii_lowercase().as_str() {
        "hex" => format!("0x{:04X}", v),
        "bin" => format!("{:016b}", v),
        "s16" => format!("{}", v as i16),
        _ => format!("{}", v), // decimal unsigned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_abcd_one() {
        // 1.0f32 == 0x3F800000, ABCD => w1=0x3F80, w2=0x0000
        let v = words_to_f32(0x3F80, 0x0000, WordOrder::Abcd);
        assert!((v - 1.0).abs() < 1e-6);
    }

    #[test]
    fn f32_cdab_one() {
        // CDAB => w1=0x0000, w2=0x3F80
        let v = words_to_f32(0x0000, 0x3F80, WordOrder::Cdab);
        assert!((v - 1.0).abs() < 1e-6);
    }

    #[test]
    fn f32_badc_one() {
        // BADC => w1=0x803F, w2=0x0000
        let v = words_to_f32(0x803F, 0x0000, WordOrder::Badc);
        assert!((v - 1.0).abs() < 1e-6);
    }

    #[test]
    fn f32_dcba_one() {
        // DCBA => w1=0x0000, w2=0x803F
        let v = words_to_f32(0x0000, 0x803F, WordOrder::Dcba);
        assert!((v - 1.0).abs() < 1e-6);
    }

    #[test]
    fn format_variants() {
        assert_eq!(format_u16(0x00FF, "hex"), "0x00FF");
        assert_eq!(format_u16(0xFFFF, "s16"), "-1");
        assert_eq!(format_u16(5, "dec"), "5");
    }
}
