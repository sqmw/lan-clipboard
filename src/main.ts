import { invoke } from "@tauri-apps/api/core";

type Settings = {
  limits: {
    max_item_bytes: number;
  };
  sync: {
    device_id: string;
    device_code: string;
    enabled: boolean;
    listen_port: number;
    peers: string[];
    poll_interval_ms: number;
  };
  security: {
    encryption_enabled: boolean;
    require_pairing_code: boolean;
    pairing_code: string;
  };
};

type RuntimeStatus = {
  running: boolean;
  device_id: string;
  last_error: string | null;
  recent_log_count: number;
};

type DiscoveredDevice = {
  device_id: string;
  device_code: string;
  addr: string;
  port: number;
};

type RuntimeLog = {
  ts_ms: number;
  level: string;
  message: string;
};

const getInput = (id: string): HTMLInputElement =>
  document.querySelector(`#${id}`) as HTMLInputElement;
const getTextArea = (id: string): HTMLTextAreaElement =>
  document.querySelector(`#${id}`) as HTMLTextAreaElement;
const getText = (id: string): HTMLElement =>
  document.querySelector(`#${id}`) as HTMLElement;

let settings: Settings;
let lastDiscovered: DiscoveredDevice[] = [];

async function loadSettings(): Promise<void> {
  settings = await invoke<Settings>("get_settings");
  getInput("poll-interval-ms").value = String(settings.sync.poll_interval_ms);
  getTextArea("peers").value = settings.sync.peers.join("\n");
  getInput("sync-enabled").checked = settings.sync.enabled;
  getInput("encryption-enabled").checked = settings.security.encryption_enabled;
  getInput("require-pairing-code").checked = settings.security.require_pairing_code;
  getInput("pairing-code").value = settings.security.pairing_code;
  getInput("max-item-bytes").value = String(settings.limits.max_item_bytes);
  getText("self-device-code").textContent = settings.sync.device_code || "-";
}

function collectSettings(): Settings {
  const peers = getTextArea("peers")
    .value.split("\n")
    .map((line) => line.trim())
    .filter(Boolean);

  return {
    ...settings,
    limits: {
      max_item_bytes: Number(getInput("max-item-bytes").value),
    },
    sync: {
      ...settings.sync,
      enabled: getInput("sync-enabled").checked,
      peers,
      poll_interval_ms: Number(getInput("poll-interval-ms").value),
    },
    security: {
      encryption_enabled: getInput("encryption-enabled").checked,
      require_pairing_code: getInput("require-pairing-code").checked,
      pairing_code: getInput("pairing-code").value.trim(),
    },
  };
}

async function saveSettings(): Promise<void> {
  settings = collectSettings();
  await invoke("set_settings", { next: settings });
}

async function refreshStatus(): Promise<void> {
  const status = await invoke<RuntimeStatus>("sync_status");
  getText("status-running").textContent = `状态: ${status.running ? "运行中" : "已停止"}`;
  getText("status-device-id").textContent = `设备ID: ${status.device_id}`;
  getText("status-log-count").textContent = `日志条数: ${status.recent_log_count}`;
  getText("status-error").textContent = `最近错误: ${status.last_error ?? "无"}`;
}

async function startSync(): Promise<void> {
  await saveSettings();
  await invoke("start_sync");
  await refreshStatus();
}

async function stopSync(): Promise<void> {
  await invoke("stop_sync");
  await refreshStatus();
}

function renderDevices(devices: DiscoveredDevice[]): void {
  const container = getText("discovered-devices");
  lastDiscovered = devices;
  if (!devices.length) {
    container.innerHTML = "<p>未发现可用设备。</p>";
    return;
  }
  const rows = devices
    .map(
      (device, index) =>
        `<div class="device-row"><span>设备码 ${device.device_code || "??????"} (${device.addr}:${device.port})</span><button data-index="${index}" type="button">加入</button></div>`,
    )
    .join("");
  container.innerHTML = rows;
  container.querySelectorAll("button").forEach((button) => {
    button.addEventListener("click", () => {
      const index = Number((button as HTMLButtonElement).dataset.index ?? "-1");
      const target = devices[index];
      if (!target) return;
      addPeer(`${target.addr}:${target.port}`);
    });
  });
}

async function scanDevices(): Promise<void> {
  const devices = await invoke<DiscoveredDevice[]>("discover_devices");
  renderDevices(devices);
}

function addPeer(peer: string): void {
  const current = getTextArea("peers")
    .value.split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
  if (!current.includes(peer)) {
    current.push(peer);
    getTextArea("peers").value = current.join("\n");
  }
}

async function connectByCode(): Promise<void> {
  const code = getInput("peer-device-code").value.trim();
  if (!code) return;
  const found = lastDiscovered.find((device) => device.device_code === code);
  if (!found) {
    getText("status-error").textContent = "最近错误: 未找到该设备码，请先手动扫描设备";
    return;
  }
  addPeer(`${found.addr}:${found.port}`);
  await saveSettings();
  await refreshStatus();
}

async function refreshLogs(): Promise<void> {
  const logs = await invoke<RuntimeLog[]>("get_runtime_logs", { limit: 300 });
  const lines = logs.map((log) => {
    const stamp = new Date(log.ts_ms).toLocaleTimeString();
    return `[${stamp}] [${log.level}] ${log.message}`;
  });
  getText("runtime-logs").textContent = lines.join("\n");
}

async function clearLogs(): Promise<void> {
  await invoke("clear_runtime_logs");
  await refreshLogs();
  await refreshStatus();
}

async function boot(): Promise<void> {
  await loadSettings();
  await refreshStatus();
  await refreshLogs();
  if (settings.sync.enabled) {
    await startSync();
  }
}

window.addEventListener("DOMContentLoaded", () => {
  document.querySelector("#save-settings")?.addEventListener("click", async () => {
    await saveSettings();
    await refreshStatus();
  });
  document.querySelector("#start-sync")?.addEventListener("click", startSync);
  document.querySelector("#stop-sync")?.addEventListener("click", stopSync);
  document.querySelector("#refresh-status")?.addEventListener("click", refreshStatus);
  document.querySelector("#scan-devices")?.addEventListener("click", scanDevices);
  document.querySelector("#connect-by-code")?.addEventListener("click", connectByCode);
  document.querySelector("#refresh-logs")?.addEventListener("click", refreshLogs);
  document.querySelector("#clear-logs")?.addEventListener("click", clearLogs);
  boot().catch((error) => {
    getText("status-error").textContent = `最近错误: ${String(error)}`;
  });
});
