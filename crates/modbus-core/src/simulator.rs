//! Built-in device simulator: an auto-vary generator that drives a shared
//! [`DataImage`] so the master (and the built-in slave) can be exercised with
//! zero hardware. The same image backs both the Modbus TCP slave and the
//! Modbus RTU serial slave, so values written from any side are visible
//! everywhere (数据共享).

use crate::slave::DataImage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaryMode {
    Off,
    Sine,
    Random,
    Increment,
}

impl VaryMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "sine" => VaryMode::Sine,
            "random" => VaryMode::Random,
            "increment" => VaryMode::Increment,
            _ => VaryMode::Off,
        }
    }
}

/// The simulator generator: only holds the auto-vary tick counter.
/// The actual registers live in a [`DataImage`] shared with the slave servers,
/// and each register carries its own vary mode (per-register settings).
pub struct Simulator {
    pub tick: u64,
}

impl Simulator {
    pub fn new() -> Self {
        Self { tick: 0 }
    }

    /// Advance the auto-vary generator by one step.
    ///
    /// `holding_varies` / `input_varies` are the (address, vary-mode) pairs
    /// assembled from the per-register settings of the active unit; holding
    /// registers and input registers vary independently (input registers are
    /// read-only for the master, but the simulator may still drive them).
    /// Only addresses in these lists are touched.
    pub fn step_on(
        &mut self,
        img: &mut DataImage,
        holding_varies: &[(u16, VaryMode)],
        input_varies: &[(u16, VaryMode)],
    ) {
        self.tick = self.tick.wrapping_add(1);
        for &(addr, mode) in holding_varies {
            match mode {
                VaryMode::Off => {}
                VaryMode::Sine => {
                    let i = addr as usize;
                    let phase = (self.tick as f64 / 20.0) + (i as f64 * 0.3);
                    let v = ((phase.sin() * 0.5 + 0.5) * 65535.0) as u16;
                    img.holding_registers.insert(addr, v);
                }
                VaryMode::Random => {
                    let cur = img.holding_registers.get(&addr).copied().unwrap_or(0);
                    let v =
                        fast_rand(self.tick.wrapping_mul(2654435761).wrapping_add(cur as u64)) as u16;
                    img.holding_registers.insert(addr, v);
                }
                VaryMode::Increment => {
                    let cur = img.holding_registers.get(&addr).copied().unwrap_or(0);
                    img.holding_registers.insert(addr, cur.wrapping_add(1));
                }
            }
        }
        for &(addr, mode) in input_varies {
            match mode {
                VaryMode::Off => {}
                VaryMode::Sine => {
                    let i = addr as usize;
                    let phase = (self.tick as f64 / 20.0) + (i as f64 * 0.3);
                    let v = ((phase.sin() * 0.5 + 0.5) * 65535.0) as u16;
                    img.input_registers.insert(addr, v);
                }
                VaryMode::Random => {
                    let cur = img.input_registers.get(&addr).copied().unwrap_or(0);
                    let v =
                        fast_rand(self.tick.wrapping_mul(2654435761).wrapping_add(cur as u64)) as u16;
                    img.input_registers.insert(addr, v);
                }
                VaryMode::Increment => {
                    let cur = img.input_registers.get(&addr).copied().unwrap_or(0);
                    img.input_registers.insert(addr, cur.wrapping_add(1));
                }
            }
        }
    }
}

impl Default for Simulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Tiny deterministic PRNG (xorshift) — no external dependency.
fn fast_rand(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vary_changes_values_per_register() {
        let mut sim = Simulator::new();
        let mut img = DataImage::new(0, 0, 0, 4);
        img.holding_registers.insert(0, 0);
        img.holding_registers.insert(1, 100);
        img.input_registers.insert(0, 1000);
        // 每个寄存器独立设置：0 自增、1 关闭（保持）；输入寄存器 0 自增
        let holding_varies = [
            (0, VaryMode::Increment),
            (1, VaryMode::Off),
        ];
        let input_varies = [(0, VaryMode::Increment)];
        sim.step_on(&mut img, &holding_varies, &input_varies);
        assert_eq!(img.holding_registers.get(&0), Some(&1));
        assert_eq!(img.holding_registers.get(&1), Some(&100)); // Off 不变
        assert_eq!(img.input_registers.get(&0), Some(&1001));
        sim.step_on(&mut img, &holding_varies, &input_varies);
        assert_eq!(img.holding_registers.get(&0), Some(&2));
        assert_eq!(img.holding_registers.get(&1), Some(&100));
        assert_eq!(img.input_registers.get(&0), Some(&1002));
    }

    #[test]
    fn handles_read() {
        let mut img = DataImage::new(0, 0, 0, 10);
        let resp = img.handle_request(&[0x03, 0x00, 0x00, 0x00, 0x01]).unwrap();
        assert_eq!(resp[0], 0x03);
        assert_eq!(resp[1], 2);
    }
}
