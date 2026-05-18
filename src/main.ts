import { invoke } from "@tauri-apps/api/core";

type Settings = {
  limits: {
    max_item_bytes: number;
  };
  sync: {
    device_id: string;
    shared_code: string;
    enabled: boolean;
    local_ip: string;
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
  device_name: string;
  local_ip?: string | null;
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

type NetworkInterfaceOption = {
  name: string;
  ip: string;
  label: string;
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
let currentStatus: RuntimeStatus | null = null;
let statusTimer: number | null = null;
let observedMemberCount = 1;
let networkOptions: NetworkInterfaceOption[] = [];
let membersExpanded = false;

async function loadSettings(): Promise<void> {
  settings = await invoke<Settings>("get_settings");
  networkOptions = await invoke<NetworkInterfaceOption[]>("list_network_interfaces");
  getTextArea("peers").value = settings.sync.peers.join("\n");
  getInput("encryption-enabled").checked = settings.security.encryption_enabled;
  getInput("pairing-code").value = settings.security.pairing_code;
  getInput("shared-code").value = settings.sync.shared_code;
  const mb = Math.max(1, Math.round(settings.limits.max_item_bytes / (1024 * 1024)));
  getInput("max-item-mb").value = String(mb);
  renderNetworkOptions(settings.sync.local_ip);
}

function renderNetworkOptions(selectedIp: string): void {
  const select = document.querySelector("#network-ip") as HTMLSelectElement;
  const normalized = selectedIp.trim();
  const activeIp = currentStatus?.local_ip?.trim() ?? "";
  const recommendedIp = selectRecommendedNetworkIp();
  const effectiveSelected = normalized || recommendedIp || "";
  const orderedOptions = [...networkOptions].sort((left, right) =>
    compareNetworkOptions(left, right, effectiveSelected, activeIp, recommendedIp),
  );
  const options = [
    `<option value="" ${effectiveSelected ? "" : "selected"}>自动选择最合适的局域网网络</option>`,
    ...orderedOptions.map((option) => {
      const suffix = [
        option.ip === recommendedIp ? "推荐" : "",
        option.ip === activeIp ? "当前使用" : "",
      ]
        .filter(Boolean)
        .join(" / ");
      const label = suffix ? `${option.label} · ${suffix}` : option.label;
      return `<option value="${escapeHtml(option.ip)}" ${
        effectiveSelected === option.ip ? "selected" : ""
      }>${escapeHtml(label)}</option>`;
    }),
    
  ];
  select.innerHTML = options.join("");
}

function compareNetworkOptions(
  left: NetworkInterfaceOption,
  right: NetworkInterfaceOption,
  selectedIp: string,
  activeIp: string,
  recommendedIp: string,
): number {
  const score = (option: NetworkInterfaceOption): number => {
    if (option.ip === selectedIp) return 400;
    if (option.ip === activeIp) return 300;
    if (option.ip === recommendedIp) return 200;
    if (isPrivateIpv4(option.ip)) return 100;
    return 0;
  };
  return (
    score(right) - score(left) ||
    left.name.localeCompare(right.name) ||
    left.ip.localeCompare(right.ip)
  );
}

function selectRecommendedNetworkIp(): string {
  const peerSubnets = new Set(
    lastDiscovered
      .map((device) => subnetKey(device.addr))
      .filter((value): value is string => Boolean(value)),
  );
  if (peerSubnets.size > 0) {
    const matching = networkOptions.find((option) => peerSubnets.has(subnetKey(option.ip)));
    if (matching) {
      return matching.ip;
    }
  }
  return currentStatus?.local_ip?.trim() || "";
}

function subnetKey(ip: string): string {
  const parts = ip.trim().split(".");
  if (parts.length !== 4) {
    return "";
  }
  return parts.slice(0, 3).join(".");
}

function isPrivateIpv4(ip: string): boolean {
  const parts = ip.trim().split(".").map(Number);
  if (parts.length !== 4 || parts.some((part) => Number.isNaN(part))) {
    return false;
  }
  const [a, b] = parts;
  return a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168);
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
      local_ip: (document.querySelector("#network-ip") as HTMLSelectElement).value.trim(),
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
  currentStatus = status;
  renderNetworkOptions(settings.sync.local_ip);
  getText("status-running").textContent = `状态: ${status.running ? "运行中" : "已停止"}`;
  observedMemberCount = Math.max(observedMemberCount, status.peer_count, 1);
  getText("status-peer-count").textContent = `共享域成员（含本机）: ${observedMemberCount}`;
}

async function startSync(): Promise<void> {
  await invoke("start_sync");
  await refreshStatus();
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (character) => {
    const entities: Record<string, string> = {
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    };
    return entities[character];
  });
}

function renderDeviceRow(
  title: string,
  subtitle: string,
  tag: string,
  className = "",
): string {
  const rowClass = className ? `device-row ${className}` : "device-row";
  return `<div class="${rowClass}"><span>${escapeHtml(title)} <em>${escapeHtml(subtitle)}</em></span><span class="device-tag">${escapeHtml(tag)}</span></div>`;
}

function renderDevices(devices: DiscoveredDevice[]): void {
  const container = getText("discovered-devices");
  const feedback = getText("scan-feedback");
  lastDiscovered = devices;
  observedMemberCount = Math.max(1, devices.length + 1);
  getText("status-peer-count").textContent = `共享域成员（含本机）: ${observedMemberCount}`;
  const selfName = currentStatus?.device_name || "本机设备";
  const selfIp = currentStatus?.local_ip?.trim();
  const selfMeta = selfIp ? `本机 · ${selfIp}` : "本机";
  const remoteRows = devices.length
    ? devices
        .map((device) => renderDeviceRow(device.device_name, device.addr, "共享域内"))
        .join("")
    : `<p class="empty-members">当前没有发现其他共享域成员。</p>`;
  container.innerHTML = `
    <details class="device-list-tree" ${membersExpanded ? "open" : ""}>
      <summary class="device-list-summary">
        ${renderDeviceRow(selfName, selfMeta, "本机", "device-row-self")}
        <span class="device-list-toggle" aria-hidden="true">${membersExpanded ? "收起" : "查看全部"}</span>
      </summary>
      <div class="device-list-children">${remoteRows}</div>
    </details>
  `;
  const details = container.querySelector("details");
  if (details) {
    details.addEventListener("toggle", () => {
      membersExpanded = details.open;
      const toggle = details.querySelector(".device-list-toggle");
      if (toggle) {
        toggle.textContent = details.open ? "收起" : "查看全部";
      }
    });
  }
  renderNetworkOptions(settings.sync.local_ip);
  feedback.textContent = devices.length
    ? `当前共享域共有 ${devices.length + 1} 台设备在线，展开可查看全部设备。`
    : "当前共享域只有本机在线。";
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
        ? `扫描完成，发现 ${devices.length} 台其他共享域成员在线。`
        : "扫描完成，当前只有本机在线。";
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
  (document.querySelector("#network-ip") as HTMLSelectElement).addEventListener(
    "change",
    markConfigDirty,
  );
  getInput("encryption-enabled").addEventListener("change", markConfigDirty);
  getTextArea("peers").addEventListener("input", markConfigDirty);

  boot().catch((error) => {
    getText("scan-feedback").textContent = `启动错误: ${String(error)}`;
  });
});
