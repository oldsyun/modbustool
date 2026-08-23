use crate::framing::FramingError;
use std::fmt;

/// Errors that can occur at any layer of the Modbus stack.
#[derive(Debug)]
pub enum ModbusError {
    Framing(FramingError),
    Exception(u8),
    Io(std::io::Error),
    Other(String),
}

impl fmt::Display for ModbusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModbusError::Framing(e) => write!(f, "framing error: {e}"),
            ModbusError::Exception(code) => {
                // 异常码 + 中文名称 + 详细说明（用户自定义解释表）
                write!(f, "modbus exception: {code:#04X} · {}", exception::zh_name(*code))?;
                if let Some(desc) = exception::description(*code) {
                    write!(f, " — {desc}")?;
                }
                Ok(())
            }
            ModbusError::Io(e) => write!(f, "io error: {e}"),
            ModbusError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ModbusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ModbusError::Framing(e) => Some(e),
            ModbusError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<FramingError> for ModbusError {
    fn from(e: FramingError) -> Self {
        Self::Framing(e)
    }
}

impl From<std::io::Error> for ModbusError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Modbus exception codes (FC | 0x80 response).
pub mod exception {
    pub const ILLEGAL_FUNCTION: u8 = 0x01;
    pub const ILLEGAL_DATA_ADDRESS: u8 = 0x02;
    pub const ILLEGAL_DATA_VALUE: u8 = 0x03;
    pub const SLAVE_DEVICE_FAILURE: u8 = 0x04;
    pub const ACKNOWLEDGE: u8 = 0x05;
    pub const SLAVE_DEVICE_BUSY: u8 = 0x06;
    /// 网关路径不可用（多 Unit 模式下用于「未配置的 Unit ID」）。
    pub const GATEWAY_PATH_UNAVAILABLE: u8 = 0x0A;
    /// 网关目标设备无法响应（多 Unit 模式下用于「未配置的 Unit ID」）。
    pub const GATEWAY_TARGET_FAILED: u8 = 0x0B;

    /// Human-readable name of an exception code (matches nmw style).
    pub fn name(code: u8) -> &'static str {
        match code {
            0x01 => "Illegal Function",
            0x02 => "Illegal Data Address",
            0x03 => "Illegal Data Value",
            0x04 => "Slave Device Failure",
            0x05 => "Acknowledge",
            0x06 => "Slave Device Busy",
            _ => "Unknown Exception",
        }
    }

    /// 异常码中文名称。
    pub fn zh_name(code: u8) -> &'static str {
        match code {
            0x01 => "非法的功能码",
            0x02 => "非法的数据地址",
            0x03 => "非法的数据值",
            0x04 => "从设备故障",
            0x06 => "从设备忙",
            0x0A => "网关路径不可用",
            0x0B => "网关目标设备无法响应",
            _ => "未知异常",
        }
    }

    /// 异常码详细说明（中文）；未收录的码返回 None。
    pub fn description(code: u8) -> Option<&'static str> {
        match code {
            0x01 => Some("查询中收到的功能代码不被从站识别或不被从站允许。"),
            0x02 => Some("查询中收到的数据地址（寄存器编号）不是从站允许的地址，即寄存器不存在。如果请求多个寄存器，则至少有一个寄存器不被允许。"),
            0x03 => Some("查询数据字段中包含的值对于从站来说是不可接受的。"),
            0x04 => Some("从站尝试执行请求的操作时发生不可恢复的错误"),
            0x06 => Some("从属设备正在处理一个长持续时间的命令。主设备应稍后重试。"),
            0x0A => Some("与网关结合使用的专门用途，通常意味着网关配置错误或超载"),
            0x0B => Some("专门与网关结合使用，表示未从目标设备收到响应。"),
            _ => None,
        }
    }
}
