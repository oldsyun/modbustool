//! Shared application state, managed by Tauri and accessed from commands.
//!
//! The async Modbus client lives behind a `tokio::sync::Mutex` (safe to hold
//! across `.await`); the simulator and config use std `Mutex` (sync, no await
//! held). Flags coordinate the background poll/simulator loops.
//!
//! Connections are mutually exclusive — only one of {tcp, rtu} is active at a
//! time.  The `conn_type` flag tells the UI which protocol is in use.
//!
//! Polls are tracked by id in a HashMap so multiple polls (different Unit IDs
//! or different registers) can run concurrently against the same connection.
//!
//! The built-in simulator/slave supports **multi-Unit-ID simulation**: a
//! [`SimUnit`] table, each entry owning its own sparse [`SharedImage`] and its
//! own register registry. The TCP slave, the RTU (serial) slave and the UI
//! editor all address units by ID, so different Unit IDs see independent data.

use modbus_core::client::ModbusClient;
use modbus_core::server::{new_shared_image, SharedImage, UnitImages};
use modbus_core::simulator::Simulator;
use modbus_core::transport::rtu::RtuTransport;
use modbus_core::transport::udp::UdpTransport;
use modbus_core::workspace::Workspace;
use crate::simreg::RegisterDef;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tauri::async_runtime::JoinHandle;
use tokio::sync::{broadcast, Mutex as AsyncMutex};

/// 0 = disconnected, 1 = TCP, 2 = RTU, 3 = UDP
pub const CONN_NONE: u8 = 0;
pub const CONN_TCP: u8 = 1;
pub const CONN_RTU: u8 = 2;
pub const CONN_UDP: u8 = 3;

/// 内置从站运行模式位（可同时运行多个模式，共享同一份 Unit 镜像）：
/// - TCP  ：Modbus TCP（MBAP 帧，绑定本机端口）
/// - UDP  ：Modbus UDP（MBAP 帧承载于 UDP 数据报）
/// - RTU  ：Modbus RTU（串口）
/// - RTU_TCP：Modbus RTU over TCP/IP（RTU 帧承载于 TCP 流，无 MBAP 头）
/// - RTU_UDP：Modbus RTU over UDP/IP（RTU 帧承载于 UDP 数据报，无 MBAP 头）
pub const SLAVE_NONE: u8 = 0;
pub const SLAVE_TCP: u8 = 1;
pub const SLAVE_UDP: u8 = 2;
pub const SLAVE_RTU: u8 = 4;
pub const SLAVE_RTU_TCP: u8 = 8;
pub const SLAVE_RTU_UDP: u8 = 16;

/// 异步从站运行时（TCP/UDP/RTU-over-TCP/RTU-over-UDP）：统一用 broadcast
/// shutdown + tokio JoinHandle 管理生命周期；conns 仅 TCP 类有意义。
pub struct SlaveRuntime {
    pub shutdown: broadcast::Sender<()>,
    pub handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    pub conns: Arc<AtomicUsize>,
}

impl SlaveRuntime {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel::<()>(2);
        Self {
            shutdown: tx,
            handle: Arc::new(Mutex::new(None)),
            conns: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// 串口 RTU 从站运行时：阻塞线程 + AtomicBool 停止标志（与串口阻塞 I/O 匹配）。
pub struct SlaveSerialRuntime {
    pub stop: Arc<AtomicBool>,
    pub handle: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl SlaveSerialRuntime {
    fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            handle: Arc::new(Mutex::new(None)),
        }
    }
}

/// A background poll task: stop flag + spawned handle.
pub struct PollTask {
    pub stop: Arc<AtomicBool>,
    pub handle: JoinHandle<()>,
}

/// 一个被模拟的从站（Unit）：独立的稀疏数据镜像 + 独立的寄存器注册表。
pub struct SimUnit {
    pub unit_id: u8,
    pub image: SharedImage,
    pub regs: Arc<Mutex<Vec<RegisterDef>>>,
}

/// 新建一个从站单元：稀疏镜像 + 按需注册的少量示例寄存器。
pub fn new_sim_unit(unit_id: u8) -> SimUnit {
    let image: SharedImage = new_shared_image();
    let mut regs: Vec<RegisterDef> = Vec::new();
    {
        let mut img = image.lock().unwrap();
        for s in crate::simreg::default_reg_samples() {
            img.write_slot(&s.def.area, s.def.addr, s.value);
            regs.push(s.def);
        }
    }
    SimUnit {
        unit_id,
        image,
        regs: Arc::new(Mutex::new(regs)),
    }
}

pub struct AppState {
    pub client: Arc<AsyncMutex<Option<ModbusClient>>>,
    pub rtu: Arc<Mutex<Option<RtuTransport>>>,
    pub udp: Arc<AsyncMutex<Option<UdpTransport>>>,
    pub conn_type: Arc<AtomicU8>,
    /// 多 Unit 从站表：Unit ID → 独立的镜像 + 寄存器注册表。
    pub slave_units: Arc<Mutex<Vec<SimUnit>>>,
    /// 从站服务器实时读取的 Unit 镜像表（随增删同步重建）。
    pub unit_images: Arc<Mutex<UnitImages>>,
    pub simulator: Arc<Mutex<Simulator>>,
    pub config: Arc<Mutex<Workspace>>,
    pub poll_running: Arc<AtomicBool>,
    /// Background poll tasks keyed by `poll_id`. Multiple polls may run in
    /// parallel — each is its own spawned task. `poll_id` is supplied by the
    /// frontend (one per `PollConfig`).
    pub poll_tasks: Arc<Mutex<HashMap<u32, PollTask>>>,
    // ── Built-in slave server state（5 种运行模式，可同时运行）──
    /// Modbus TCP 从站运行时（MBAP over TCP）。
    pub slave_tcp: SlaveRuntime,
    /// Modbus UDP 从站运行时（MBAP over UDP）。
    pub slave_udp: SlaveRuntime,
    /// Modbus RTU over TCP 从站运行时（RTU 帧 over TCP 流）。
    pub slave_rtu_tcp: SlaveRuntime,
    /// Modbus RTU over UDP 从站运行时（RTU 帧 over UDP 数据报）。
    pub slave_rtu_udp: SlaveRuntime,
    /// Modbus RTU 串口从站运行时（阻塞线程）。
    pub slave_rtu_serial: SlaveSerialRuntime,
    /// 最近一次各模式的启动配置（用于对话框重开时回填）。
    pub slave_configs: Arc<Mutex<std::collections::HashMap<String, serde_json::Value>>>,
    /// 活动的从站模式位域（SLAVE_TCP | SLAVE_UDP | SLAVE_RTU | SLAVE_RTU_TCP | SLAVE_RTU_UDP）。
    pub slave_mode: Arc<AtomicU8>,
}

impl AppState {
    pub fn new() -> Self {
        // 默认 Unit 1：稀疏镜像 + 少量示例寄存器（按需注册，无预分配）。
        let default_unit = new_sim_unit(1);
        let mut unit_images: UnitImages = UnitImages::new();
        unit_images.insert(default_unit.unit_id, default_unit.image.clone());
        Self {
            client: Arc::new(AsyncMutex::new(None)),
            rtu: Arc::new(Mutex::new(None)),
            udp: Arc::new(AsyncMutex::new(None)),
            conn_type: Arc::new(AtomicU8::new(CONN_NONE)),
            slave_units: Arc::new(Mutex::new(vec![default_unit])),
            unit_images: Arc::new(Mutex::new(unit_images)),
            simulator: Arc::new(Mutex::new(Simulator::new())),
            config: Arc::new(Mutex::new(Workspace::new())),
            poll_running: Arc::new(AtomicBool::new(false)),
            poll_tasks: Arc::new(Mutex::new(HashMap::new())),
            slave_tcp: SlaveRuntime::new(),
            slave_udp: SlaveRuntime::new(),
            slave_rtu_tcp: SlaveRuntime::new(),
            slave_rtu_udp: SlaveRuntime::new(),
            slave_rtu_serial: SlaveSerialRuntime::new(),
            slave_configs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            slave_mode: Arc::new(AtomicU8::new(SLAVE_NONE)),
        }
    }

    /// 按 `slave_units` 重建 `unit_images`（Unit 增删后调用，供从站服务器实时读取）。
    pub fn rebuild_unit_images(&self) {
        let mut map = self.unit_images.lock().unwrap();
        map.clear();
        let units = self.slave_units.lock().unwrap();
        for u in units.iter() {
            map.insert(u.unit_id, u.image.clone());
        }
    }
}

pub const ORDERING: Ordering = Ordering::SeqCst;
