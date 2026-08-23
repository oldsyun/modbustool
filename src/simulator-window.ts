// 模拟器（内置从站）—— 独立窗口实现，含两个视图：
//  · 视图「设置运行模式」(mode)  ：选择并连接一个从站通道（与「通道设置」一致的连接操作）。
//  · 视图「寄存器设置」(reg)     ：运行模式启停 + Unit/寄存器增删改 + 实时值。
// 窗口可独立关闭，关闭不停止后端从站；主窗口关闭时由主进程统一回收。
//
// 模块化：支持同时启用 5 种运行模式（TCP / UDP / RTU over TCP / RTU over UDP / RTU 串口），
// 各模式共享同一份 Unit 镜像，可独立启停、运行时增删寄存器即时生效。

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

type RegArea = "holding" | "input" | "coil" | "discrete";
type SimMode = "tcp" | "udp" | "rtu_tcp" | "rtu_udp" | "rtu";

interface RegisterDef {
  id: string;
  area: RegArea;
  addr: number;
  name: string;
  dtype: string; // u16 | i16 | u32 | i32 | f32 | bit
  access: string; // rw | ro
  vary: string; // off | sine | random | increment
  created_at_ms?: number;
}

type Snapshot = {
  coils: Record<string, boolean>;
  discreteInputs: Record<string, boolean>;
  inputRegisters: Record<string, number>;
  holdingRegisters: Record<string, number>;
};

type RegListResp = { unit_id: number; defs: RegisterDef[]; snapshot: Snapshot };
type UnitView = { unit_id: number; reg_count: number };
// slave-values 事件载荷：Snapshot 四区 + 所属 unitId
type SlaveValuesEvent = Snapshot & { unitId?: number };
type SlaveStatus = {
  modes: Partial<Record<SimMode, boolean>>;
  tcpConns: number;
  configs: Record<string, Record<string, unknown>>;
};

const $ = <T extends HTMLElement = HTMLElement>(id: string): T =>
  document.getElementById(id) as T;

const AREA_LABEL: Record<string, string> = {
  holding: "保持寄存器",
  input: "输入寄存器",
  coil: "线圈",
  discrete: "离散输入",
};
const DTYPE_LABEL: Record<string, string> = {
  u16: "U16",
  i16: "I16",
  u32: "U32",
  i32: "I32",
  f32: "F32",
  bit: "BOOL",
  bits: "位图",
};

function toBitString(val: number): string {
  let s = "";
  for (let i = 0; i < 16; i++) {
    s += (val >> i) & 1;
  }
  return s;
}
const VARY_LABEL: Record<string, string> = {
  off: "关闭",
  sine: "正弦",
  random: "随机",
  increment: "自增",
};
const MODE_LABEL: Record<SimMode, string> = {
  tcp: "Modbus TCP",
  udp: "Modbus UDP",
  rtu_tcp: "RTU over TCP",
  rtu_udp: "RTU over UDP",
  rtu: "RTU（串口）",
};
const isBitArea = (a: string) => a === "coil" || a === "discrete";

// ── 运行期状态 ────────────────────────────────────────
let activeUnitId = 1;
let units: UnitView[] = [];
let defs: RegisterDef[] = [];
let snap: Snapshot | null = null;
let tabArea: RegArea = "holding";
let selectedMode: SimMode = "tcp";
let runningModes: Partial<Record<SimMode, boolean>> = {};
let tcpConns = 0;
let slaveConfigs: Record<string, any> = {};
let editingId: string | null = null;
let pendingReplace: { dupId: string; input: Record<string, unknown> } | null = null;
let pendingDeleteId: string | null = null;
let inited = false;

// ── 工具 ──────────────────────────────────────────────
// 注意：后端大量命令返回 `Result<(), String>`，Tauri 会把 `()` 序列化为 `null`。
// 因此 safe() 出错时返回 `undefined`（而非 null），以便调用点用 `ok !== undefined`
// 同时区分「成功(null)」与「失败(undefined)」，避免把成功的 `()` 误判为错误。
async function safe<T>(p: Promise<T>): Promise<T | undefined> {
  try {
    return await p;
  } catch (e) {
    showSimError(String(e));
    return undefined;
  }
}

/** 临时以红色在状态栏提示错误，2.6s 后由 refreshStatus 复原。 */
function showSimError(msg: string) {
  const el = $("simModeStatus");
  el.textContent = msg;
  el.style.color = "var(--danger)";
  window.setTimeout(() => {
    void refreshStatus();
  }, 2600);
}

function displayValue(d: RegisterDef, raw: number): number {
  if (d.dtype === "i16" && (raw & 0x8000) !== 0) return raw - 0x10000;
  return raw;
}

function rawValue(d: RegisterDef, v: number): number {
  let n = Math.round(v);
  if (d.dtype === "i16" && n < 0) n += 0x10000;
  return Math.max(0, Math.min(65535, n));
}

function liveOf(d: RegisterDef): number {
  if (!snap) return 0;
  const key = String(d.addr);
  if (isBitArea(d.area)) {
    const m = (d.area === "coil" ? snap.coils : snap.discreteInputs) ?? {};
    return m[key] ? 1 : 0;
  }
  const m = (d.area === "holding" ? snap.holdingRegisters : snap.inputRegisters) ?? {};
  return m[key] ?? 0;
}

// ── 视图切换（现整合成弹窗） ──────────────────────────
function openSimConnDialog(): void {
  applySimMode();
  ($("simConnDialog") as HTMLDialogElement).showModal();
}
function closeSimConnDialog(): void {
  ($("simConnDialog") as HTMLDialogElement).close();
}

// ── 运行模式（mode 视图：连接通道） ───────────────────
function applySimMode(): void {
  const checked = document.querySelector<HTMLInputElement>(
    'input[name="simMode"]:checked',
  );
  if (checked) selectedMode = checked.value as SimMode;
  document.querySelectorAll<HTMLElement>(".radio-item[data-simmode]").forEach((el) => {
    el.classList.toggle("active", el.dataset.simmode === selectedMode);
  });
  document.querySelectorAll<HTMLElement>(".sim-fields[data-sim-for]").forEach((el) => {
    const show = (el.dataset.simFor ?? "").split(/\s+/).includes(selectedMode);
    el.classList.toggle("hidden", !show);
  });
  syncModeFields();
  updateModeHint();
}

const MODE_HINT: Record<SimMode, string> = {
  tcp: "<b>Modbus TCP 从站</b>：绑定本机地址并监听端口，主站经标准 MBAP 帧访问所有 Unit。",
  udp: "<b>Modbus UDP 从站</b>：以 UDP 数据报承载 MBAP 帧（无连接），适合局域网快速轮询。",
  rtu_tcp: "<b>RTU over TCP</b>：在 TCP 流上承载 RTU 帧（无 MBAP 头），适配串口服务器/透传网关。",
  rtu_udp: "<b>RTU over UDP</b>：每个 UDP 数据报承载一个完整 RTU 帧，适配网络转串口设备。",
  rtu: "<b>Modbus RTU（串口）</b>：以本机串口作为子站，需独占串口。",
};

function updateModeHint(): void {
  $("simModeHint").innerHTML = MODE_HINT[selectedMode];
}

async function refreshStatus(): Promise<void> {
  const st = await safe<SlaveStatus>(invoke("sim_slave_status"));
  if (!st) return;
  runningModes = st.modes ?? {};
  tcpConns = st.tcpConns ?? 0;
  slaveConfigs = st.configs ?? {};
  
  syncModeFields();
  renderModeStatus();
  renderModePanel();
}

function renderModeStatus(): void {
  const running = !!runningModes[selectedMode];
  const el = $("simModeStatus");
  el.textContent = running ? `${MODE_LABEL[selectedMode]} · 已连接` : "未连接";
  el.style.color = running ? "var(--accent)" : "";
  const btn = $("simConnect") as HTMLButtonElement;
  btn.textContent = running ? "■ 断开通道" : "⇄ 连接通道";
  btn.classList.toggle("primary", !running);
  btn.classList.toggle("connected", running);
}

function syncModeFields(): void {
  const running = !!runningModes[selectedMode];
  const cfg = slaveConfigs[selectedMode];
  if (cfg && running) {
    if (selectedMode === "rtu") {
      if (cfg.portName) ($("simRtuPortName") as HTMLSelectElement).value = cfg.portName;
      if (cfg.baudRate) ($("simRtuBaud") as HTMLSelectElement).value = String(cfg.baudRate);
      if (cfg.dataBits) ($("simRtuDataBits") as HTMLSelectElement).value = String(cfg.dataBits);
      if (cfg.stopBits) ($("simRtuStopBits") as HTMLSelectElement).value = String(cfg.stopBits);
      if (cfg.parity) ($("simRtuParity") as HTMLSelectElement).value = cfg.parity;
      if (cfg.interFrameMs !== undefined) ($("simRtuInter") as HTMLInputElement).value = cfg.interFrameMs === null ? "" : String(cfg.interFrameMs);
    } else {
      if (cfg.bind) ($("simBind") as HTMLInputElement).value = cfg.bind;
      if (cfg.port) ($("simPort") as HTMLInputElement).value = String(cfg.port);
    }
  }
  
  document.querySelectorAll<HTMLElement>(".sim-fields").forEach(div => {
    div.querySelectorAll("input, select").forEach(el => {
      (el as HTMLInputElement | HTMLSelectElement).disabled = running;
    });
  });
}

function renderModePanel(): void {
  const panel = $("simModePanel");
  if (!panel) return;
  panel.innerHTML = "";
  // 仅显示当前选中的运行模式
  const modes: SimMode[] = [selectedMode];
  for (const m of modes) {
    const running = !!runningModes[m];
    const cfg = slaveConfigs[m];
    const row = document.createElement("div");
    row.className = "sim-mode-row" + (running ? " running" : "");
    const label = document.createElement("span");
    label.className = "sim-mode-name";
    
    let detail = "";
    if (cfg && running) {
      if (m === "rtu") {
        detail = `${cfg.portName} ${cfg.baudRate} ${cfg.dataBits}${cfg.parity?.charAt(0).toUpperCase() || 'N'}${cfg.stopBits}`;
      } else {
        detail = `端口: ${cfg.port}`;
      }
    }
    
    label.textContent = MODE_LABEL[m] + (detail ? ` (${detail})` : "");
    if (m === "tcp") {
      const c = document.createElement("small");
      c.textContent = tcpConns ? `（${tcpConns} 连接）` : "";
      label.appendChild(c);
    }
    const btn = document.createElement("button");
    btn.className = "tb-btn small primary" + (running ? " connected" : "");
    btn.textContent = running ? "停止模拟" : "启动模拟";
    btn.addEventListener("click", () => void toggleMode(m));
    row.appendChild(label);
    row.appendChild(btn);
    panel.appendChild(row);
  }
}

/** reg 视图中点击某模式的连接/停止。未配置则打开通道设置弹窗。 */
async function toggleMode(mode: SimMode): Promise<void> {
  if (runningModes[mode]) {
    const ok = await safe(invoke("sim_slave_stop", { mode }));
    if (ok !== undefined) await refreshStatus();
    return;
  }
  const st = await safe<SlaveStatus>(invoke("sim_slave_status"));
  const cfg = st?.configs?.[mode];
  if (cfg && Object.keys(cfg).length > 0) {
    const ok = await safe(invoke("sim_slave_start", { req: cfg }));
    if (ok !== undefined) await refreshStatus();
  } else {
    selectedMode = mode;
    openSimConnDialog();
  }
}

async function connectSelected(): Promise<void> {
  const mode = selectedMode;
  const args: Record<string, unknown> = { mode };
  if (mode === "rtu") {
    const portName = ($("simRtuPortName") as HTMLSelectElement).value;
    if (!portName) {
      showSimError("请先选择串口");
      return;
    }
    args.portName = portName;
    args.baudRate = Number(($("simRtuBaud") as HTMLSelectElement).value);
    args.dataBits = Number(($("simRtuDataBits") as HTMLSelectElement).value);
    args.stopBits = Number(($("simRtuStopBits") as HTMLSelectElement).value);
    args.parity = ($("simRtuParity") as HTMLSelectElement).value;
    const inter = ($("simRtuInter") as HTMLInputElement).value.trim();
    args.interFrameMs = inter === "" ? null : Number(inter);
  } else {
    args.bind = ($("simBind") as HTMLInputElement).value.trim() || "0.0.0.0";
    args.port = Number(($("simPort") as HTMLInputElement).value) || 502;
  }
  // 关键修复：后端 sim_slave_start(req: SlaveStartReq)，JS 必须以 { req: {...} } 形式传参。
  const ok = await safe(invoke("sim_slave_start", { req: args }));
  if (ok !== undefined) {
    await refreshStatus();
    closeSimConnDialog();
  }
}

// ── Unit ID 管理 ─────────────────────────────────────
async function loadUnits(): Promise<void> {
  const res = await safe<UnitView[]>(invoke("unit_list"));
  if (!res) return;
  units = res.sort((a, b) => a.unit_id - b.unit_id);
  if (!units.some((u) => u.unit_id === activeUnitId)) {
    activeUnitId = units[0]?.unit_id ?? 1;
  }
  renderUnitChips();
  await loadRegs();
}

function renderUnitChips(): void {
  const wrap = $("simUnitChips");
  wrap.innerHTML = "";
  for (const u of units) {
    const chip = document.createElement("span");
    chip.className = "live-tab" + (u.unit_id === activeUnitId ? " active" : "");
    chip.textContent = `Unit ${u.unit_id}`;
    chip.dataset.unitId = String(u.unit_id);
    chip.addEventListener("click", () => {
      activeUnitId = u.unit_id;
      renderUnitChips();
      void loadRegs();
    });
    wrap.appendChild(chip);
  }
}

async function addUnit(): Promise<void> {
  const v = Number(($("simUnitInput") as HTMLInputElement).value);
  if (!v || v < 1 || v > 247) {
    showSimError("Unit ID 须在 1 ~ 247");
    return;
  }
  const ok = await safe(invoke("unit_add", { unitId: v }));
  if (ok !== undefined) {
    ($("simUnitInput") as HTMLInputElement).value = "";
    await loadUnits();
    if (units.some((u) => u.unit_id === v)) {
      activeUnitId = v;
      renderUnitChips();
      await loadRegs();
    }
  }
}

async function removeUnit(): Promise<void> {
  if (units.length <= 1) {
    showSimError("至少保留一个 Unit ID");
    return;
  }
  const ok = await safe(invoke("unit_remove", { unitId: activeUnitId }));
  if (ok !== undefined) await loadUnits();
}

// ── 寄存器 查询 / 渲染 ───────────────────────────────
async function loadRegs(): Promise<void> {
  const res = await safe<RegListResp>(
    invoke("sim_reg_list", { keyword: null, unitId: activeUnitId }),
  );
  if (!res) return;
  defs = res.defs;
  snap = res.snapshot;
  renderLiveTable();
}

function areaCounts(): Record<RegArea, number> {
  const c: Record<RegArea, number> = { holding: 0, input: 0, coil: 0, discrete: 0 };
  for (const d of defs) c[d.area]++;
  return c;
}

function visibleDefs(): RegisterDef[] {
  return defs
    .filter((d) => d.area === tabArea)
    .sort((a, b) => a.addr - b.addr);
}

function renderLiveTable(): void {
  const tbody = $("simRegTableBody");
  tbody.innerHTML = "";
  const c = areaCounts();
  ($("simCntHolding") as HTMLElement).textContent = c.holding ? `(${c.holding})` : "";
  ($("simCntInput") as HTMLElement).textContent = c.input ? `(${c.input})` : "";
  ($("simCntCoil") as HTMLElement).textContent = c.coil ? `(${c.coil})` : "";
  ($("simCntDiscrete") as HTMLElement).textContent = c.discrete ? `(${c.discrete})` : "";
  $("simRegCount").textContent = defs.length ? `（共 ${defs.length} 个）` : "";

  const items = visibleDefs();
  if (items.length === 0) {
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 8;
    td.className = "live-empty";
    td.textContent = `「${AREA_LABEL[tabArea]}」暂无寄存器，点击「+ 新增寄存器」添加。`;
    tr.appendChild(td);
    tbody.appendChild(tr);
    return;
  }
  for (const d of items) tbody.appendChild(buildRow(d));
}

function buildRow(d: RegisterDef): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.dataset.addr = String(d.addr);
  tr.dataset.id = d.id;
  const isBit = isBitArea(d.area);
  const raw = liveOf(d);

  const addrTd = document.createElement("td");
  addrTd.className = "sim-addr";
  addrTd.textContent = `0x${d.addr.toString(16).toUpperCase().padStart(4, "0")} (${d.addr})`;
  tr.appendChild(addrTd);

  const nameTd = document.createElement("td");
  nameTd.textContent = d.name || "—";
  tr.appendChild(nameTd);

  const typeTd = document.createElement("td");
  typeTd.textContent = DTYPE_LABEL[d.dtype] ?? d.dtype;
  tr.appendChild(typeTd);

  const accessTd = document.createElement("td");
  accessTd.textContent = d.access === "ro" ? "只读" : "读写";
  tr.appendChild(accessTd);

  const varyTd = document.createElement("td");
  varyTd.textContent = d.area === "holding" ? VARY_LABEL[d.vary] ?? d.vary : "—";
  if (d.area === "holding" && d.vary && d.vary !== "off") {
    varyTd.style.color = "var(--accent)";
  }
  tr.appendChild(varyTd);

  const valTd = document.createElement("td");
  const hexTd = document.createElement("td");
  hexTd.className = "sim-hex";

  if (isBit) {
    valTd.textContent = raw !== 0 ? "ON" : "OFF";
    hexTd.textContent = raw !== 0 ? "0xFF00" : "0x0000";
  } else if (d.dtype === "bits") {
    valTd.style.fontFamily = "monospace";
    valTd.style.letterSpacing = "1px";
    valTd.textContent = toBitString(raw);
    hexTd.textContent = "0x" + raw.toString(16).toUpperCase().padStart(4, "0");
  } else {
    const inp = document.createElement("input");
    inp.type = "number";
    inp.className = "sim-num";
    inp.value = String(displayValue(d, raw));
    inp.addEventListener("change", async () => {
      const v = rawValue(d, parseFloat(inp.value) || 0);
      const fn = d.area === "holding" ? "sim_set_register" : "sim_set_input";
      await safe(invoke(fn, { addr: d.addr, value: v, unitId: activeUnitId }));
      inp.value = String(displayValue(d, v));
      hexTd.textContent = "0x" + v.toString(16).toUpperCase().padStart(4, "0");
    });
    valTd.appendChild(inp);
    hexTd.textContent = "0x" + raw.toString(16).toUpperCase().padStart(4, "0");
  }
  tr.appendChild(valTd);
  tr.appendChild(hexTd);

  const opTd = document.createElement("td");
  const editBtn = document.createElement("button");
  editBtn.className = "tb-btn small";
  editBtn.textContent = "编辑";
  editBtn.addEventListener("click", () => openRegForm(d));
  opTd.appendChild(editBtn);

  const delBtn = document.createElement("button");
  delBtn.className = "tb-btn small danger";
  delBtn.textContent = "删除";
  delBtn.addEventListener("click", () => askDelete(d));
  opTd.appendChild(delBtn);
  tr.appendChild(opTd);

  return tr;
}

/** 仅刷新值/HEX 单元格（slave-values 高频更新）：跳过正在编辑的行，避免打断输入。 */
function refreshLiveValues(): void {
  if (!snap) return;
  const tbody = $("simRegTableBody");
  const active = document.activeElement as HTMLElement | null;
  tbody.querySelectorAll<HTMLTableRowElement>("tr[data-addr]").forEach((tr) => {
    const id = tr.dataset.id;
    if (!id) return;
    const d = defs.find((x) => x.id === id);
    if (!d || d.area !== tabArea) return;
    if (active && tr.contains(active)) return;
    const raw = liveOf(d);
    const valTd = tr.children[5] as HTMLElement;
    const hexTd = tr.children[6] as HTMLElement;
    if (isBitArea(d.area)) {
      // 移除线圈/离散输入高频值与 hex 实时更新
    } else if (d.dtype === "bits") {
      valTd.textContent = toBitString(raw);
      hexTd.textContent = "0x" + raw.toString(16).toUpperCase().padStart(4, "0");
    } else {
      const inp = valTd.querySelector("input") as HTMLInputElement | null;
      if (inp) inp.value = String(displayValue(d, raw));
      hexTd.textContent = "0x" + raw.toString(16).toUpperCase().padStart(4, "0");
    }
  });
}

// ── 寄存器 增 / 改（含重复地址「替换」引导） ───────────
function openRegForm(d?: RegisterDef): void {
  editingId = d ? d.id : null;
  $("simRegFormTitle").textContent = d ? "编辑寄存器" : "新增寄存器";
  $("simRegFormError").textContent = "";
  // 数量字段：编辑时隐藏，新增时显示
  const countWrap = document.querySelector<HTMLElement>(".reg-fld-count");
  if (countWrap) countWrap.style.display = d ? "none" : "";
  if (!d) ($("srCount") as HTMLInputElement).value = "1";

  const area: RegArea = d ? d.area : tabArea; // 默认当前 tab 应用区域
  $("srArea").value = area;
  syncRegFormArea(area); // 优先同步区域设置

  if (d) {
    ($("srAddr") as HTMLInputElement).value = String(d.addr);
    ($("srName") as HTMLInputElement).value = d.name;
    ($("srDtype") as HTMLSelectElement).value = d.dtype;
    ($("srAccess") as HTMLSelectElement).value = d.access;
    ($("srVary") as HTMLSelectElement).value = d.vary || "off";
    const raw = liveOf(d);
    if (isBitArea(area)) {
      setSrValueControl("checkbox", raw !== 0);
    } else if (d.dtype === "bits") {
      setSrValueControl("bits", raw);
    } else {
      setSrValueControl("number", displayValue(d, raw));
    }
  } else {
    // 新增：地址自动建议为当前 tab 区域的下一个空闲地址
    const usedAddrs = new Set(defs.filter(d2 => d2.area === area).map(d2 => d2.addr));
    let suggested = 0;
    while (usedAddrs.has(suggested) && suggested < 65535) suggested++;
    
    ($("srAddr") as HTMLInputElement).value = String(suggested);
    ($("srName") as HTMLInputElement).value = "";
    ($("srDtype") as HTMLSelectElement).value = "u16";
    ($("srAccess") as HTMLSelectElement).value = area === "input" || area === "discrete" ? "ro" : "rw";
    ($("srVary") as HTMLSelectElement).value = "off";
    setSrValueControl("number", 0);
  }
  ($("simRegDialog") as HTMLDialogElement).showModal();
}

function syncRegFormArea(area: RegArea): void {
  const dtypeSel = $("srDtype") as HTMLSelectElement;
  const varySel = $("srVary") as HTMLSelectElement;
  const varyWrap = $("srVaryWrap");
  const varyHint = $("srVaryHint");

  if (isBitArea(area)) {
    dtypeSel.disabled = true;
    dtypeSel.value = "bit";
    varySel.disabled = true;
    varySel.value = "off";
    varyWrap.classList.add("hidden");
    // 用滑动开关表达布尔量，不再显示文字提示。
    varyHint.classList.add("hidden");
    setSrValueControl("checkbox", false);
  } else {
    dtypeSel.disabled = false;
    dtypeSel.value = "u16";
    if (area === "input") ($("srAccess") as HTMLSelectElement).value = "ro";
    // 保持寄存器与输入寄存器都支持自动变化（输入寄存器对主站只读，但模拟器
    // 可自行驱动数值变化）；线圈/离散为布尔量，不支持。
    const varyOk = area === "holding" || area === "input";
    varySel.disabled = !varyOk;
    if (!varyOk) varySel.value = "off";
    varyWrap.classList.remove("hidden");
    varyHint.classList.remove("hidden");
    varyHint.textContent =
      area === "holding"
        ? "每个寄存器可独立设置自动变化；多字类型按单字原始值存储。"
        : "输入寄存器对主站只读，可设置自动变化驱动数值。";
    setSrValueControl("number", 0);
  }
}

function setSrValueControl(kind: "number" | "checkbox" | "bits", value: number | boolean): void {
  const fld = $("srValControl");
  if (kind === "checkbox") {
    fld.innerHTML = `
      <label class="switch-control" style="margin-top: 4px;">
        <input id="srValue" type="checkbox" />
        <span class="switch-slider"></span>
      </label>
    `;
    ($("srValue") as HTMLInputElement).checked = !!value;
  } else if (kind === "bits") {
    let html = '<div class="bits-grid" style="display: grid; grid-template-columns: repeat(4, 1fr); gap: 10px; width: 100%; margin-top: 10px;">';
    const valNum = typeof value === "number" ? value : 0;
    for (let i = 0; i < 16; i++) {
      const bitOn = (valNum >> i) & 1;
      html += `
        <div class="bit-item" style="display: flex; align-items: center; gap: 8px;">
          <span style="font-size: 11px; font-weight: 500; min-width: 24px; color: var(--text-muted);">B${i}:</span>
          <label class="switch-control">
            <input type="checkbox" class="sr-bit-checkbox" data-bit="${i}" ${bitOn ? "checked" : ""} />
            <span class="switch-slider"></span>
          </label>
        </div>
      `;
    }
    html += '</div>';
    fld.innerHTML = html;
  } else {
    fld.innerHTML = '<input id="srValue" type="number" value="0" style="width:110px;" />';
    ($("srValue") as HTMLInputElement).value = String(value);
  }
}

function collectRegInput(area: RegArea, dtype: string): Record<string, unknown> {
  let value = 0;
  if (isBitArea(area)) {
    const el = $("srValue") as HTMLInputElement;
    value = el.checked ? 1 : 0;
  } else if (dtype === "bits") {
    let val = 0;
    const fld = $("srValControl");
    fld.querySelectorAll<HTMLInputElement>(".sr-bit-checkbox").forEach((cb) => {
      const bit = parseInt(cb.dataset.bit!, 10);
      if (cb.checked) {
        val |= 1 << bit;
      }
    });
    value = val;
  } else {
    const el = $("srValue") as HTMLInputElement;
    value = rawValue({ area, dtype } as RegisterDef, parseFloat(el.value) || 0);
  }

  const addr = parseInt(($("srAddr") as HTMLInputElement).value, 10);

  return {
    area,
    addr,
    name: ($("srName") as HTMLInputElement).value.trim(),
    dtype,
    access: ($("srAccess") as HTMLSelectElement).value,
    vary: area === "holding" || area === "input" ? ($("srVary") as HTMLSelectElement).value : "off",
    value,
  };
}

async function saveRegForm(): Promise<void> {
  const area = ($("srArea") as HTMLSelectElement).value as RegArea;
  const dtype = ($("srDtype") as HTMLSelectElement).value;

  const rawAddr = parseInt(($("srAddr") as HTMLInputElement).value, 10);
  if (isNaN(rawAddr) || rawAddr < 0 || rawAddr > 65535) {
    $("simRegFormError").textContent = "地址须为 0 ~ 65535";
    return;
  }

  const input = collectRegInput(area, dtype);

  // 如果是新增且数量 > 1，走批量接口
  if (!editingId) {
    const count = parseInt(($("srCount") as HTMLInputElement).value, 10) || 1;
    if (count > 1) {
      const el = $("srValue") as HTMLInputElement;
      const initValue = isBitArea(area) ? (el.checked ? 1 : 0) : (parseFloat(el.value) || 0);
      const added = await safe<number>(invoke("sim_reg_add_batch", {
        area,
        startAddr: input.addr as number,
        count,
        namePrefix: ($("srName") as HTMLInputElement).value.trim(),
        dtype: dtype,
        access: ($("srAccess") as HTMLSelectElement).value,
        vary: area === "holding" || area === "input" ? ($("srVary") as HTMLSelectElement).value : "off",
        initValue,
        unitId: activeUnitId,
      }));
      if (added !== undefined) {
        $("simRegFormError").textContent = "";
        ($("simRegDialog") as HTMLDialogElement).close();
      }
      return;
    }
  }

  const dup = defs.find(
    (d) => d.id !== editingId && d.area === area && d.addr === (input.addr as number),
  );
  if (dup && !editingId) {
    pendingReplace = { dupId: dup.id, input: input as Record<string, unknown> };
    $("simReplaceMsg").textContent =
      `${AREA_LABEL[area]} 0x${(input.addr as number).toString(16).toUpperCase().padStart(4, "0")}「${dup.name || "未命名"}」已存在。是否用新配置替换它？`;
    ($("simReplaceDialog") as HTMLDialogElement).showModal();
    return;
  }
  await doSave(input as Record<string, unknown>);
}

async function doSave(input: Record<string, unknown>): Promise<void> {
  try {
    if (editingId) {
      await invoke("sim_reg_update", { id: editingId, input, unitId: activeUnitId });
    } else {
      await invoke("sim_reg_add", { input, unitId: activeUnitId });
    }
    ($("simRegDialog") as HTMLDialogElement).close();
    // 后端会广播 sim-regs-updated → 自动刷新本表
  } catch (e) {
    $("simRegFormError").textContent = String(e);
  }
}

async function doReplace(): Promise<void> {
  if (!pendingReplace) return;
  try {
    await invoke("sim_reg_update", {
      id: pendingReplace.dupId,
      input: pendingReplace.input,
      unitId: activeUnitId,
    });
    ($("simReplaceDialog") as HTMLDialogElement).close();
    ($("simRegDialog") as HTMLDialogElement).close();
  } catch (e) {
    $("simRegFormError").textContent = String(e);
    ($("simReplaceDialog") as HTMLDialogElement).close();
  }
  pendingReplace = null;
}

// ── 寄存器 删 ─────────────────────────────────────────
function askDelete(d: RegisterDef): void {
  pendingDeleteId = d.id;
  $("simDelMsg").textContent =
    `确定要删除 ${AREA_LABEL[d.area]} 0x${d.addr.toString(16).toUpperCase().padStart(4, "0")}` +
    `「${d.name || "未命名"}」吗？删除后该地址读回为 0。`;
  ($("simDelDialog") as HTMLDialogElement).showModal();
}

async function doDelete(): Promise<void> {
  if (!pendingDeleteId) return;
  const ok = await safe(
    invoke("sim_reg_delete", { id: pendingDeleteId, unitId: activeUnitId }),
  );
  if (ok !== undefined) ($("simDelDialog") as HTMLDialogElement).close();
  pendingDeleteId = null;
}

// ── 串口列表 ──────────────────────────────────────────
async function refreshSerialPorts(): Promise<void> {
  const ports = await safe<string[]>(invoke("list_serial_ports"));
  const sel = $("simRtuPortName") as HTMLSelectElement;
  sel.innerHTML = "";
  if (ports && ports.length) {
    for (const p of ports) {
      const o = document.createElement("option");
      o.value = p;
      o.textContent = p;
      sel.appendChild(o);
    }
  } else {
    const o = document.createElement("option");
    o.value = "";
    o.textContent = "无可用串口";
    sel.appendChild(o);
  }
}

// ── 事件订阅（一次性） ────────────────────────────────
function subscribe(): void {
  listen<SlaveValuesEvent>("slave-values", (e) => {
    if (e.payload.unitId !== undefined && e.payload.unitId !== activeUnitId) return;
    snap = e.payload;
    refreshLiveValues();
  });

  listen<RegListResp>("sim-regs-updated", (e) => {
    if (e.payload.unit_id !== activeUnitId) return;
    defs = e.payload.defs;
    snap = e.payload.snapshot;
    renderLiveTable();
  });
}

// ── 首屏视图决策 ──────────────────────────────────────
async function decideInitialView(): Promise<void> {
  void loadUnits();
  const st = await safe<SlaveStatus>(invoke("sim_slave_status"));
  if (st) {
    runningModes = st.modes ?? {};
    tcpConns = st.tcpConns ?? 0;
    slaveConfigs = st.configs ?? {};
    renderModePanel();
    const anyRunning = Object.values(runningModes).some(Boolean);
    const hasConfig = st.configs && Object.keys(st.configs).length > 0;
    if (!anyRunning && !hasConfig) {
      openSimConnDialog();
    }
  }
  void refreshSerialPorts();
}

// ── 初始化：绑定所有事件（仅一次） ────────────────────
export function initSimulatorWindow(): void {
  if (inited) return;
  inited = true;

  subscribe();

  // 解决 macOS WebKit 销毁窗口时，若页面存在滚动元素，可能触发 WebCore::ScrollingTree::takePendingScrollUpdates 崩溃的 Bug
  void getCurrentWindow().onCloseRequested(async (event) => {
    event.preventDefault();
    try {
      document.body.style.overflow = "hidden";
      document.body.innerHTML = "";
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 30));
    void getCurrentWindow().destroy();
  });

  // 通道设置弹窗绑定
  $("btnSimConnSettings").addEventListener("click", () => {
    openSimConnDialog();
  });
  $("simConnClose").addEventListener("click", () => {
    closeSimConnDialog();
  });
  $("simConnCancel").addEventListener("click", () => {
    closeSimConnDialog();
  });

  $("simWinClose").addEventListener("click", () => {
    void getCurrentWindow().close();
  });

  // 模式 radio
  document.querySelectorAll<HTMLInputElement>('input[name="simMode"]').forEach((r) => {
    r.addEventListener("change", () => {
      if (!r.checked) return;
      selectedMode = r.value as SimMode;
      applySimMode();
    });
  });

  // 连接 / 断开通道
  $("simConnect").addEventListener("click", () => {
    if (runningModes[selectedMode]) void safe(invoke("sim_slave_stop", { mode: selectedMode })).then(() => refreshStatus());
    else void connectSelected();
  });

  // 串口刷新
  $("simBtnRefreshPorts").addEventListener("click", () => void refreshSerialPorts());

  // Unit 管理
  $("simUnitAddBtn").addEventListener("click", () => void addUnit());
  $("simUnitDelBtn").addEventListener("click", () => void removeUnit());
  ($("simUnitInput") as HTMLInputElement).addEventListener("keydown", (e) => {
    if (e.key === "Enter") void addUnit();
  });

  // 区域标签
  document.querySelectorAll<HTMLElement>("#simAreaTabs .live-tab").forEach((tab) => {
    tab.addEventListener("click", () => {
      document.querySelectorAll("#simAreaTabs .live-tab").forEach((t) => t.classList.remove("active"));
      tab.classList.add("active");
      tabArea = tab.dataset.area as RegArea;
      renderLiveTable();
    });
  });

  // 寄存器 新增 / 种子 / 清零
  $("simRegAddBtn").addEventListener("click", () => openRegForm());
  $("simRegSeed").addEventListener("click", async () => {
    const ok = await safe(invoke("sim_reg_seed", { unitId: activeUnitId }));
    if (ok !== undefined) await loadRegs();
  });
  $("simReset").addEventListener("click", async () => {
    const ok = await safe(invoke("sim_reset", { unitId: activeUnitId }));
    if (ok !== undefined) await loadRegs();
  });

  // Excel 导入 / 导出
  $("simRegExportBtn").addEventListener("click", async () => {
    const path = await safe<string>(invoke("sim_reg_export_xlsx", { unitId: activeUnitId }));
    if (path !== undefined && path) showSimError(`已导出到: ${path}`);
  });
  $("simRegImportBtn").addEventListener("click", async () => {
    const result = await safe<[number, number]>(invoke("sim_reg_import_xlsx", {
      unitId: activeUnitId,
      replace: false,
    }));
    if (result !== undefined && (result[0] > 0 || result[1] > 0)) {
      showSimError(`导入完成：成功 ${result[0]} 条，跳过 ${result[1]} 条`);
    }
  });

  ($("srArea") as HTMLSelectElement).addEventListener("change", () => {
    syncRegFormArea(($("srArea") as HTMLSelectElement).value as RegArea);
  });
  ($("srDtype") as HTMLSelectElement).addEventListener("change", () => {
    const dtype = ($("srDtype") as HTMLSelectElement).value;
    if (dtype === "bits") {
      setSrValueControl("bits", 0);
    } else {
      setSrValueControl("number", 0);
    }
  });
  $("simRegFormSave").addEventListener("click", () => void saveRegForm());
  $("simRegFormCancel").addEventListener("click", () =>
    ($("simRegDialog") as HTMLDialogElement).close(),
  );
  $("simRegFormClose").addEventListener("click", () =>
    ($("simRegDialog") as HTMLDialogElement).close(),
  );

  // 重复地址「替换」
  $("simReplaceOk").addEventListener("click", () => void doReplace());
  $("simReplaceCancel").addEventListener("click", () => {
    ($("simReplaceDialog") as HTMLDialogElement).close();
    pendingReplace = null;
  });
  $("simReplaceClose").addEventListener("click", () => {
    ($("simReplaceDialog") as HTMLDialogElement).close();
    pendingReplace = null;
  });

  // 删除确认
  $("simDelOk").addEventListener("click", () => void doDelete());
  $("simDelCancel").addEventListener("click", () =>
    ($("simDelDialog") as HTMLDialogElement).close(),
  );
  $("simDelClose").addEventListener("click", () =>
    ($("simDelDialog") as HTMLDialogElement).close(),
  );

  // 首屏：依据是否已有运行/配置的通道决定显示「设置运行模式」还是「寄存器设置」
  void decideInitialView();
  void refreshSerialPorts();
}

document.addEventListener("DOMContentLoaded", () => {
  initSimulatorWindow();
});
