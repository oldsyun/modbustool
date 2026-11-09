//! Register registry for the simulator: per-Unit register definitions.
//! Direct port of the old `simreg.rs`.

use modbus_core::server::ImageSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn gen_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = DefaultHasher::new();
    nanos.hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegisterDef {
    pub id: String,
    pub area: String,
    pub addr: u16,
    pub name: String,
    pub dtype: String,
    pub access: String,
    pub vary: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RegInput {
    pub area: String,
    pub addr: u16,
    pub name: String,
    pub dtype: String,
    pub access: String,
    pub vary: String,
    pub value: f64,
}

#[derive(Clone, Serialize)]
pub struct RegListResp {
    pub unit_id: u8,
    pub defs: Vec<RegisterDef>,
    pub snapshot: ImageSnapshot,
}

pub struct RegSample {
    pub def: RegisterDef,
    pub value: f64,
}

pub fn area_label(area: &str) -> &'static str {
    match area {
        "holding" => "保持寄存器",
        "input" => "输入寄存器",
        "coil" => "线圈",
        "discrete" => "离散输入",
        _ => "未知区域",
    }
}

pub fn default_reg_samples() -> Vec<RegSample> {
    let mut seq = 0u64;
    let mut mk = |area: &str,
                  addr: u16,
                  name: &str,
                  dtype: &str,
                  access: &str,
                  vary: &str,
                  value: f64| {
        seq += 1;
        RegSample {
            def: RegisterDef {
                id: format!("seed_{seq}"),
                area: area.to_string(),
                addr,
                name: name.to_string(),
                dtype: dtype.to_string(),
                access: access.to_string(),
                vary: vary.to_string(),
                created_at_ms: 0,
            },
            value,
        }
    };
    vec![
        mk("holding", 0, "电压", "u16", "rw", "sine", 220.0),
        mk("holding", 1, "电流", "u16", "rw", "increment", 10.0),
        mk("holding", 2, "功率", "i16", "rw", "off", 0.0),
        mk("input", 0, "温度", "i16", "ro", "off", 25.0),
        mk("input", 1, "湿度", "u16", "ro", "off", 55.0),
        mk("coil", 0, "主继电器", "bit", "rw", "off", 0.0),
        mk("coil", 1, "备用继电器", "bit", "rw", "off", 0.0),
        mk("discrete", 0, "运行状态", "bit", "ro", "off", 1.0),
    ]
}
