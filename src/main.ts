import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { getCurrentWindow } from "@tauri-apps/api/window";

// ═══════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════

const $ = <T extends HTMLElement = HTMLElement>(id: string): T =>
  document.getElementById(id) as T;

/** Format Date → "HH:MM:SS:mmm" (24h, ms precision). */
function fmtTs(d: Date): string {
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  const ms = String(d.getMilliseconds()).padStart(3, "0");
  return `${hh}:${mm}:${ss}:${ms}`;
}

function log(msg: string, kind: "tx" | "rx" | "ok" | "err" | "info" = "info") {
  const el = $("logOutput");
  const ts = fmtTs(new Date());
  const color =
    kind === "tx" ? "#0071e3" :
    kind === "rx" ? "#34c759" :
    kind === "ok" ? "#34c759" :
    kind === "err" ? "#ff3b0f" : "#86868b";
  const line = document.createElement("div");
  line.style.color = color;
  line.textContent = `[${ts}] ${msg}`;
  el.appendChild(line);
  el.scrollTop = el.scrollHeight;
}

/** Render a Modbus frame exchange: TX line (blue) + RX line (green) with RTT suffix. */
function logFrame(opts: { tx: string; rx: string; rttMs: number | null; error?: string | null }) {
  const el = $("logOutput");
  const ts1 = fmtTs(new Date());

  if (opts.tx) {
    const txLine = document.createElement("div");
    txLine.style.color = "#0071e3";
    txLine.textContent = `[${ts1}]TX: ${opts.tx}`;
    el.appendChild(txLine);
  }

  // RX 时间戳与 TX 相同（同一次响应），末尾追加 RTT。
  const suffix = opts.rttMs !== null && opts.rttMs !== undefined ? `（${opts.rttMs}ms）` : "";
  const rxText = opts.error
    ? `ERR: ${opts.error}`
    : (opts.rx || "(无响应)");
  const rxLine = document.createElement("div");
  rxLine.style.color = opts.error ? "#ff3b0f" : "#34c759";
  rxLine.textContent = `[${ts1}]RX: ${rxText}${suffix}`;
  el.appendChild(rxLine);

  el.scrollTop = el.scrollHeight;
}

/** 日志分隔条：在「单次发送」与「周期性发送」两类操作之间插入明显的分隔线，便于区分。 */
function logDivider(title: string) {
  const el = $("logOutput");
  const hr = document.createElement("div");
  hr.className = "log-divider";
  hr.textContent = title;
  el.appendChild(hr);
  el.scrollTop = el.scrollHeight;
}

async function safe<T>(p: Promise<T>, okMsg?: string): Promise<T | null> {
  try {
    const r = await p;
    if (okMsg) log(okMsg, "ok");
    return r;
  } catch (e) {
    log(String(e), "err");
    return null;
  }
}

// ═══════════════════════════════════════════════════════════
// State
// ═══════════════════════════════════════════════════════════

let connected = false;
let connProto: "tcp" | "udp" | "rtu" | "rtuudp" | null = null;
let polling = false;
// 连接已建立（端口已打开）但仍在等待首帧轮询成功后，才升级为"已连接"。
let awaitPollConnected = false;

function setConnected(proto: "tcp" | "udp" | "rtu" | "rtuudp") {
  connected = true;
  connProto = proto;
  // 端口刚打开阶段显示「已就绪/已打开」；待首帧轮询成功后再升级为「已连接」。
  const label =
    proto === "rtu" ? "串口已打开"
    : proto === "udp" ? "UDP 通道已就绪"
    : proto === "rtuudp" ? "RTU-over-UDP 通道已就绪"
    : "TCP 端口已打开";
  $("statusConn").textContent = label;
  awaitPollConnected = true;
  // 同一个按钮在已连接时切换为"断开"
  const btn = $("btnConnect");
  btn.textContent = "⟳ 断开";
  btn.classList.add("connected");
  btn.title = "断开当前连接";
  // 联动启用依赖连接的操作
  ($("chkAutoPoll") as HTMLInputElement).disabled = false;
  // 端口/串口已打开即可单次发送（与是否轮询无关）
  $("btnSendOnce").disabled = false;
}

function setDisconnected() {
  connected = false;
  connProto = null;
  awaitPollConnected = false;
  $("statusConn").textContent = "未连接";
  // 还原按钮为"连接"
  const btn = $("btnConnect");
  btn.textContent = "⬤ 连接";
  btn.classList.remove("connected");
  btn.title = "连接";
  // 联动禁用依赖连接的操作；断开同时停止轮询（后端已停任务，前端复位按钮态）
  const autoPollBtn = $("chkAutoPoll") as HTMLButtonElement;
  autoPollBtn.disabled = true;
  $("btnSendOnce").disabled = true;
  setPolling(false);
}

function setPolling(active: boolean) {
  polling = active;
  const btn = $("chkAutoPoll") as HTMLButtonElement;
  if (active) {
    btn.textContent = "⟳ 停止轮询";
    btn.classList.add("connected");
    btn.title = "停止定时轮询";
  } else {
    btn.textContent = "⬤ 轮询";
    btn.classList.remove("connected");
    btn.title = "开始定时轮询";
  }
  $("statusPoll").textContent = active ? "正在轮询..." : "已停止";
}

// ═══════════════════════════════════════════════════════════
// Connection Dialog
// ═══════════════════════════════════════════════════════════

const dlgConnect = $("connectDialog") as HTMLDialogElement;
const dlgWrite = $("writeDialog") as HTMLDialogElement;
type ConnMode = "tcp" | "rtu" | "rtuotcp";
let selectedMode: ConnMode = "rtu";
type ConnTransport = "tcp" | "udp";
let selectedTransport: ConnTransport = "tcp";
// RTU over TCP/UDP 模式的 IP 承载方式（RTU 帧，无 MBAP 头）
let rtuOverIpTransport: ConnTransport = "tcp";

/** 按当前模式显示/隐藏对话框字段。 */
function applyConnMode() {
  document.querySelectorAll<HTMLInputElement>("input[name='connMode']").forEach((r) => {
    const label = r.closest(".radio-item") as HTMLElement | null;
    if (label) label.classList.toggle("active", r.value === selectedMode);
  });
  document.querySelectorAll<HTMLElement>(".conn-fields").forEach((el) => {
    const showFor = (el.dataset.showFor ?? "").split(/\s+/);
    el.classList.toggle("hidden", !showFor.includes(selectedMode));
  });
  if (selectedMode === "rtu") refreshSerialPorts();
}

// Toolbar 连接按钮：toggle 行为 —— 未连接时打开连接对话框；已连接时断开
$("btnConnect").addEventListener("click", async () => {
  if (connected) {
    await invoke("disconnect"); // 后端同时停止所有轮询任务
    runningPollIds.clear();
    setDisconnected();
    log("已断开，所有轮询已停止", "info");
    clearTable();
  } else {
    dlgConnect.showModal();
  }
});

// Close handlers
$("dlgClose").onclick = () => dlgConnect.close();
$("dlgCancel").onclick = () => dlgConnect.close();
$("writeDlgClose").onclick = () => dlgWrite.close();
$("writeDlgCancel").onclick = () => dlgWrite.close();
// 对话框任意方式关闭（确定/取消/X/ESC）后，若打开前在轮询则恢复轮询
dlgWrite.addEventListener("close", () => maybeResumePolls());

// 通用提示 / 警告弹窗（写入溢出等场景复用，避免依赖原生 alert）
const dlgAlert = $("alertDialog") as HTMLDialogElement;
function uiAlert(msg: string, title = "提示") {
  $("alertMsg").textContent = msg;
  $("alertTitle").textContent = title;
  dlgAlert.showModal();
}
$("alertOk").onclick = () => { dlgAlert.close(); if ($("writeDialog").open) ($("writeValue") as HTMLInputElement).focus(); };
$("alertClose").onclick = () => { dlgAlert.close(); if ($("writeDialog").open) ($("writeValue") as HTMLInputElement).focus(); };

// 协议模式 radio 切换
document.querySelectorAll<HTMLInputElement>("input[name='connMode']").forEach((r) => {
  r.addEventListener("change", () => {
    if (r.checked) selectedMode = r.value as ConnMode;
    applyConnMode();
  });
});

// Modbus TCP 的传输方式（TCP 流 / UDP 数据报）
$("connTransport").addEventListener("change", (e) => {
  selectedTransport = (e.target as HTMLSelectElement).value as ConnTransport;
});

// RTU over TCP/UDP 的 IP 承载方式（Modbus RTU over TCP/IP / UDP/IP）
$("rtuOverIpTransport").addEventListener("change", (e) => {
  rtuOverIpTransport = (e.target as HTMLSelectElement).value as ConnTransport;
});

// Connect button in dialog — 连接仅验证串口/端口能否打开，不设 Unit ID、不做测试读
$("dlgConnect").addEventListener("click", async () => {
  const btn = $("dlgConnect") as HTMLButtonElement;
  btn.disabled = true; // 防重复提交：避免并发发起两个 connect 命令
  /** 连接成功后：尽力停止可能残留的旧轮询任务（失败不阻断），按钮回到「启动」激活态并关闭弹框。 */
  const onConnected = async (proto: "tcp" | "udp" | "rtu" | "rtuudp") => {
    await safe(invoke("stop_all_polls")); // 尽力停旧任务，异常不影响连接结果
    runningPollIds.clear();
    setPolling(false);
    setConnected(proto);
    dlgConnect.close();
  };
  try {
    const host = $("connHost").value;
    const port = Number($("connPort").value);
    const timeoutMs = Number($("connTimeout").value);
    const retries = Number($("connRetries").value);
    const interFrameRaw = $("connInterFrame").value.trim();
    const interFrameMs = interFrameRaw === "" ? null : Number(interFrameRaw);

    if (selectedMode === "tcp") {
      // 成功判定不依赖返回值：connect_* 返回 ()，Tauri 会 resolve 为 null，
      // 与「失败返回 null」无法区分，故用 try/catch 判断 invoke 是否抛异常。
      try {
        await invoke("connect_tcp", { host, port, timeoutMs, retries, transport: selectedTransport });
        if (selectedTransport === "udp") {
          log(`UDP → ${host}:${port} 通道已就绪（无连接，发送即达）`, "ok");
        } else {
          log(`TCP → ${host}:${port} 端口已打开`, "ok");
        }
        await onConnected(selectedTransport);
      } catch (e) { log(String(e), "err"); }
    } else if (selectedMode === "rtuotcp") {
      try {
        await invoke("connect_rtu_over_tcp", { host, port, timeoutMs, retries, interFrameMs, transport: rtuOverIpTransport });
        if (rtuOverIpTransport === "udp") {
          log(`RTU-over-UDP → ${host}:${port} 通道已就绪（一个数据报一个 RTU 帧）`, "ok");
        } else {
          log(`RTU-over-TCP → ${host}:${port} 端口已打开`, "ok");
        }
        await onConnected(rtuOverIpTransport === "udp" ? "rtuudp" : "tcp");
      } catch (e) { log(String(e), "err"); }
    } else {
      const serialPort = ($("rtuPort") as HTMLSelectElement).value;
      const baud = Number($("rtuBaud").value);
      if (!serialPort) { log("请选择串口", "err"); return; }
      try {
        await invoke("connect_rtu", {
          portName: serialPort,
          baudRate: baud,
          dataBits: Number($("rtuDataBits").value),
          stopBits: Number($("rtuStopBits").value),
          parity: ($("rtuParity") as HTMLSelectElement).value,
          timeoutMs,
          interFrameMs,
        });
        log(`RTU ✓ 串口 ${serialPort} @ ${baud}bps 已打开`, "ok");
        await onConnected("rtu");
      } catch (e) { log(String(e), "err"); }
    }
  } finally {
    btn.disabled = false;
  }
});

// Refresh serial ports list
async function refreshSerialPorts() {
  const sel = $("rtuPort") as HTMLSelectElement;
  sel.innerHTML = '<option value="">加载中...</option>';
  const ports = await safe<string[]>(invoke("list_serial_ports"));
  if (ports && ports.length > 0) {
    sel.innerHTML = "";
    ports.forEach((p) => {
      const opt = document.createElement("option");
      opt.value = p; opt.textContent = p;
      sel.appendChild(opt);
    });
  } else {
    sel.innerHTML = '<option value="">无可用串口</option>';
  }
}
$("btnRefreshPorts").addEventListener("click", refreshSerialPorts);

// ═══════════════════════════════════════════════════════════
// Write Dialog — 由表格行内「写入」按钮触发
// ═══════════════════════════════════════════════════════════

/** 当前待写入的行上下文（打开对话框时记录，确认时消费）。 */
interface PendingWrite {
  addr: number;
  fmt: RowFormat;
  order: ByteOrder;
  func: string;
}
let pendingWrite: PendingWrite | null = null;
let resumePollAfterWrite = false; // 写入对话框打开时若正在轮询，关闭后需恢复

/**
 * 各显示格式的可写入数值范围（用于提示与溢出校验）。
 * f32 以 IEEE754 单精度可表示量级约定上下界。
 */
const WRITE_RANGE: Record<RowFormat, { min: number; max: number; label: string; intOnly: boolean }> = {
  u16: { min: 0, max: 0xffff, label: "0 ~ 65535", intOnly: true },
  i16: { min: -0x8000, max: 0x7fff, label: "-32768 ~ 32767", intOnly: true },
  u32: { min: 0, max: 0xffffffff, label: "0 ~ 4294967295", intOnly: true },
  i32: { min: -0x80000000, max: 0x7fffffff, label: "-2147483648 ~ 2147483647", intOnly: true },
  f32: { min: -3.4028235e38, max: 3.4028235e38, label: "约 ±3.4e38（IEEE754 单精度）", intOnly: false },
};

/** 显示格式的可读中文名（用于提示文案）。 */
function formatName(f: RowFormat): string {
  return {
    u16: "无符号 16 位",
    i16: "有符号 16 位",
    u32: "无符号 32 位",
    i32: "有符号 32 位",
    f32: "浮点 32 位",
  }[f];
}

/**
 * 解析并校验写入输入：支持十进制与 0x 前缀十六进制。
 * - 线圈（FC01）：仅 0 / 1；
 * - 有符号格式允许以无符号补码形式输入（如 i16 的 0xFFFF = -1、i32 的 0xFFFFFFFF = -1）；
 * - 超范围或非法 → ok=false，msg 可直接用于弹窗提示。
 */
function parseWriteValue(
  raw: string,
  fmt: RowFormat,
  func: string,
): { ok: boolean; value?: number; msg?: string } {
  const s = raw.trim();
  if (!s) return { ok: false, msg: "请输入要写入的数值" };
  let isHex = false;
  let body = s;
  if (/^0x[0-9a-fA-F]+$/.test(s)) { isHex = true; body = s.slice(2); }
  let value: number;
  if (isHex) {
    value = parseInt(body, 16);
    if (!Number.isFinite(value)) return { ok: false, msg: "无效的十六进制数值" };
  } else {
    value = Number(s);
    if (!Number.isFinite(value)) return { ok: false, msg: "请输入有效数值（十进制或 0x 前缀十六进制）" };
  }
  // 线圈：仅 0 / 1
  if (func === "01") {
    if (!Number.isInteger(value) || value < 0 || value > 1)
      return { ok: false, msg: "线圈值无效：请输入 0 或 1" };
    return { ok: true, value };
  }
  const r = WRITE_RANGE[fmt];
  // 有符号格式支持以无符号补码形式输入（0x8000~0xFFFF = -32768~-1 等）
  if (fmt === "i16" && isHex && value >= 0x8000 && value <= 0xffff) value -= 0x10000;
  else if (fmt === "i32" && isHex && value >= 0x80000000 && value <= 0xffffffff) value -= 0x100000000;
  if (r.intOnly && !Number.isInteger(value))
    return { ok: false, msg: `${formatName(fmt)} 仅支持整数` };
  if (value < r.min || value > r.max)
    return { ok: false, msg: `数值超出 ${formatName(fmt)} 范围（${r.label}）` };
  return { ok: true, value };
}

/**
 * 写入输入框实时校验（每次 input / change 触发）：
 *  1. 复用 parseWriteValue 解析并比对当前寄存器格式（pendingWrite.fmt）对应的最大值；
 *  2. 一旦“溢出”（超出 WRITE_RANGE[fmt].max / min）→ 立即弹出 uiAlert 警告；
 *     同一溢出过程仅弹一次（overAlerted 守卫），避免逐字符刷屏；
 *  3. 其它非法（空、非完整数字、非整数等）→ 仅就地红字提示并禁用“写入”按钮，不弹窗；
 *  4. 合法 → 清除错误并恢复按钮。
 * 上限完全由 WRITE_RANGE 按格式驱动，因此不同寄存器类型自动取得不同上限
 * （u16=65535 / i16=±32767 / u32=4294967295 / i32=±2147483647 / f32≈±3.4e38）。
 */
let overAlerted = false;
function validateWriteLive(): boolean {
  const input = $("writeValue") as HTMLInputElement;
  const errEl = $("writeError");
  if (!pendingWrite) return false;
  const { fmt, func } = pendingWrite;
  const parsed = parseWriteValue(input.value, fmt, func);
  if (parsed.ok) {
    overAlerted = false;
    input.classList.remove("invalid");
    errEl.textContent = "";
    ($("writeDlgOk") as HTMLButtonElement).disabled = false;
    return true;
  }
  // 区分“溢出”与“其它临时/格式错误”：仅溢出立即弹窗，其余仅就地提示
  const isOverflow = !!parsed.msg && parsed.msg.includes("范围");
  input.classList.add("invalid");
  errEl.textContent = parsed.msg ?? "输入无效";
  ($("writeDlgOk") as HTMLButtonElement).disabled = true;
  if (isOverflow && !overAlerted) {
    overAlerted = true;
    uiAlert(parsed.msg ?? "数值超出范围", "数值溢出");
  }
  return false;
}

/**
 * 把已校验通过的用户数值拆成 1 或 2 个 u16 字。
 * 调用方须先用 parseWriteValue 完成范围/格式校验。
 * 32 位格式（u32/i32/f32）按当前字节序拆成高低两个字，供 FC16 一次写入。
 * FC01 线圈：值非 0 即 1。
 */
function valueToWords(value: number, fmt: RowFormat, order: ByteOrder, func: string): number[] | null {
  if (func === "01") return [value !== 0 ? 1 : 0];
  if (fmt === "u16") return [value & 0xffff];
  if (fmt === "i16") return [value < 0 ? value + 0x10000 : value];
  // 32 位：得到 4 字节（大端 b0..b3），再按字节序映射为 w1/w2
  let bytes: [number, number, number, number];
  if (fmt === "f32") {
    const buf = new ArrayBuffer(4);
    const dv = new DataView(buf);
    dv.setFloat32(0, value);
    bytes = [dv.getUint8(0), dv.getUint8(1), dv.getUint8(2), dv.getUint8(3)];
  } else {
    const u = value >>> 0; // u32/i32 → 无符号 32 位位模式（负数自动补码）
    bytes = [(u >>> 24) & 0xff, (u >>> 16) & 0xff, (u >>> 8) & 0xff, u & 0xff];
  }
  const [b0, b1, b2, b3] = bytes;
  switch (order) {
    case "abcd": return [b0 << 8 | b1, b2 << 8 | b3];
    case "cdab": return [b2 << 8 | b3, b0 << 8 | b1];
    case "badc": return [b1 << 8 | b0, b3 << 8 | b2];
    case "dcba": return [b3 << 8 | b2, b1 << 8 | b0];
  }
}

/** 写入对话框关闭后，若打开前正在轮询则恢复所有轮询（覆盖确定/取消/X/ESC 各种关闭方式）。 */
function maybeResumePolls() {
  if (resumePollAfterWrite) {
    resumePollAfterWrite = false;
    void startAllPolls();
  }
}

/** 打开写入对话框并记录待写行上下文。写入目标固定为当前行地址（32 位含下一寄存器）。 */
async function openWriteDialog(addr: number, fmt: RowFormat, order: ByteOrder, func: string) {
  pendingWrite = { addr, fmt, order, func };
  // 打开写入对话框即暂停轮询（写操作期间不刷新，避免与写入冲突）；关闭后若此前在轮询则恢复
  if (polling) {
    resumePollAfterWrite = true;
    await stopAllPolls();
  } else {
    resumePollAfterWrite = false;
  }
  $("writeDlgTitle").textContent = is32Bit(fmt)
    ? `写入寄存器 ${addr} & ${addr + 1}`
    : `写入寄存器 ${addr}`;
  $("writeValue").value = "";
  // 打开时重置实时校验状态（错误红字、禁用标记、溢出弹窗守卫）
  overAlerted = false;
  ($("writeValue") as HTMLInputElement).classList.remove("invalid");
  ($("writeDlgOk") as HTMLButtonElement).disabled = false;
  $("writeError").textContent = "";
  const is32 = is32Bit(fmt);
  const fmtName = formatName(fmt);
  const rangeText =
    func === "01" ? "范围：0 或 1" : `范围：${WRITE_RANGE[fmt].label}`;
  const hexText = "支持十六进制（0x 前缀，如 0x1A）";
  if (func === "01") {
    $("writeHint").textContent =
      `FC01 线圈 @${addr}（Unit ${currentPoll().unitId}）— ${rangeText}；${hexText}`;
  } else if (is32) {
    $("writeHint").textContent =
      `FC03 @${addr}（${fmtName}，字序 ${order.toUpperCase()}）— ${rangeText}；32 位数值将同时写入寄存器 ${addr} 与 ${addr + 1}；${hexText}`;
  } else {
    $("writeHint").textContent =
      `FC03 @${addr}（${fmtName}）— ${rangeText}；写入单个寄存器（Unit ${currentPoll().unitId}）；${hexText}`;
  }
  dlgWrite.showModal();
  $("writeValue").focus();
}

$("writeDlgOk").addEventListener("click", async () => {
  if (!pendingWrite) return;
  const { addr, fmt, order, func } = pendingWrite;
  const raw = $("writeValue").value;
  // 先做范围/格式/十六进制解析校验；溢出或非法时弹出对话框提示，并保持写入框打开
  const parsed = parseWriteValue(raw, fmt, func);
  if (!parsed.ok) {
    uiAlert(parsed.msg ?? "输入无效", "数值无效");
    return;
  }
  const words = valueToWords(parsed.value, fmt, order, func);
  if (!words) { uiAlert("数值无法转换为寄存器字", "数值无效"); return; }
  const unitId = currentPoll().unitId;
  const hexVals = words.map((w) => `0x${w.toString(16).padStart(4, "0").toUpperCase()}`).join(", ");
  try {
    // 写入指令：记录原始 TX/RX 报文，格式与单次发送保持一致
    const res = await invoke<{ tx: string; rx: string; rttMs: number }>("write_point", {
      unitId,
      func,
      addr,
      values: words,
    });
    logDivider("写入发送");
    logFrame({ tx: `[写入发送] ${res.tx}`, rx: res.rx, rttMs: res.rttMs });
    log(`写入成功 @${addr}（FC${func}，Unit ${unitId}）：${hexVals}`, "ok");
    await readBackAndRefresh(); // 写入后立即发起一次读取（写操作期间轮询已暂停，读回干净）
    dlgWrite.close(); // 关闭触发 close 事件 → 恢复轮询
  } catch (e) {
    log(`写入失败 @${addr}：${String(e)}`, "err");
  }
});

// 实时校验：输入/变更即校验，溢出立即提示并禁用“写入”按钮
$("writeValue").addEventListener("input", validateWriteLive);
$("writeValue").addEventListener("change", validateWriteLive);

/** 写入成功后立即按当前轮询范围发起一次读取并刷新表格（读回确认）。 */
async function readBackAndRefresh() {
  if (!connected) return;
  const p = currentPoll();
  try {
    const res = await invoke<{ regs: number[]; tx: string; rx: string; rtt_ms: number }>("read_points", {
      unitId: p.unitId,
      func: p.func,
      addr: p.startAddr,
      count: p.count,
    });
    if (res && res.regs && res.regs.length > 0) {
      currentData = { addr: p.startAddr, regs: res.regs };
      pollDataCache.set(p.id, { addr: p.startAddr, regs: res.regs });
      renderTable(p.startAddr, res.regs);
      // 写入后读回：记录原始 TX/RX 报文，格式与单次发送保持一致
      logDivider("写入后读回");
      logFrame({ tx: `[写入后读回] ${res.tx}`, rx: res.rx, rttMs: res.rtt_ms });
      log(`写入后读回 @${p.startAddr} ×${p.count}（FC${p.func}，Unit ${p.unitId}）`, "info");
    }
  } catch (e) {
    log(`写入后读回失败：${String(e)}`, "err");
  }
}

// ═══════════════════════════════════════════════════════════
// Poll Control — 多轮询标签页（共享同一通道）
// ═══════════════════════════════════════════════════════════

type RowFormat = "u16" | "i16" | "u32" | "i32" | "f32";
type ByteOrder = "abcd" | "cdab" | "badc" | "dcba";
interface RowState { format: RowFormat; byteOrder: ByteOrder; }

// 每行独立的格式/字节序选择；addr 为 key
const rowStates = new Map<number, RowState>();

interface PollConfig {
  id: number;
  name: string;
  unitId: number;
  func: string; // "01" | "02" | "03" | "04" | "05" | "06" | "15" | "16"
  startAddr: number;
  count: number;
  period: number;
  writeValue?: string;
  byteOrder?: ByteOrder;
  rowStates?: Record<number, RowState>;
}

let nextPollId = 0;
const polls: PollConfig[] = [
  { id: nextPollId++, name: "轮询 1", unitId: 1, func: "03", startAddr: 0, count: 10, period: 500, writeValue: "", byteOrder: "abcd", rowStates: {} },
];
let activePollId = polls[0].id;

function parsePollStartAddr(value: string): number {
  const text = value.trim();
  const addr = /^0x[0-9a-f]+$/i.test(text) ? parseInt(text.slice(2), 16) : Number(text);
  return Number.isInteger(addr) && addr >= 0 && addr <= 0xffff ? addr : 0;
}

// 每个轮询最近一次数据缓存（切换标签页时显示各自数据）
const pollDataCache = new Map<number, { addr: number; regs: number[] }>();
// 后台正在运行的轮询任务集合（前端镜像，用于停止/删除）
const runningPollIds = new Set<number>();

function currentPoll(): PollConfig {
  return polls.find((p) => p.id === activePollId) ?? polls[0];
}

/** 将面板控件值及当前表格各点位格式写回当前轮询配置。 */
function saveControlsToPoll() {
  const p = currentPoll();
  p.name = ($("pollName") as HTMLInputElement).value || p.name;
  p.unitId = Number(($("pollUnitId") as HTMLInputElement).value) || p.unitId;
  p.func = ($("pollFunc") as HTMLSelectElement).value;
  p.startAddr = parsePollStartAddr(($('pollStartAddr') as HTMLInputElement).value);
  p.count = Number(($("pollCount") as HTMLInputElement).value) || 1;
  p.period = Number(($("pollPeriod") as HTMLInputElement).value) || 500;
  p.writeValue = ($("pollValue") as HTMLInputElement).value || "";
  const boEl = $("pollByteOrder") as HTMLSelectElement | null;
  if (boEl) {
    p.byteOrder = boEl.value as ByteOrder;
  }
  p.rowStates = Object.fromEntries(rowStates.entries());
}

/** 动态显隐轮询面板下的各个操作框组 */
function updatePollControlsVisibility() {
  const func = ($("pollFunc") as HTMLSelectElement).value;
  const isWrite = ["05", "06", "15", "16"].includes(func);
  
  const countGroup = $("pollCountGroup");
  const valGroup = $("pollValGroup");
  const periodGroup = $("pollPeriodGroup");
  const autoPollBtn = $("chkAutoPoll");

  const pollByteOrderGroup = $("pollByteOrderGroup");
  if (pollByteOrderGroup) {
    if (func === "01" || func === "02") {
      pollByteOrderGroup.style.display = "none";
    } else {
      pollByteOrderGroup.style.display = "";
    }
  }
  
  if (isWrite) {
    valGroup.style.display = "";
    if (func === "05" || func === "06") {
      countGroup.style.display = "none";
    } else {
      countGroup.style.display = "";
      ($("pollCount") as HTMLInputElement).disabled = true;
    }
    if (periodGroup) periodGroup.style.display = "none";
    if (autoPollBtn) autoPollBtn.style.display = "none";
    
    // 切换至写入模式时，强行停止正在运行的读取轮询
    if (polling) {
      void stopAllPolls();
    }
  } else {
    valGroup.style.display = "none";
    countGroup.style.display = "";
    ($("pollCount") as HTMLInputElement).disabled = false;
    if (periodGroup) periodGroup.style.display = "inline-flex";
    if (autoPollBtn) autoPollBtn.style.display = "";
  }
}

/** 将轮询配置加载到面板控件，并恢复该轮询专属的点位格式设置。 */
function loadPollToControls(p: PollConfig) {
  $("pollName").value = p.name;
  $("pollUnitId").value = String(p.unitId);
  $("pollFunc").value = p.func;
  $("pollStartAddr").value = String(p.startAddr);
  $("pollCount").value = String(p.count);
  $("pollPeriod").value = String(p.period);
  $("pollValue").value = p.writeValue || "";
  const boEl = $("pollByteOrder") as HTMLSelectElement | null;
  if (boEl) {
    boEl.value = p.byteOrder || "abcd";
  }

  // 恢复该轮询的点位行状态
  rowStates.clear();
  if (p.rowStates) {
    for (const [addrStr, st] of Object.entries(p.rowStates)) {
      rowStates.set(Number(addrStr), { ...st });
    }
  }
  
  updatePollControlsVisibility();
  
  if (plcAddrMode) refreshAddrColumn(); // 功能码可能变化，刷新 PLC 地址换算
}

/** 渲染标签条：名称 + 寄存器范围小字。 */
function renderPollTabs() {
  const list = $("pollTabList");
  list.innerHTML = "";
  for (const p of polls) {
    const tab = document.createElement("button");
    tab.className = "poll-tab" + (p.id === activePollId ? " active" : "");
    tab.textContent = p.name;
    tab.title = `Unit ${p.unitId} · FC${p.func} · 寄存器 ${p.startAddr}~${p.startAddr + p.count - 1} ×${p.count} /${p.period}ms`;
    tab.addEventListener("click", () => activatePoll(p.id));
    list.appendChild(tab);
  }
}

/** 切换激活标签页：只切换视图，不中断任何正在后台运行的轮询。 */
function activatePoll(id: number) {
  if (id === activePollId) return;
  saveControlsToPoll();
  activePollId = id;
  renderPollTabs();
  loadPollToControls(currentPoll());
  const cached = pollDataCache.get(id);
  if (cached) {
    currentData = { addr: cached.addr, regs: cached.regs };
    updateRows(cached.addr, cached.regs);
  } else {
    currentData = null;
    const p = currentPoll();
    renderTable(p.startAddr, new Array(p.count).fill(0));
    showTableEmpty();
  }
}

/** 新建轮询标签页：继承当前配置（Unit ID 不自动 +1，由用户手动指定）。 */
function addPoll() {
  saveControlsToPoll();
  const src = currentPoll();
  const np: PollConfig = {
    id: nextPollId++,
    name: `轮询 ${polls.length + 1}`,
    unitId: src.unitId,
    func: src.func,
    startAddr: src.startAddr,
    count: src.count,
    period: src.period,
    byteOrder: src.byteOrder || "abcd",
    rowStates: {},
  };
  polls.push(np);
  activePollId = np.id;
  renderPollTabs();
  loadPollToControls(np);
  currentData = null;
  renderTable(np.startAddr, new Array(np.count).fill(0));
  showTableEmpty();
  log(`已添加「${np.name}」（Unit ID ${np.unitId}），共享当前通道`, "info");
}

/** 删除当前标签页；保留至少一个轮询。 */
function removePoll() {
  const idx = polls.findIndex((p) => p.id === activePollId);
  if (idx < 0) return;
  const removed = polls[idx];
  // 该轮询若在后台运行，先停止其任务
  if (runningPollIds.has(removed.id)) {
    invoke("stop_poll", { pollId: removed.id });
    runningPollIds.delete(removed.id);
  }
  polls.splice(idx, 1);
  if (polls.length === 0) {
    polls.push({ id: nextPollId++, name: "轮询 1", unitId: 1, func: "03", startAddr: 0, count: 10, period: 500, byteOrder: "abcd", rowStates: {} });
  }
  activePollId = polls[0].id;
  pollDataCache.delete(removed.id);
  renderPollTabs();
  loadPollToControls(currentPoll());
  const cached = pollDataCache.get(activePollId);
  if (cached) {
    currentData = { addr: cached.addr, regs: cached.regs };
    updateRows(cached.addr, cached.regs);
  } else {
    currentData = null;
    const p = currentPoll();
    renderTable(p.startAddr, new Array(p.count).fill(0));
    showTableEmpty();
  }
  setPolling(runningPollIds.size > 0);
}

// 控件变化 → 保存回配置（input 实时 + change 兜底）
["pollName", "pollUnitId", "pollFunc", "pollStartAddr", "pollCount", "pollPeriod", "pollValue", "pollByteOrder"].forEach((id) => {
  const el = $(id);
  if (!el) return;
  el.addEventListener("input", () => {
    saveControlsToPoll();
    const p = currentPoll();
    p.name = ($("pollName") as HTMLInputElement).value || p.name;
    renderPollTabs();
    if (id === "pollFunc") {
      updatePollControlsVisibility();
      if (plcAddrMode) refreshAddrColumn();
      if (currentData) {
        renderTable(currentData.addr, currentData.regs);
      } else {
        renderTable(p.startAddr, new Array(p.count).fill(0));
        showTableEmpty();
      }
    }
    if ((id === "pollStartAddr" || id === "pollCount") && !currentData) {
      renderTable(p.startAddr, new Array(p.count).fill(0));
      showTableEmpty();
    }
    if (id === "pollValue" && ["15", "16"].includes(p.func)) {
      const parts = ($("pollValue") as HTMLInputElement).value.split(",").map(s => s.trim()).filter(s => s.length > 0);
      if (parts.length > 0) {
        ($("pollCount") as HTMLInputElement).value = String(parts.length);
        saveControlsToPoll();
        renderPollTabs();
      }
    }
  });
  el.addEventListener("change", () => {
    saveControlsToPoll();
    if (id === "pollByteOrder") {
      if (currentData) {
        updateRows(currentData.addr, currentData.regs);
      } else {
        refreshTable();
      }
    }
  });
});

/** 启动当前已配置的所有轮询队列（各自独立后台任务，共享同一连接通道）。 */
async function startAllPolls() {
  if (!connected) { log("请先连接", "err"); return; }
  saveControlsToPoll();
  for (const p of polls) {
    // start_poll 返回 ()（Tauri resolve null），成功判定用 try/catch 而非返回值
    try {
      await invoke("start_poll", {
        pollId: p.id,
        pollName: p.name,
        func: p.func,
        addr: p.startAddr,
        count: p.count,
        intervalMs: p.period,
        unitId: p.unitId,
      });
      runningPollIds.add(p.id);
      log(`[${p.name}] 队列已启动 @${p.startAddr} ×${p.count} /${p.period}ms (Unit ${p.unitId}, FC${p.func})`, "ok");
    } catch (e) {
      log(`[${p.name}] 启动失败：${String(e)}`, "err");
    }
  }
  setPolling(runningPollIds.size > 0);
}

/** 停止所有轮询队列。 */
async function stopAllPolls() {
  await invoke("stop_all_polls");
  runningPollIds.clear();
  setPolling(false);
  log("所有轮询已停止", "info");
  showTableEmpty(); // 停止后清空数据：解析值/原始字显示 "—"
}

$("chkAutoPoll").addEventListener("click", async () => {
  if (!connected) {
    log("请先连接", "err");
    return;
  }
  if (polling) {
    await stopAllPolls();
  } else {
    await startAllPolls();
  }
});

/**
 * 依据 PDU 生成写入帧字节流
 */
function buildPollPdu(p: PollConfig): Uint8Array | null {
  const func = parseInt(p.func, 10);
  const startAddr = p.startAddr;
  const qty = p.count || 1;
  const valueStr = (p.writeValue || "").trim();

  if (isNaN(startAddr) || startAddr < 0 || startAddr > 65535) {
    return null;
  }

  const pdu: number[] = [];

  if (func === 1 || func === 2 || func === 3 || func === 4) {
    if (isNaN(qty) || qty < 1 || qty > 2000) {
      return null;
    }
    pdu.push(func);
    pdu.push(startAddr >> 8);
    pdu.push(startAddr & 0xFF);
    pdu.push(qty >> 8);
    pdu.push(qty & 0xFF);
  } else if (func === 5) {
    const val = parseInt(valueStr, 10);
    if (val !== 0 && val !== 1) {
      return null;
    }
    pdu.push(func);
    pdu.push(startAddr >> 8);
    pdu.push(startAddr & 0xFF);
    pdu.push(val === 1 ? 0xFF : 0x00);
    pdu.push(0x00);
  } else if (func === 6) {
    let val = 0;
    if (valueStr.toLowerCase().startsWith("0x")) {
      val = parseInt(valueStr, 16);
    } else {
      val = parseInt(valueStr, 10);
    }
    if (isNaN(val) || val < 0 || val > 65535) {
      return null;
    }
    pdu.push(func);
    pdu.push(startAddr >> 8);
    pdu.push(startAddr & 0xFF);
    pdu.push(val >> 8);
    pdu.push(val & 0xFF);
  } else if (func === 15) {
    const parts = valueStr.split(",").map(s => s.trim()).filter(s => s.length > 0);
    if (parts.length === 0) return null;
    const actualQty = parts.length;
    pdu.push(func);
    pdu.push(startAddr >> 8);
    pdu.push(startAddr & 0xFF);
    pdu.push(actualQty >> 8);
    pdu.push(actualQty & 0xFF);

    const byteCount = Math.ceil(actualQty / 8);
    pdu.push(byteCount);

    const bytes = new Uint8Array(byteCount);
    for (let i = 0; i < actualQty; i++) {
      const bitVal = parseInt(parts[i], 10);
      if (bitVal !== 0 && bitVal !== 1) return null;
      if (bitVal === 1) {
        const byteIndex = Math.floor(i / 8);
        const bitIndex = i % 8;
        bytes[byteIndex] |= (1 << bitIndex);
      }
    }
    for (let i = 0; i < byteCount; i++) {
      pdu.push(bytes[i]);
    }
  } else if (func === 16) {
    const parts = valueStr.split(",").map(s => s.trim()).filter(s => s.length > 0);
    if (parts.length === 0) return null;
    const actualQty = parts.length;
    pdu.push(func);
    pdu.push(startAddr >> 8);
    pdu.push(startAddr & 0xFF);
    pdu.push(actualQty >> 8);
    pdu.push(actualQty & 0xFF);

    const byteCount = actualQty * 2;
    pdu.push(byteCount);

    for (let i = 0; i < actualQty; i++) {
      let val = 0;
      if (parts[i].toLowerCase().startsWith("0x")) {
        val = parseInt(parts[i], 16);
      } else {
        val = parseInt(parts[i], 10);
      }
      if (isNaN(val) || val < 0 || val > 65535) return null;
      pdu.push(val >> 8);
      pdu.push(val & 0xFF);
    }
  }

  return new Uint8Array(pdu);
}

/**
 * 实时抓包数据更新至底层 PDU 帧分析追踪行
 */
function updateTraceInfo(tx: string, rx: string, rttMs: number | null, error?: string | null) {
  const fmtHex = (s: string) => {
    if (!s) return "--";
    if (s.includes(" ")) return s.trim().toUpperCase();
    const clean = s.replace(/[^0-9a-fA-F]/g, "");
    return clean.match(/.{1,2}/g)?.join(" ").toUpperCase() || "--";
  };
  $("txtPollSentAdu").textContent = fmtHex(tx);
  $("txtPollRecvAdu").textContent = error ? `Error: ${error}` : fmtHex(rx);
  $("txtPollRtt").textContent = rttMs !== null ? `${rttMs}ms` : "--";
}

/**
 * 单次发送：支持读指令与写指令执行
 */
async function sendOnce() {
  if (!connected) { log("请先连接（端口/串口已打开）", "err"); return; }
  const p = currentPoll();
  
  const isWrite = ["05", "06", "15", "16"].includes(p.func);
  if (isWrite) {
    const pdu = buildPollPdu(p);
    if (!pdu) {
      log("写入指令参数错误，请检查！", "err");
      return;
    }
    const pduHex = Array.from(pdu).map(b => b.toString(16).padStart(2, "0")).join("");
    try {
      const res = await invoke<{tx: string, rx: string, rtt_ms: number, pdu_resp: string}>("send_raw_frame", {
        unitId: p.unitId,
        hex: pduHex
      });
      logDivider("单次发送");
      logFrame({
        tx: `[写入指令] ${res.tx.toUpperCase()}`,
        rx: res.rx.toUpperCase(),
        rttMs: res.rtt_ms
      });
      updateTraceInfo(res.tx, res.rx, res.rtt_ms);
      log(`写入执行成功（FC${p.func}，Unit ${p.unitId}，起始地址 ${p.startAddr}）`, "ok");
    } catch (e) {
      log(`写入失败: ${e}`, "err");
      updateTraceInfo("", "", null, String(e));
    }
  } else {
    try {
      const res = await invoke<{ regs: number[]; tx: string; rx: string; rtt_ms: number }>("read_points", {
        unitId: p.unitId,
        func: p.func,
        addr: p.startAddr,
        count: p.count,
      });
      logDivider("单次发送");
      logFrame({
        tx: `[单次发送] ${res.tx}`,
        rx: res.rx,
        rttMs: res.rtt_ms,
      });
      updateTraceInfo(res.tx, res.rx, res.rtt_ms);
      if (res && res.regs && res.regs.length > 0) {
        currentData = { addr: p.startAddr, regs: res.regs };
        pollDataCache.set(p.id, { addr: p.startAddr, regs: res.regs });
        renderTable(p.startAddr, res.regs);
        log(`单次发送 @${p.startAddr} ×${p.count}（FC${p.func}，Unit ${p.unitId}）`, "ok");
      }
    } catch (e) {
      log(`单次发送失败：${String(e)}`, "err");
      updateTraceInfo("", "", null, String(e));
    }
  }
}

$("btnSendOnce").addEventListener("click", () => void sendOnce());

// ═══════════════════════════════════════════════════════════
// Data Table Rendering
// ═══════════════════════════════════════════════════════════

// 点位显示模式：false = 报文地址（0x00）；true = PLC 地址（40001…）
let plcAddrMode = false;

/** PLC 地址区起始值，按当前激活轮询的功能码（FC01 线圈 / FC02 离散输入 / FC03 保持 / FC04 输入）。 */
function plcBaseForFunc(): number {
  const fc = currentPoll().func;
  switch (fc) {
    case "01": return 1;      // 0x 线圈 00001~
    case "02": return 10001;  // 1x 触点 10001~
    case "04": return 30001;  // 3x 输入寄存器 30001~
    default: return 40001;    // 4x 保持寄存器 40001~
  }
}

/** 点位列显示：PLC 模式 → "40001"；报文模式 → "0x00"。 */
function addrLabel(addr: number): string {
  if (plcAddrMode) return String(plcBaseForFunc() + addr);
  return `0x${addr.toString(16).padStart(2, "0").toUpperCase()}`;
}

/** 重算表格中点位列的显示（切换模式后调用）。 */
function refreshAddrColumn() {
  const occ = computeOccupied();
  const tbody = $("dataTableBody");
  tbody.querySelectorAll("tr[data-addr]").forEach((tr) => {
    const a = Number((tr as HTMLElement).dataset.addr);
    (tr.children[0] as HTMLElement).textContent = addrLabel(a);
    // 被占用行的提示文本同样跟随点位显示模式
    const tdParsed = tr.children[4] as HTMLElement;
    if (tdParsed.dataset.occ === "1") {
      const by = occ.get(a);
      if (by !== undefined) tdParsed.textContent = `由点位 ${addrLabel(by)} 占用`;
    }
  });
}

/** u16 → "HH LL"（按 Modbus 字节序：高字节在前）。 */
function formatRawWord(reg: number): string {
  const hi = ((reg >> 8) & 0xff).toString(16).padStart(2, "0").toUpperCase();
  const lo = (reg & 0xff).toString(16).padStart(2, "0").toUpperCase();
  return `${hi} ${lo}`;
}

/** 按 byteOrder 重组 32 位格式的 4 个字节（缺省寄存器按 0 处理）。 */
function regBytes(
  regs: number[],
  offset: number,
  order: ByteOrder,
): [number, number, number, number] {
  const w1 = regs[offset] ?? 0;
  const w2 = regs[offset + 1] ?? 0;
  const b1hi = (w1 >> 8) & 0xff, b1lo = w1 & 0xff;
  const b2hi = (w2 >> 8) & 0xff, b2lo = w2 & 0xff;
  switch (order) {
    case "abcd": return [b1hi, b1lo, b2hi, b2lo];
    case "cdab": return [b2hi, b2lo, b1hi, b1lo];
    case "badc": return [b1lo, b1hi, b2lo, b2hi];
    case "dcba": return [b2lo, b2hi, b1lo, b1hi];
  }
}

/** 16 位二进制显示："0000 0000 0000 0101"。 */
function bin16(v: number): string {
  return (v & 0xffff)
    .toString(2)
    .padStart(16, "0")
    .replace(/(.{4})(?=.)/g, "$1 ");
}

/** 32 位二进制显示："0000 0000 ... 0101"（8 组单行，解析值列已加宽）。 */
function bin32(u: number): string {
  return (u >>> 0)
    .toString(2)
    .padStart(32, "0")
    .replace(/(.{4})(?=.)/g, "$1 ");
}

/** 按行 format + byteOrder 计算解析值。32 位格式需要 regs[offset+1]。 */
function parseReg(
  regs: number[],
  offset: number,
  fmt: RowFormat,
  order: ByteOrder,
): string {
  const func = currentPoll().func;
  if (func === "01" || func === "02") {
    return (regs[offset] ?? 0) !== 0 ? "true" : "false";
  }
  const v = regs[offset] ?? 0;
  switch (fmt) {
    case "u16":
      return String(v);
    case "i16":
      return String(v >= 0x8000 ? v - 0x10000 : v);
    case "u32":
    case "i32":
    case "f32": {
      if (regs.length < offset + 2) return "—";
      const bytes = regBytes(regs, offset, order);
      const u32 = ((bytes[0] << 24) | (bytes[1] << 16) | (bytes[2] << 8) | bytes[3]) >>> 0;
      if (fmt === "u32") return String(u32);
      if (fmt === "i32") return String(u32 >= 0x80000000 ? u32 - 0x100000000 : u32);
      const buf = new ArrayBuffer(4);
      const dv = new DataView(buf);
      dv.setUint8(0, bytes[0]); dv.setUint8(1, bytes[1]);
      dv.setUint8(2, bytes[2]); dv.setUint8(3, bytes[3]);
      return dv.getFloat32(0).toFixed(4);
    }
  }
}

/** 行当前解析值对应的二进制显示（按 format/byteOrder 取位模式）。 */
function parsedBinary(regs: number[], offset: number, fmt: RowFormat, order: ByteOrder): string {
  const func = currentPoll().func;
  if (func === "01" || func === "02") {
    return String(regs[offset] ?? 0);
  }
  if (is32Bit(fmt)) {
    if (regs.length < offset + 2) return "—";
    const bytes = regBytes(regs, offset, order);
    const u32 = ((bytes[0] << 24) | (bytes[1] << 16) | (bytes[2] << 8) | bytes[3]) >>> 0;
    return bin32(u32);
  }
  return bin16(regs[offset] ?? 0);
}

const FORMAT_OPTIONS: Array<{ value: RowFormat; label: string }> = [
  { value: "u16", label: "无符号 16 位" },
  { value: "i16", label: "有符号 16 位" },
  { value: "u32", label: "无符号 32 位" },
  { value: "i32", label: "有符号 32 位" },
  { value: "f32", label: "浮点数 32 位" },
];

const ORDER_OPTIONS: Array<{ value: ByteOrder; label: string }> = [
  { value: "abcd", label: "ABCD" },
  { value: "badc", label: "BADC" },
  { value: "cdab", label: "CDAB" },
  { value: "dcba", label: "DCBA" },
];

const is32Bit = (f: RowFormat) => f === "u32" || f === "i32" || f === "f32";

/** 16 位格式时禁用字节序下拉（保持灰色显示但不可选）。 */
function applyByteOrderLock(formatSel: HTMLSelectElement, orderSel: HTMLSelectElement) {
  const func = currentPoll().func;
  if (func === "01" || func === "02") {
    formatSel.disabled = true;
    orderSel.disabled = true;
  } else {
    const fmt = formatSel.value as RowFormat;
    orderSel.disabled = !is32Bit(fmt);
  }
}

function buildSelect(
  options: Array<{ value: string; label: string }>,
  selected: string,
  onChange: (newVal: string) => void,
): HTMLSelectElement {
  const sel = document.createElement("select");
  sel.className = "inline-select";
  for (const o of options) {
    const opt = document.createElement("option");
    opt.value = o.value;
    opt.textContent = o.label;
    if (o.value === selected) opt.selected = true;
    sel.appendChild(opt);
  }
  sel.addEventListener("change", () => onChange(sel.value));
  return sel;
}

/** 构建单行 <tr>。每行带独立的 format/byteOrder 下拉。 */
function buildRow(
  addr: number,
  reg: number,
  state: RowState,
  regsAll: number[],
  offset: number,
): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.dataset.addr = String(addr);

  // 点位：默认显示报文中的原始寄存器地址（0x00、0x01…）；
  // 点击「# PLC 地址」后切换为 PLC 地址（如 40001 = 4x 区 + 偏移）。
  const tdAddr = document.createElement("td");
  tdAddr.className = "col-addr";
  tdAddr.textContent = addrLabel(addr);
  tr.appendChild(tdAddr);

  // 原始字（HEX 字节）
  const tdRaw = document.createElement("td");
  tdRaw.className = "col-raw";
  tdRaw.textContent = formatRawWord(reg);
  tr.appendChild(tdRaw);

  // 显示格式下拉
  const tdFormat = document.createElement("td");
  tdFormat.className = "col-format";
  const func = currentPoll().func;
  const isBit = func === "01" || func === "02";
  
  let formatSel: HTMLSelectElement;
  if (isBit) {
    formatSel = buildSelect([{ value: "bit", label: "布尔型" }], "bit", () => {});
    formatSel.disabled = true;
  } else {
    formatSel = buildSelect(FORMAT_OPTIONS, state.format, (newFmt) => {
      const cur = rowStates.get(addr);
      if (cur) cur.format = newFmt as RowFormat;
      // 格式切换可能改变 32 位占用关系（占用/释放下一行）→ 整表刷新
      refreshTable();
    });
  }
  tdFormat.appendChild(formatSel);
  tr.appendChild(tdFormat);

  // 字节序下拉
  const tdOrder = document.createElement("td");
  tdOrder.className = "col-order";
  if (currentPoll().func === "01" || currentPoll().func === "02") {
    tdOrder.style.display = "none";
  }
  const orderSel = buildSelect(ORDER_OPTIONS, state.byteOrder, (newOrder) => {
    const cur = rowStates.get(addr);
    if (cur) cur.byteOrder = newOrder as ByteOrder;
    tdParsed.textContent = tdParsed.dataset.bin === "1"
      ? parsedBinary(regsAll, offset, formatSel.value as RowFormat, newOrder as ByteOrder)
      : parseReg(regsAll, offset, formatSel.value as RowFormat, newOrder as ByteOrder);
  });
  applyByteOrderLock(formatSel, orderSel);
  tdOrder.appendChild(orderSel);
  tr.appendChild(tdOrder);

  // 解析值：双击在「解析值 ↔ 二进制」间切换
  const tdParsed = document.createElement("td");
  tdParsed.className = "col-parsed";
  tdParsed.title = "双击切换 解析值 / 二进制";
  tdParsed.textContent = parseReg(regsAll, offset, state.format, state.byteOrder);
  tdParsed.addEventListener("dblclick", () => {
    if (tdParsed.dataset.bin === "1") {
      // 恢复解析值
      tdParsed.dataset.bin = "0";
      tdParsed.classList.remove("bin");
      tdParsed.textContent = parseReg(regsAll, offset, formatSel.value as RowFormat, orderSel.value as ByteOrder);
    } else {
      // 显示二进制位模式
      tdParsed.dataset.bin = "1";
      tdParsed.classList.add("bin");
      tdParsed.textContent = parsedBinary(regsAll, offset, formatSel.value as RowFormat, orderSel.value as ByteOrder);
    }
  });
  tr.appendChild(tdParsed);
  // 操作列：写入按钮（由 refreshWriteButtons 统一按功能码/占用关系维护）
  const tdOp = document.createElement("td");
  tdOp.className = "col-op";
  tr.appendChild(tdOp);

  return tr;
}

/** 默认 RowState：新行默认 U16 + 全局字序（格式完全按行独立）。 */
function defaultRowState(): RowState {
  return {
    format: "u16",
    byteOrder: ($("pollByteOrder") as HTMLSelectElement).value as ByteOrder,
  };
}

/**
 * 计算被 32 位格式行占用的寄存器：返回 { 被占用addr: 占用它的addr }。
 * 32 位格式（u32/i32/f32）占用两个寄存器——下一行被冻结并提示占用。
 * 被占用行自身不能再作为占用者（如 0x00→0x01，0x02→0x03 依次类推）。
 */
function computeOccupied(): Map<number, number> {
  const occ = new Map<number, number>();
  const addrs = [...rowStates.keys()].sort((a, b) => a - b);
  for (const addr of addrs) {
    if (occ.has(addr)) continue; // 本行已被占用，不能作为占用者
    const st = rowStates.get(addr);
    if (st && is32Bit(st.format)) occ.set(addr + 1, addr);
  }
  return occ;
}

/** 将占用逻辑应用到当前表格所有行：被占用行冻结下拉、解析值显示占用提示。 */
function applyOccupation() {
  const occ = computeOccupied();
  const tbody = $("dataTableBody");
  const func = currentPoll().func;
  const isBit = func === "01" || func === "02";

  // 控制表头字节序列的显隐
  const thOrder = document.querySelector(".data-table th.col-order") as HTMLElement | null;
  if (thOrder) {
    thOrder.style.display = isBit ? "none" : "";
  }

  tbody.querySelectorAll("tr[data-addr]").forEach((tr) => {
    const a = Number((tr as HTMLElement).dataset.addr);
    const fmtSel = tr.children[2].querySelector("select") as HTMLSelectElement;
    const orderSel = tr.children[3].querySelector("select") as HTMLSelectElement;
    const tdParsed = tr.children[4] as HTMLElement;
    const tdOrder = tr.children[3] as HTMLElement;

    if (tdOrder) {
      tdOrder.style.display = isBit ? "none" : "";
    }

    if (!fmtSel || !orderSel) return;
    if (isBit) {
      fmtSel.disabled = true;
      orderSel.disabled = true;
      tdParsed.dataset.occ = "0";
      tdParsed.classList.remove("occupied");
    } else {
      const occBy = occ.get(a);
      if (occBy !== undefined) {
        // 被占用：冻结两个下拉，解析值显示占用提示
        fmtSel.disabled = true;
        orderSel.disabled = true;
        tdParsed.dataset.occ = "1";
        tdParsed.classList.add("occupied");
        tdParsed.textContent = `由点位 ${addrLabel(occBy)} 占用`;
      } else {
        fmtSel.disabled = false;
        orderSel.disabled = !is32Bit(fmtSel.value as RowFormat);
        tdParsed.dataset.occ = "0";
        tdParsed.classList.remove("occupied");
      }
    }
  });
}

/** 数据变化或格式变化后整表刷新（依赖最近一次数据）。 */
function refreshTable() {
  if (currentData) updateRows(currentData.addr, currentData.regs);
}

/**
 * 维护操作列的「写入」按钮：
 * - 仅当前轮询功能码为 FC01（线圈）或 FC03（保持寄存器）时显示；
 * - 被 32 位格式占用的行不显示（其值由占用者写入）。
 */
function refreshWriteButtons() {
  const fc = currentPoll().func;
  const writable = fc === "01" || fc === "03";
  const occ = computeOccupied();
  const tbody = $("dataTableBody");
  tbody.querySelectorAll("tr[data-addr]").forEach((tr) => {
    const a = Number((tr as HTMLElement).dataset.addr);
    const tdOp = tr.children[5] as HTMLElement;
    tdOp.innerHTML = "";
    if (!writable || occ.has(a)) return;
    const btn = document.createElement("button");
    btn.className = "tb-btn tiny write-btn";
    btn.textContent = "写入";
    btn.title = `写入寄存器 ${a}（FC${fc}）`;
    btn.addEventListener("click", () => {
      const cur = rowStates.get(a) ?? defaultRowState();
      openWriteDialog(a, cur.format, cur.byteOrder, currentPoll().func);
    });
    tdOp.appendChild(btn);
  });
}

let lastRenderedFunc: string | null = null;

/** 全量渲染：清空表格，按当前 regs 重新构建。已有 addr 的 rowState 保留。 */
function renderTable(addr: number, regs: number[]) {
  lastRenderedFunc = currentPoll().func;
  const tbody = $("dataTableBody");
  tbody.innerHTML = "";
  for (let i = 0; i < regs.length; i++) {
    const a = addr + i;
    // 关键：每行落回默认时都要独立副本（{...}），否则多行共享同一对象，
    // 修改任一行格式会连带改动所有未单独设置过的行。
    const st = rowStates.get(a) ?? { ...defaultRowState() };
    rowStates.set(a, st);
    tbody.appendChild(buildRow(a, regs[i], st, regs, i));
  }
  applyOccupation(); // 32 位行占用下一行
  refreshWriteButtons(); // 操作列写入按钮
}

/** 部分更新：轮询新数据到达时刷新 raw 和 parsed，保留每行独立配置。 */
function updateRows(addr: number, regs: number[]) {
  const tbody = $("dataTableBody");
  const func = currentPoll().func;
  // 若行数变化，或者功能码发生了变化（例如从 03 切换到 01 且数量刚好相同）→ 全量重建。
  const existing = tbody.querySelectorAll("tr[data-addr]").length;
  if (existing !== regs.length || lastRenderedFunc !== func) {
    renderTable(addr, regs);
    return;
  }
  const occ = computeOccupied();
  for (let i = 0; i < regs.length; i++) {
    const a = addr + i;
    const tr = tbody.querySelector<HTMLTableRowElement>(`tr[data-addr="${a}"]`);
    if (!tr) continue;
    // 同样使用独立副本，避免共享默认对象被任一行改写
    const st = rowStates.get(a) ?? { ...defaultRowState() };
    rowStates.set(a, st);
    const tdRaw = tr.children[1] as HTMLElement;       // 原始字
    const tdFormat = tr.children[2] as HTMLElement;    // 显示格式
    const tdOrder = tr.children[3] as HTMLElement;     // 字节序
    const tdParsed = tr.children[4] as HTMLElement;    // 解析值
    (tr.children[0] as HTMLElement).textContent = addrLabel(a); // 点位（含 PLC 切换模式）
    tdRaw.textContent = formatRawWord(regs[i]);
    const fmtSel = tdFormat.querySelector("select") as HTMLSelectElement;
    const orderSel = tdOrder.querySelector("select") as HTMLSelectElement;

    const isBit = func === "01" || func === "02";
    if (isBit) {
      if (fmtSel) {
        fmtSel.value = "bit";
        fmtSel.disabled = true;
      }
      if (orderSel) orderSel.disabled = true;
      tdParsed.dataset.occ = "0";
      tdParsed.classList.remove("occupied");
    } else {
      if (fmtSel && fmtSel.value !== st.format) fmtSel.value = st.format;
      if (orderSel && orderSel.value !== st.byteOrder) orderSel.value = st.byteOrder;
      // 被 32 位行占用：冻结下拉、解析值显示占用提示（不显示数据）
      const occBy = occ.get(a);
      if (occBy !== undefined) {
        if (fmtSel) fmtSel.disabled = true;
        if (orderSel) orderSel.disabled = true;
        tdParsed.dataset.occ = "1";
        tdParsed.classList.add("occupied");
        tdParsed.textContent = `由点位 ${addrLabel(occBy)} 占用`;
        continue;
      }
      if (fmtSel) fmtSel.disabled = false;
      if (orderSel) orderSel.disabled = !is32Bit(fmtSel.value as RowFormat);
      tdParsed.dataset.occ = "0";
      tdParsed.classList.remove("occupied");
    }
    // 该行若处于二进制展开态，刷新时保持展开并用新数据重算，否则恢复解析值
    if (tdParsed.dataset.bin === "1") {
      tdParsed.textContent = parsedBinary(regs, i, fmtSel.value as RowFormat, orderSel.value as ByteOrder);
    } else {
      tdParsed.textContent = parseReg(regs, i, st.format, st.byteOrder);
    }
  }
  refreshWriteButtons(); // 操作列按钮（功能码/占用关系可能变化）
}

function clearTable() {
  $("dataTableBody").innerHTML = "";
  rowStates.clear();
}

/** 停止轮询后清空数据：保留行结构与格式配置，原始字/解析值显示 "—"；
 *  32 位占用行仍显示占用提示（与数据无关的配置语义）。 */
function showTableEmpty() {
  const tbody = $("dataTableBody");
  currentData = null;
  tbody.querySelectorAll("tr[data-addr]").forEach((tr) => {
    const tdRaw = tr.children[1] as HTMLElement;
    const tdParsed = tr.children[4] as HTMLElement;
    tdRaw.textContent = "—";
    tdParsed.textContent = "—";
    tdParsed.classList.remove("bin");
    delete tdParsed.dataset.bin;
  });
  applyOccupation(); // 占用行覆盖为"由点位 XX 占用"提示，非占用行保持 "—"
}

// ═══════════════════════════════════════════════════════════
// Backend Event Streams
// ═══════════════════════════════════════════════════════════

// 当前最新数据（用于全局配置变化时不破坏已有行时的不必要重渲染）
let currentData: { addr: number; regs: number[] } | null = null;

listen<{
  pollId: number;
  pollName?: string;
  addr: number;
  regs: number[] | null;
  tx: string;
  rx: string;
  rttMs: number | null;
  error?: string | null;
}>("poll-frame", (e) => {
  const { pollId, pollName, regs, tx, rx, rttMs, error, addr } = e.payload;
  const tag = pollName ? `[${pollName}] ` : "";
  // 帧日志（毫秒时间戳 + TX 蓝 / RX 绿 + RTT），多队列并发时按名称区分
  logFrame({ tx: tag + tx, rx, rttMs, error });
  
  if (pollId === activePollId) {
    updateTraceInfo(tx, rx, rttMs, error);
  }
  
  if (regs && regs.length > 0) {
    // 数据归属到该轮询自己的缓存；切换标签页时仍可回显各自数据
    pollDataCache.set(pollId, { addr, regs });
    // 仅当数据来自当前激活的轮询时才刷新表格
    if (pollId === activePollId) {
      currentData = { addr, regs };
      updateRows(addr, regs);
    }
    // 首帧轮询成功 → 连接状态由「端口已打开」升级为「已连接」
    if (awaitPollConnected) {
      awaitPollConnected = false;
      $("statusConn").textContent =
        connProto === "rtu" ? "RTU 已连接"
        : connProto === "udp" ? "UDP 已连接"
        : connProto === "rtuudp" ? "RTU-over-UDP 已连接"
        : "TCP 已连接";
    }
  }
});

// 注：模拟器实时快照（slave-values）由独立「模拟器」窗口监听（src/simulator-window.ts）。

// ═══════════════════════════════════════════════════════════
// Toolbar Buttons (non-connection)
// ═══════════════════════════════════════════════════════════

// PLC 地址 — 切换点位列显示（报文地址 0x00 ↔ PLC 地址 40001）
$("btnPlcAddr").addEventListener("click", () => {
  plcAddrMode = !plcAddrMode;
  $("btnPlcAddr").classList.toggle("active", plcAddrMode);
  refreshAddrColumn();
  if (plcAddrMode) {
    const base = plcBaseForFunc();
    log(`已切换为 PLC 地址显示：${base} ~ ${base + 9}（报文中为 0x00 起，区号隐含在功能码）`, "info");
  } else {
    log("已切换为报文地址显示：0x00、0x01…（再点一次恢复）", "info");
  }
});

// Add Poll — 新建多轮询标签页（工具栏按钮）
$("btnAddPoll").addEventListener("click", addPoll);
// 标签条右侧的 + 按钮同样新建
$("btnAddPollTab").addEventListener("click", addPoll);

// Delete Poll — 删除当前标签页（不影响其他正在运行的轮询）
$("btnDelPoll").addEventListener("click", () => {
  removePoll();
  log("当前轮询已删除", "info");
});

// ═══════════════════════════════════════════════════════════
// Project Save / Import — 保存与导入完整工程配置
// ═══════════════════════════════════════════════════════════

interface ProjectFile {
  version: number;
  appName: string;
  savedAt: string;
  connection: {
    mode: ConnMode;
    host: string;
    port: number;
    transport: ConnTransport;
    rtuOverIpTransport: ConnTransport;
    rtuPort: string;
    rtuBaud: number;
    rtuDataBits: number;
    rtuStopBits: number;
    rtuParity: string;
    timeoutMs: number;
    retries: number;
    interFrameMs: string;
  };
  settings: {
    plcAddrMode: boolean;
  };
  activePollIndex: number;
  polls: Array<{
    name: string;
    unitId: number;
    func: string;
    startAddr: number;
    count: number;
    period: number;
    writeValue?: string;
    byteOrder?: ByteOrder;
    rowStates?: Record<number, RowState>;
  }>;
}

async function saveProject() {
  saveControlsToPoll();
  const activeIdx = polls.findIndex((p) => p.id === activePollId);

  const project: ProjectFile = {
    version: 1,
    appName: "ModbusTool",
    savedAt: new Date().toISOString(),
    connection: {
      mode: selectedMode,
      host: ($("connHost") as HTMLInputElement).value || "192.168.0.10",
      port: Number(($("connPort") as HTMLInputElement).value) || 502,
      transport: selectedTransport,
      rtuOverIpTransport: rtuOverIpTransport,
      rtuPort: ($("rtuPort") as HTMLSelectElement).value || "",
      rtuBaud: Number(($("rtuBaud") as HTMLSelectElement).value) || 9600,
      rtuDataBits: Number(($("rtuDataBits") as HTMLSelectElement).value) || 8,
      rtuStopBits: Number(($("rtuStopBits") as HTMLSelectElement).value) || 1,
      rtuParity: ($("rtuParity") as HTMLSelectElement).value || "none",
      timeoutMs: Number(($("connTimeout") as HTMLInputElement).value) || 1000,
      retries: Number(($("connRetries") as HTMLInputElement).value) || 1,
      interFrameMs: ($("connInterFrame") as HTMLInputElement).value || "",
    },
    settings: {
      plcAddrMode: plcAddrMode,
    },
    activePollIndex: activeIdx >= 0 ? activeIdx : 0,
    polls: polls.map((p) => {
      const rStates = p.id === activePollId ? Object.fromEntries(rowStates.entries()) : (p.rowStates || {});
      return {
        name: p.name,
        unitId: p.unitId,
        func: p.func,
        startAddr: p.startAddr,
        count: p.count,
        period: p.period,
        writeValue: p.writeValue,
        byteOrder: p.byteOrder || "abcd",
        rowStates: rStates,
      };
    }),
  };

  try {
    const jsonStr = JSON.stringify(project, null, 2);
    const savedPath = await invoke<string>("save_project_file", { content: jsonStr });
    if (savedPath) {
      log(`项目已保存至：${savedPath}（共 ${project.polls.length} 个轮询，已导出所有寄存器点位配置）`, "ok");
    } else {
      log("已取消保存项目", "info");
    }
  } catch (e) {
    log(`保存项目失败：${String(e)}`, "err");
  }
}

async function importProject() {
  try {
    const content = await invoke<string>("import_project_file");
    if (!content) {
      log("已取消导入项目", "info");
      return;
    }

    let data: any;
    try {
      data = JSON.parse(content);
    } catch (e) {
      uiAlert("无法解析项目文件，JSON 格式可能已损坏。", "导入错误");
      return;
    }

    if (!data || !Array.isArray(data.polls) || data.polls.length === 0) {
      uiAlert("项目文件格式无效或不包含任何轮询配置。", "导入错误");
      return;
    }

    // 若当前正在轮询，先停止所有后台轮询
    if (polling) {
      await safe(invoke("stop_all_polls"));
      runningPollIds.clear();
      setPolling(false);
    }

    // 1. 恢复连接配置
    if (data.connection) {
      const conn = data.connection;
      if (conn.mode && ["tcp", "rtu", "rtuotcp"].includes(conn.mode)) {
        selectedMode = conn.mode as ConnMode;
        const radio = document.querySelector<HTMLInputElement>(`input[name='connMode'][value='${selectedMode}']`);
        if (radio) radio.checked = true;
        applyConnMode();
      }
      if (conn.host !== undefined) ($("connHost") as HTMLInputElement).value = String(conn.host);
      if (conn.port !== undefined) ($("connPort") as HTMLInputElement).value = String(conn.port);
      if (conn.transport && ["tcp", "udp"].includes(conn.transport)) {
        selectedTransport = conn.transport as ConnTransport;
        ($("connTransport") as HTMLSelectElement).value = conn.transport;
      }
      if (conn.rtuOverIpTransport && ["tcp", "udp"].includes(conn.rtuOverIpTransport)) {
        rtuOverIpTransport = conn.rtuOverIpTransport as ConnTransport;
        ($("rtuOverIpTransport") as HTMLSelectElement).value = conn.rtuOverIpTransport;
      }
      if (conn.rtuPort !== undefined) ($("rtuPort") as HTMLSelectElement).value = String(conn.rtuPort);
      if (conn.rtuBaud !== undefined) ($("rtuBaud") as HTMLSelectElement).value = String(conn.rtuBaud);
      if (conn.rtuDataBits !== undefined) ($("rtuDataBits") as HTMLSelectElement).value = String(conn.rtuDataBits);
      if (conn.rtuStopBits !== undefined) ($("rtuStopBits") as HTMLSelectElement).value = String(conn.rtuStopBits);
      if (conn.rtuParity !== undefined) ($("rtuParity") as HTMLSelectElement).value = String(conn.rtuParity);
      if (conn.timeoutMs !== undefined) ($("connTimeout") as HTMLInputElement).value = String(conn.timeoutMs);
      if (conn.retries !== undefined) ($("connRetries") as HTMLInputElement).value = String(conn.retries);
      if (conn.interFrameMs !== undefined) ($("connInterFrame") as HTMLInputElement).value = String(conn.interFrameMs);
    }

    // 2. 恢复全局显示设置
    if (data.settings && typeof data.settings.plcAddrMode === "boolean") {
      plcAddrMode = data.settings.plcAddrMode;
      $("btnPlcAddr").classList.toggle("active", plcAddrMode);
    }

    // 3. 恢复轮询配置
    polls.length = 0;
    pollDataCache.clear();
    for (let i = 0; i < data.polls.length; i++) {
      const p = data.polls[i];
      const np: PollConfig = {
        id: nextPollId++,
        name: p.name || `轮询 ${i + 1}`,
        unitId: Number(p.unitId) || 1,
        func: p.func || "03",
        startAddr: Number(p.startAddr) || 0,
        count: Number(p.count) || 10,
        period: Number(p.period) || 500,
        writeValue: p.writeValue || "",
        byteOrder: p.byteOrder || "abcd",
        rowStates: p.rowStates || {},
      };
      polls.push(np);
    }

    // 4. 激活指定轮询标签
    let targetIdx = 0;
    if (typeof data.activePollIndex === "number" && data.activePollIndex >= 0 && data.activePollIndex < polls.length) {
      targetIdx = data.activePollIndex;
    }
    activePollId = polls[targetIdx].id;

    // 5. 渲染标签并载入界面与表格
    renderPollTabs();
    loadPollToControls(currentPoll());
    currentData = null;
    renderTable(currentPoll().startAddr, new Array(currentPoll().count).fill(0));
    showTableEmpty();

    log(`成功导入项目：恢复了 ${polls.length} 个轮询及所有寄存器点位格式配置`, "ok");
  } catch (e) {
    log(`导入项目失败：${String(e)}`, "err");
  }
}

// 绑定保存与导入项目按钮
$("btnSaveProject").addEventListener("click", () => void saveProject());
$("btnLoadProject").addEventListener("click", () => void importProject());

// Trace 展开隐藏按钮
$("btnToggleTrace").addEventListener("click", () => {
  const content = $("pollTraceContent") as HTMLElement;
  const btn = $("btnToggleTrace") as HTMLButtonElement;
  if (content.style.display === "none") {
    content.style.display = "flex";
    btn.textContent = "隐藏";
  } else {
    content.style.display = "none";
    btn.textContent = "显示";
  }
});

// 模拟器按钮：打开独立的「模拟器」窗口（已开则聚焦，不重复创建）。
// 窗口内：无已配置通道 → 显示「设置运行模式」；已运行/已配置 → 直接显示「寄存器设置」。
$("btnSimulator").addEventListener("click", async () => {
  const existing = await WebviewWindow.getByLabel("simulator");
  if (existing) {
    await existing.show();
    await existing.setFocus();
    return;
  }
  const win = new WebviewWindow("simulator", {
    url: "simulator.html",
    title: "模拟器 · Modbus Tool",
    width: 1095,
    height: 800,
    minWidth: 900,
    minHeight: 600,
    resizable: true,
  });
  win.once("tauri://error", (e) => {
    log(`打开「模拟器」窗口失败：${JSON.stringify(e.payload)}`, "err");
  });
});

// 主窗口关闭时，回收模拟器相关窗口，确保资源正确释放。
void getCurrentWindow().onCloseRequested(async () => {
  const sim = await WebviewWindow.getByLabel("simulator");
  if (sim) {
    try {
      await sim.close();
    } catch {
      /* 窗口可能已关闭 */
    }
  }
});

// ═══════════════════════════════════════════════════════════
// Command Builder (指令生成已合并至轮询面板)
// ═══════════════════════════════════════════════════════════

// 初始化：渲染多轮询标签条 + 加载第一个轮询配置 + 初始表格空态渲染
renderPollTabs();
loadPollToControls(polls[0]);
renderTable(polls[0].startAddr, new Array(polls[0].count).fill(0));
showTableEmpty();
// 初始化连接对话框模式字段显示
applyConnMode();

// 禁用 WebView 右键菜单：macOS WKWebView 默认右键含 Reload（刷新页面丢失状态），一并屏蔽。
// 但在通信日志区域（#logOutput）内放行，改显示自定义菜单（导出到 TXT / 清空日志）。
document.addEventListener("contextmenu", (e) => {
  // 通信日志右键 → 显示自定义菜单
  const logEl = $("logOutput");
  if (logEl.contains(e.target as Node) || e.target === logEl) {
    e.preventDefault();
    showLogContextMenu(e.clientX, e.clientY);
    return;
  }
  e.preventDefault();
});

// ── 通信日志自定义右键菜单 ──
let ctxMenuEl: HTMLDivElement | null = null;

/** 关闭当前打开的右键菜单（如有）。 */
function closeLogContextMenu() {
  if (ctxMenuEl) {
    ctxMenuEl.remove();
    ctxMenuEl = null;
  }
}

/** 在指定坐标显示通信日志右键菜单。 */
function showLogContextMenu(x: number, y: number) {
  closeLogContextMenu();

  const menu = document.createElement("div");
  menu.className = "ctx-menu";
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;

  // 导出到 TXT
  const itemExport = document.createElement("div");
  itemExport.className = "ctx-menu-item";
  itemExport.textContent = "导出到 TXT";
  itemExport.addEventListener("click", () => {
    closeLogContextMenu();
    void exportLogToTxt();
  });
  menu.appendChild(itemExport);

  // 清空日志
  const sep = document.createElement("div");
  sep.className = "ctx-menu-sep";
  menu.appendChild(sep);

  const itemClear = document.createElement("div");
  itemClear.className = "ctx-menu-item danger";
  itemClear.textContent = "清空日志";
  itemClear.addEventListener("click", () => {
    closeLogContextMenu();
    $("logOutput").innerHTML = "";
    log("通信日志已清空", "info");
  });
  menu.appendChild(itemClear);

  document.body.appendChild(menu);
  ctxMenuEl = menu;

  // 防止菜单溢出窗口右/下边界
  const rect = menu.getBoundingClientRect();
  if (rect.right > window.innerWidth) {
    menu.style.left = `${x - rect.width}px`;
  }
  if (rect.bottom > window.innerHeight) {
    menu.style.top = `${y - rect.height}px`;
  }
}

// 点击菜单外或按 ESC 关闭菜单
document.addEventListener("click", () => closeLogContextMenu());
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeLogContextMenu();
});

/** 收集通信日志全部文本行，调用后端保存为 .txt 文件。 */
async function exportLogToTxt() {
  const el = $("logOutput");
  const lines: string[] = [];
  el.querySelectorAll("div").forEach((d) => {
    const text = d.textContent?.trim();
    if (text) lines.push(text);
  });
  if (lines.length === 0) {
    log("通信日志为空，无可导出内容", "info");
    return;
  }
  const content = lines.join("\n");
  try {
    const savedPath = await invoke<string>("export_log_txt", { content });
    if (savedPath) {
      log(`通信日志已导出至：${savedPath}`, "ok");
    } else {
      log("已取消导出", "info");
    }
  } catch (e) {
    log(`导出失败：${String(e)}`, "err");
  }
}

log("Modbus Tool 就绪 — 点击「连接」开始", "info");
