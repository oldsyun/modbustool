# Modbus Tool

一个面向开发、调试和联调的 Modbus 工作台。应用使用 **Tauri 2.x + Rust + Vite + TypeScript** 构建，支持 macOS，并兼顾 Windows / Linux。

它同时提供 Modbus 主站工作台和内置从站模拟器，可用于验证设备通信、观察底层报文，以及在没有真实从站设备时进行联调。

## 功能

- 支持 Modbus TCP、Modbus UDP、RTU over TCP、RTU over UDP 和 Modbus RTU 串口通信。
- 支持功能码 `01`、`02`、`03`、`04`、`05`、`06`、`15`、`16` 的读写操作。
- 支持多组轮询配置，可为每组配置独立的地址、操作和周期，并行执行。
- 实时查看请求和响应的 PDU / ADU、十六进制字节流及往返耗时。
- 支持 `U16`、`I16`、`U32`、`I32`、`F32` 数据格式，以及 `ABCD`、`CDAB`、`BADC`、`DCBA` 字序转换。
- 内置从站模拟器，支持多 Unit、寄存器批量创建、地址冲突检查和寄存器自动变化。
- 支持模拟器寄存器配置的 Excel `.xlsx` 导入与导出。

## 快速开始

### 使用桌面应用

环境要求：

- Node.js 和 npm
- Rust 工具链
- Tauri 2 开发环境（系统 WebView、平台编译依赖）

安装依赖并启动开发版本：

```bash
npm install
npm run tauri dev
```

构建发布包：

```bash
npm run tauri build
```

构建产物位于 `src-tauri/target/release/` 及其平台对应的 bundle 目录中。

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

## 从站模拟器

点击工具栏中的 **模拟器** 打开独立的从站模拟器窗口。

- 在通道设置中配置并启用 TCP、UDP、RTU over TCP、RTU over UDP 或串口 RTU 通道。
- 新增寄存器时可设置 Unit、区域、地址、名称、数据类型、访问模式和数量。
- 每个寄存器可选择 `Off`、`Sine`、`Random` 或 `Increment` 变化方式。
- 通过 **导入 Excel** / **导出 Excel** 管理当前 Unit 的寄存器配置。
- 读取未显式配置的地址时，模拟器返回 `0xFFFF`，便于区分“无数据”与有效的零值。

## 项目结构

```text
crates/modbus-core/   Rust Modbus 协议内核、传输层、主站、从站和模拟器
src-tauri/            Tauri 命令桥接和桌面应用后端
src/                  Vite + TypeScript 前端
使用说明.md           界面功能和操作细节
```

协议内核可以独立测试和复用；桌面应用通过 Tauri 命令调用 Rust 后端完成通信和模拟器管理。

## macOS 串口提示

使用 USB-RS485 适配器时，请先安装对应的 FTDI、CH340 或 SiLabs 驱动。串口通常显示为 `/dev/cu.*`。如果系统或打包配置启用了沙箱，还需要为应用配置串口访问权限。

更完整的操作步骤、参数说明和模拟器细节，请参阅 [使用说明.md](使用说明.md)。
