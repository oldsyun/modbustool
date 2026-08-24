import { invoke } from "@tauri-apps/api/core";

const $ = <T extends HTMLElement = HTMLElement>(id: string): T =>
  document.getElementById(id) as T;

let scanning = false;

function parseAddress(value: string): number | null {
  const text = value.trim();
  const address = /^0x[0-9a-f]+$/i.test(text)
    ? parseInt(text.slice(2), 16)
    : Number(text);
  return Number.isInteger(address) && address >= 0 && address <= 0xffff ? address : null;
}

function setScanning(active: boolean) {
  scanning = active;
  $("startScan").toggleAttribute("disabled", active);
  $("stopScan").toggleAttribute("disabled", !active);
  $("scanAddress").toggleAttribute("disabled", active);
}

function setResponseCount(n: number) {
  $("responseCount").textContent = String(n);      // 头部概览：实际响应数（绿色色块数）
}

/** 「开始扫描」按钮前方实时累计：已扫描的 Unit ID 数（含无响应）。 */
function setScannedCount(n: number) {
  $("responseCountInline").textContent = String(n);
}

function addResponse(unitId: number) {
  const dot = document.createElement("div");
  dot.className = "scan-dot";
  dot.title = `Unit ID ${unitId} 在指定地址有响应`;
  dot.textContent = String(unitId);
  $("scanDots").appendChild(dot);
  setResponseCount($("scanDots").children.length);
}

async function scan() {
  const address = parseAddress(($<HTMLInputElement>("scanAddress")).value);
  if (address === null) {
    $("scanStatus").textContent = "地址无效";
    $("scanAddress").focus();
    return;
  }
  $("scanDots").innerHTML = "";
  setResponseCount(0);
  setScannedCount(0);
  $("scanEmpty").textContent = "正在依次探测 Unit ID...";
  $("scanStatus").textContent = "扫描中";
  setScanning(true);
  try {
    for (let unitId = 1; unitId <= 247 && scanning; unitId++) {
      setScannedCount(unitId); // 实时累计已扫描（含无响应）
      try {
        await invoke("read_points", { unitId, func: "03", addr: address, count: 1 });
        addResponse(unitId);
      } catch {
        // 超时、异常响应和 Modbus 异常都表示本次 Unit ID 未响应。
      }
    }
  } finally {
    const stopped = !scanning;
    setScanning(false);
    $("scanStatus").textContent = stopped ? "已停止" : "扫描完成";
    if ($("scanDots").children.length === 0) {
      $("scanEmpty").textContent = stopped ? "扫描已停止，暂无响应" : "未发现响应的 Unit ID";
    }
  }
}

$("startScan").addEventListener("click", () => { if (!scanning) void scan(); });
$("stopScan").addEventListener("click", () => { scanning = false; });