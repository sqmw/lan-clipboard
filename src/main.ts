import { invoke } from "@tauri-apps/api/core";

type Settings = {
  limits: {
    max_item_bytes: number;
  };
  sync: {
    device_id: string;
    shared_code: string;
    enabled: boolean;
    listen_port: number;
    peers: string[];
    poll_interval_ms: number;
  };
  security: {
    encryption_enabled: boolean;
    pairing_code: string;
  };
};

type RuntimeStatus = {
  running: boolean;
  device_id: string;
  shared_code: string;
  last_error: string | null;
  recent_log_count: number;
  peer_count: number;
};

type DiscoveredDevice = {
  device_id: string;
  device_name: string;
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
let statusTimer: number | null = null;
let observedMemberCount = 1;

async function loadSettings(): Promise<void> {
  settings = await invoke<Settings>("get_settings");
  getTextArea("peers").value = settings.sync.peers.join("\n");
  getInput("encryption-enabled").checked = settings.security.encryption_enabled;
  getInput("pairing-code").value = settings.security.pairing_code;
  getInput("shared-code").value = settings.sync.shared_code;
  const mb = Math.max(1, Math.round(settings.limits.max_item_bytes / (1024 * 1024)));
  getInput("max-item-mb").value = String(mb);
}

function collectSettings(): Settings {
  const peers = getTextArea("peers")
    .value.split("\n")
    .map((line) => line.trim())
    .filter(Boolean);

  const mb = Number(getInput("max-item-mb").value);
  const max_item_bytes = Math.max(1, Math.min(100, mb)) * 1024 * 1024;

  return {
    ...settings,
    limits: {
      max_item_bytes,
    },
    sync: {
      ...settings.sync,
      shared_code: getInput("shared-code").value.trim(),
      enabled: true,
      peers,
    },
    security: {
      ...settings.security,
      encryption_enabled: getInput("encryption-enabled").checked,
      pairing_code: getInput("pairing-code").value.trim(),
    },
  };
}

function markConfigDirty(): void {
  getText("config-feedback").textContent = "有未保存修改，点击“保存配置”后才会生效。";
}

async function saveSettings(): Promise<void> {
  if (!validateSharedCode()) {
    return;
  }
  const button = document.querySelector("#save-settings") as HTMLButtonElement | null;
  const feedback = getText("config-feedback");
  if (button) {
    button.disabled = true;
    button.textContent = "保存中...";
  }
  feedback.textContent = "正在保存配置并应用全局设置...";
  settings = collectSettings();
  try {
    await invoke("set_settings", { next: settings });
    await invoke("start_sync");
    observedMemberCount = 1;
    await refreshDomain();
    await refreshLogs();
    feedback.textContent = "配置已保存，并已按新配置重新应用。";
  } catch (error) {
    feedback.textContent = `保存失败：${String(error)}`;
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = "保存配置";
    }
  }
}

async function refreshStatus(): Promise<void> {
  const status = await invoke<RuntimeStatus>("sync_status");
  getText("status-running").textContent = `状态: ${status.running ? "运行中" : "已停止"}`;
  observedMemberCount = Math.max(observedMemberCount, status.peer_count, 1);
  getText("status-peer-count").textContent = `共享域成员（含本机）: ${observedMemberCount}`;
}

async function startSync(): Promise<void> {
  await invoke("start_sync");
  await refreshStatus();
}

function renderDevices(devices: DiscoveredDevice[]): void {
  const container = getText("discovered-devices");
  const feedback = getText("scan-feedback");
  lastDiscovered = devices;
  observedMemberCount = Math.max(1, devices.length + 1);
  getText("status-peer-count").textContent = `共享域成员（含本机）: ${observedMemberCount}`;
  if (!devices.length) {
    container.innerHTML = "";
    feedback.textContent = "当前没有发现其他共享域成员。";
    return;
  }
  const rows = devices
    .map(
      (device) =>
        `<div class="device-row"><span>${device.device_name} <em>${device.addr}</em></span><span class="device-tag">共享域内</span></div>`,
    )
    .join("");
  container.innerHTML = rows;
  feedback.textContent = `当前已发现 ${devices.length} 台共享域成员。`;
}

async function scanDevices(): Promise<void> {
  if (!validateSharedCode()) {
    return;
  }
  const button = document.querySelector("#refresh-domain") as HTMLButtonElement | null;
  const feedback = getText("scan-feedback");
  if (button) {
    button.disabled = true;
    button.textContent = "扫描中...";
  }
  feedback.textContent = "正在扫描局域网设备...";
  try {
    const devices = await invoke<DiscoveredDevice[]>("discover_devices");
    renderDevices(devices);
    feedback.textContent =
      devices.length > 0
        ? `扫描完成，发现 ${devices.length} 台共享域成员在线。`
        : "扫描完成，暂未发现其他共享域成员在线。";
  } catch (error) {
    feedback.textContent = `扫描失败：${String(error)}`;
    throw error;
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = "刷新";
    }
  }
}

async function refreshCachedDevices(): Promise<void> {
  const devices = await invoke<DiscoveredDevice[]>("cached_devices");
  renderDevices(devices);
}

async function refreshDomain(): Promise<void> {
  await scanDevices();
  await refreshStatus();
}

function validateSharedCode(): boolean {
  const code = getInput("shared-code").value.trim();
  if (!/^\d{6}$/.test(code)) {
    getText("scan-feedback").textContent = "共享码必须是 6 位数字。";
    getText("config-feedback").textContent = "共享码必须是 6 位数字，配置未保存。";
    return false;
  }
  return true;
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
  observedMemberCount = 1;
  await refreshStatus();
  await refreshLogs();
  renderDevices(lastDiscovered);
  await startSync();
  await refreshDomain();
  if (statusTimer !== null) {
    window.clearInterval(statusTimer);
  }
  statusTimer = window.setInterval(() => {
    void refreshStatus().then(refreshCachedDevices);
  }, 1500);
}

window.addEventListener("DOMContentLoaded", () => {
  document.querySelector("#save-settings")?.addEventListener("click", () => {
    void saveSettings();
  });
  document.querySelector("#refresh-domain")?.addEventListener("click", () => {
    void refreshDomain();
  });
  document.querySelector("#refresh-logs")?.addEventListener("click", refreshLogs);
  document.querySelector("#clear-logs")?.addEventListener("click", clearLogs);

  [
    "shared-code",
    "max-item-mb",
    "pairing-code",
  ].forEach((id) => {
    getInput(id).addEventListener("input", markConfigDirty);
  });
  getInput("encryption-enabled").addEventListener("change", markConfigDirty);
  getTextArea("peers").addEventListener("input", markConfigDirty);

  boot().catch((error) => {
    getText("scan-feedback").textContent = `启动错误: ${String(error)}`;
  });
});
