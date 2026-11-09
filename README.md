# Modbus Tool

一个面向开发、调试和联调的 Modbus 工作台。应用使用 **Rust + GPUI（gpui-kit）原生 GUI** 构建，支持 macOS，并兼顾 Windows / Linux。

它同时提供 Modbus 主站工作台和内置从站模拟器，可用于验证设备通信、观察底层报文，以及在没有真实从站设备时进行联调。

> 本项目已从 Tauri 2.x + Vite/TypeScript 前端迁移为纯 Rust 原生应用（GPUI）。旧的 `src-tauri/`、`src/`、`index.html` 等已移除。

## 功能

- 支持 Modbus TCP、Modbus UDP、RTU over TCP、RTU over UDP 和 Modbus RTU 串口通信。
- 支持功能码 `01`、`02`、`03`、`04`、`05`、`06`、`15`、`16` 的读写操作。
- 支持多组轮询配置，可为每组配置独立的地址、操作和周期，并行执行。
- 支持 Unit ID 扫描（从站发现）：指定一个保持寄存器地址，自动依次探测 Unit ID 1~247，以绿色圆形色块列出有响应的从站，并可随时停止。
- 实时查看请求和响应的 PDU / ADU、十六进制字节流及往返耗时。
- 支持 `U16`、`I16`、`U32`、`I32`、`F32` 数据格式，以及 `ABCD`、`CDAB`、`BADC`、`DCBA` 字序转换。
- 内置从站模拟器，支持多 Unit、寄存器批量创建、地址冲突检查和寄存器自动变化。
- 支持模拟器寄存器配置的 Excel `.xlsx` 导入与导出。
- 内置**纯串口调试**窗口：文本 / HEX 收发、行结束符、循环发送、时间戳与 HEX 显示、暂停/清空、导出日志、收发计数（与 Modbus 串口连接互斥）。

## 项目结构

```text
crates/modbus-core/   Rust Modbus 协议内核、传输层、主站、从站和模拟器（与 UI 无关，可独立测试）
app/                  GPUI 原生桌面应用
  src/core/           服务层（状态 + 全部 Modbus 操作 + 事件流，异步 Rust，无 tauri）
  src/ui/             三个原生窗口：主工作台 / 从站模拟器 / Unit ID 扫描
scripts/cargo-gpui.sh 构建脚本（封装本仓库所需的构建环境）
vendor/gpui-pre-macos  为兼容 stable 工具链而本地打补丁的 gpui 平台 crate
使用说明.md           界面功能和操作细节
```

协议内核可以独立测试和复用；桌面应用直接调用 Rust 服务层完成通信和模拟器管理（无 WebView、无 JS 桥接）。

## 快速开始

### 环境要求

- Rust 工具链（stable，已验证 1.97）
- macOS：**无需完整 Xcode**（使用下方构建脚本即可）

### 构建与运行

本仓库使用 `gpui-kit 0.9.0`（内含 `gpui-pre 0.3.11`）。该版本引用了 std 的一个不稳定特性，
因此构建时需要 `RUSTC_BOOTSTRAP=1`；同时仓库内的 `vendor/gpui-pre-macos` 已打补丁。
为避免污染系统 `~/.cargo`，构建脚本会把 `CARGO_HOME` 指向仓库内的 `.cargo-home/`。

一条命令即可：

```bash
./scripts/cargo-gpui.sh run          # 构建并运行
./scripts/cargo-gpui.sh build        # 仅构建
./scripts/cargo-gpui.sh build --release
```

产物位于 `target-gpui/debug/modbus-tool`（或 `release/`）。

> 若你使用完整 Xcode 且想直接调用 cargo，可自行设置：
> `RUSTC_BOOTSTRAP=1 CARGO_HOME=$PWD/.cargo-home cargo run -p modbus-tool-gpui`。
> 注意 `[patch.crates-io]` 必须位于工作区根 `Cargo.toml`（已配置）。

### 验证 Rust 内核

无需启动图形界面即可运行核心测试：

```bash
cargo test -p modbus-core
```

## 主站工作台

1. 点击左上角的 **连接**，选择通信方式并填写连接参数。
2. 在轮询配置中选择功能码、Unit ID、起始地址和数量。
3. 读操作可以开启定时轮询，也可以点击 **发送** 执行一次读取。
4. 写操作填写写入值后发送。功能码 `15` 和 `16` 支持使用逗号分隔的多个值，数量会根据实际值数量自动计算。
5. 在报文追踪区域查看请求和响应的 PDU、ADU、字节流与耗时。
6. 在寄存器解析表中切换数据格式和字序，也可以直接修改单行数据并执行写入。
7. 点击工具栏的 **扫描** 打开 Unit ID 扫描窗口：填写保持寄存器地址后点击 **开始扫描**，依次探测 Unit ID 1~247；有响应的从站显示为绿色色块，按钮前显示已扫描数量，可随时 **停止扫描**。

## 从站模拟器

点击工具栏中的 **模拟器** 打开独立的从站模拟器窗口。

- 在通道设置中配置并启用 TCP、UDP、RTU over TCP、RTU over UDP 或串口 RTU 通道。
- 新增寄存器时可设置 Unit、区域、地址、名称、数据类型、访问模式和数量。
- 每个寄存器可选择 `Off`、`Sine`、`Random` 或 `Increment` 变化方式。
- 通过 **导入 Excel** / **导出 Excel** 管理当前 Unit 的寄存器配置。
- 读取未显式配置的地址时，模拟器返回 `0xFFFF`，便于区分“无数据”与有效的零值。

## 串口调试

点击工具栏的 **🔌 串口调试** 打开独立的串口终端窗口。**连接参数复用主页面配置**：串口调试不再单独设置端口/波特率等，而是直接读取主页面最近一次 RTU 串口连接的参数（端口、波特率、数据位、校验、停止位），显示为只读摘要并据此连接。

- 使用前请先在主页面「连接」中选择 RTU 串口并填写参数；串口调试窗口会显示该串口摘要，点击 **连接** 即以同一串口打开（与 Modbus 主站互斥，打开时会先断开 Modbus RTU 连接，关闭后可重新连接）。
- 接收区显示收到的数据；可切换 **文本 / HEX** 显示、显示时间戳、暂停、清空、导出 TXT。
- 发送区支持 **文本 / HEX** 两种模式，可选行结束符（无 / CR / LF / CRLF）；可开启 **循环发送** 按设定毫秒间隔重复发送。
- 状态栏实时显示 TX / RX 字节计数。
- 编码按 UTF-8 / ASCII 处理。

## macOS 串口提示

使用 USB-RS485 适配器时，请先安装对应的 FTDI、CH340 或 SiLabs 驱动。串口通常显示为 `/dev/cu.*`。

更完整的操作步骤、参数说明和模拟器细节，请参阅 [使用说明.md](使用说明.md)。
