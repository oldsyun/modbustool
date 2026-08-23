//! 模拟器「寄存器注册表」——按需注册的寄存器配置层。
//!
//! 与旧实现（预分配 1000 个保持寄存器等）不同，模拟器内核采用稀疏数据
//! 镜像（`DataImage`），本模块负责维护**寄存器定义**（区域/地址/名称/
//! 数据类型/访问属性），并内置「少量示例寄存器」作为初始种子。用户在
//! 模拟器设置窗口里新增的寄存器，通过 `sim_reg_add` 等命令按需写入
//! 内核（`DataImage` 相应槽位自动创建），不再一次性分配大数组。

use modbus_core::server::ImageSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

/// 生成唯一 id（时间纳秒 + 哈希）。
pub fn gen_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = DefaultHasher::new();
    nanos.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 一条寄存器定义（配置信息；数值实时读取自共享镜像）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegisterDef {
    pub id: String,
    /// `holding` | `input` | `coil` | `discrete`
    pub area: String,
    pub addr: u16,
    pub name: String,
    /// `u16` | `i16` | `u32` | `i32` | `f32` | `bit`
    pub dtype: String,
    /// `rw` | `ro`
    pub access: String,
    /// 该寄存器的独立自动变化模式：`off` | `sine` | `random` | `increment`
    pub vary: String,
    pub created_at_ms: u64,
}

/// 新增/编辑寄存器时前端提交的字段。
#[derive(Clone, Debug, Deserialize)]
pub struct RegInput {
    pub area: String,
    pub addr: u16,
    pub name: String,
    pub dtype: String,
    pub access: String,
    /// 该寄存器的自动变化模式（`off` | `sine` | `random` | `increment`）。
    pub vary: String,
    /// 初始/更新值；bit 区域按 0/1 处理，寄存器区域按 u16 截断。
    pub value: f64,
}

/// 寄存器列表查询响应：所属 Unit + 定义 + 当前数值快照。
#[derive(Clone, Serialize)]
pub struct RegListResp {
    pub unit_id: u8,
    pub defs: Vec<RegisterDef>,
    pub snapshot: ImageSnapshot,
}

/// 一条示例寄存器（定义 + 初始值）。
pub struct RegSample {
    pub def: RegisterDef,
    pub value: f64,
}

/// 区域中文名（用于 UI 与错误提示）。
pub fn area_label(area: &str) -> &'static str {
    match area {
        "holding" => "保持寄存器",
        "input" => "输入寄存器",
        "coil" => "线圈",
        "discrete" => "离散输入",
        _ => "未知区域",
    }
}

/// 初始「少量示例寄存器」——代替旧的 1000 个预分配寄存器。
/// 仅包含 8 条常用示例，用户在设置窗口中按需增删改。
/// 其中「电压」正弦、「电流」自增，方便直观看到每个寄存器独立变化。
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
