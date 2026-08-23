# Modbus Workbench — Tauri 2.x + Rust

跨平台（**macOS 优先，兼顾 Windows / Linux**）的 Modbus 调试 / 测试工作台。
**不使用 Electron**：UI 层由系统原生 WebView 承载，业务内核为纯 Rust（`modbus-core`）。

## 技术栈
- **Tauri 2.x** —— 应用外壳、命令桥接、菜单栏（NSStatusBar）、原生菜单、窗口 Vibrancy。
- **Rust `modbus-core`** —— 零 UI 依赖的协议内核（帧编解码、传输、主站、从站/模拟器、脚本、配置）。
- **前端** —— Vite + TypeScript + `@tauri-apps/api`（连接/寄存器网格/报文日志/帧构造/脚本界面）。

## 目录结构
```
modbus-workbench/
├── Cargo.toml                 # workspace（modbus-core + src-tauri）
├── crates/modbus-core/        # 协议内核（可单独 cargo test）
│   └── src/
│       ├── crc.rs             # CRC-16/MODBUS
│       ├── framing.rs         # TCP(MBAP) / RTU 帧编解码 + CRC 校验
│       ├── data.rs            # 寄存器格式化 / 字序(ABCD·CDAB·BADC·DCBA)
│       ├── client.rs          # 异步主站（读/写/重试）
│       ├── slave.rs           # 从站数据区与请求处理
│       ├── simulator.rs       # 内置模拟器（自增/正弦/随机变化）
│       ├── transport/         # tcp.rs（就绪）/ rtu.rs（feature rtu）
│       ├── scripting.rs       # Lua 自动化脚本宿主（feature scripting）
│       └── workspace.rs       # .mbw 配置序列化
├── src-tauri/                 # Tauri 2.x 应用
│   ├── src/{main,state,commands,tray}.rs
│   ├── tauri.conf.json
│   └── capabilities/default.json
├── index.html / src/          # 前端（Vite）
├── package.json / vite.config.ts / tsconfig.json
└── gen_icon.cjs               # 生成占位应用图标
```

## 内核测试（无需 GUI）
```bash
cargo test -p modbus-core                 # 17 个用例：CRC / 帧 / 字序 / 从站 / 模拟器 / 配置
cargo check -p modbus-core --features rtu,scripting   # 校验 RTU 串口 + Lua 脚本模块
```

## 运行与构建
```bash
# —— Rust 侧（无需 Node 即可验证内核与编译）——
cargo test -p modbus-core                     # 17 用例全绿
cargo build -p modbus-workbench               # 完整二进制链接通过（已验证）
cargo check -p modbus-core --features rtu,scripting   # RTU 串口 + Lua 脚本模块

# —— 完整桌面应用（前端 + 原生壳）——
npm install                 # 安装前端依赖（@tauri-apps/api 等）
npm run tauri dev           # 开发模式：启动 Vite + Rust 后端
npm run tauri build         # 产出各平台安装包（macOS: .app / .dmg）
```
> 本机已验证：`modbus-core` 17 个测试全绿，`src-tauri` 完整 `cargo build` 成功链接
> （含 tauri v2.11.5 / mlua / serial2 / window-vibrancy / tray-icon），零编译错误、零警告。
> 运行完整 GUI 需先 `npm install && npm run build` 产出前端 `dist`，再 `npm run tauri dev`。

## macOS 注意事项
- **菜单栏常驻**：`src-tauri/src/tray.rs` 用 `TrayIconBuilder` 在状态栏放置图标 + 右键菜单。
- **原生视觉**：`main.rs` 在 macOS 上套用 `window_vibrancy`（需 `tauri.conf.json` 中
  `macOSPrivateApi: true`）。
- **串口(RTU)权限**：USB-RS485 适配器需安装 FTDI/CH340/SiLabs 驱动；应用若开启沙箱，
  需配置 `com.apple.security.device.serial` entitlement，否则读不到 `/dev/cu.*`。
- **同机主从互连**：用虚拟串口对，例如
  `socat PTY,raw,echo=0 PTY,raw,echo=0` 得到 `/dev/ttys0xx`↔`/dev/ttys1xx` 一对，
  从站起其一、主站连其二。
- **分发信任**：发布前需 **代码签名 + 公证（Notarization）**，否则 Gatekeeper 拦截。

## 已实现 / 待补全
- 已实现：内核（CRC/帧/字序/主站/从站/模拟器/配置/Lua 脚本）、Tauri 命令桥接、菜单栏、
  前端连接/读写/轮询/模拟器/帧下发/脚本/配置界面。
- 待补全（工程化下一步）：报文日志环形缓冲与导出、实时曲线(uPlot/ECharts)、
  RTU 连接命令、脚本驱动真实在线主站（当前脚本绑定内置模拟器）、单元/集成测试覆盖率、
  Windows/Linux 打包验证。
