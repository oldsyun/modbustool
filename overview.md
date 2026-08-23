# Modbus Workbench — 模拟器重构为独立窗口 + 双视图模块

## 1. 模块化设计（5 种运行模式，可并发启用）
模拟器为**独立 `WebviewWindow`**（label `simulator`，页面 `simulator.html`），与主界面分离的窗口。支持 5 种从站模式，可同时运行、独立启停、切换即时生效（无需重启）：
- **Modbus TCP**：绑定本机地址并监听端口（MBAP 帧）
- **Modbus UDP**：UDP 数据报承载 MBAP 帧
- **RTU over TCP**：TCP 流上承载 RTU 帧（无 MBAP 头）
- **RTU over UDP**：每个 UDP 数据报承载一个完整 RTU 帧
- **Modbus RTU（串口）**：以本机串口作为子站

后端统一入口：`sim_slave_start(req)` / `sim_slave_stop(mode)` / `sim_slave_stop_all()` / `sim_slave_status()`（`sim_slave_status` 返回各模式运行状态 + TCP 连接数 + 最近配置）。

## 2. 页面优化（连接语义 + 双视图引导）
- 点击工具栏「⚙ 模拟器」→ 打开独立 `simulator.html` 窗口。**首屏按 `sim_slave_status()` 判断**：
  - 无任何已运行/已配置通道 → 直接进入「**设置运行模式**」页（引导初始配置）。
  - 已有运行/配置通道 → 直接进入「**寄存器设置**」页。
- 「设置运行模式」页：5 模式 radio + 自适应配置 + **连接/断开通道**按钮（语义为「连接」操作，而非「启动从站」），与「通道设置」`connectDialog` 一致。
- 「寄存器设置」页（通道连接后创建）：运行模式面板（启停各通道模拟）+ Unit 标签（增删 Unit）+ 4 区域 tab（保持/输入/线圈/离散输入）+ 实时寄存器表（增/删/改寄存器，重复地址弹「替换」确认）。
- **独立关闭不停止模拟**：关闭寄存器设置窗口不会停止后端从站（从站任务归 `AppState` 所有）；再次点击「⚙ 模拟器」若从站仍在运行则直接展示寄存器设置页。
- **主 app 关闭回收资源**：主窗口 `onCloseRequested` 会关闭模拟器窗口，确保资源正确释放。

## 3. 运行时寄存器管理
- 按区域实时显示各寄存器的 **地址 / 名称 / 类型 / 访问 / 自动变化 / 值 / HEX**。
- 支持运行时**动态新增、编辑、删除寄存器**——数量即时变化，无需重启；多字类型按单字原始值存储。
- 实时同步：`slave-values`（主站写入 / 自动变化推送数值）与 `sim-regs-updated`（寄存器定义变更）事件驱动刷新（`src/simulator-window.ts` 监听，按 Unit ID 过滤；正在编辑的行不会被覆盖）。

## 关键 Bug 修复
- `sim_slave_start` 报错 `missing required key req`：该命令第三参数是**结构体** `req: SlaveStartReq`，前端必须以 `invoke("sim_slave_start", { req: {...} })` 形式传参。
- 字段命名不匹配：`SlaveStartReq` 增加 `#[serde(rename_all = "camelCase")]`，前端字段须用 camelCase（`mode/bind/port/portName/baudRate/dataBits/stopBits/parity/interFrameMs`），否则串口参数被静默忽略。
- 快照键名统一：`ImageSnapshot` 已加 `#[serde(rename_all = "camelCase")]`，`slave-values` 与 `sim-regs-updated`/`sim_reg_list` 三处键名一致（camelCase）。

## 关键文件
- `simulator.html`（新）：双视图 `#simModeView` / `#simRegView` + `#simRegDialog`/`#simReplaceDialog`/`#simDelDialog`
- `src/simulator-window.ts`（新）：`initSimulatorWindow()` 绑定全部交互与事件
- `src/main.ts`：`btnSimulator` 改为 `WebviewWindow` 打开/聚焦模拟器窗口；主窗口关闭回收模拟器窗口
- `vite.config.ts`：恢复 `simulator` 入口；`capabilities/default.json`：恢复 `windows: ["main","simulator"]` + `core:webview:allow-create-webview-window`
- `src/style.css`：新增窗口布局类（`.sim-win`/`#simWinBar`/`.sim-view`/`.sim-mode-panel` 等）
- 后端：`crates/modbus-core/src/server.rs`（`run_udp_slave` / `run_rtu_over_tcp_slave` / `run_rtu_over_udp_slave` / `ImageSnapshot`）、`src-tauri/src/state.rs`、`src-tauri/src/commands.rs`（`sim_slave_*` + `SlaveStartReq` camelCase）、`src-tauri/src/simreg.rs`

## 验证
- `cargo check -p modbus-workbench` ✓
- `cargo test -p modbus-core` ✓ 全过
- `npm run build`（vite）✓ 产出 `dist/index.html` + `dist/simulator.html`
- `npx tsc --noEmit`：仅余 `main.ts` 历史遗留 `$()` 类型告警（vite/esbuild 不检查，构建通过）
- `npm run tauri build` ✓ → `target/release/bundle/macos/ModbusWorkbench.app`
