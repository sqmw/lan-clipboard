import { invoke, isTauri } from "@tauri-apps/api/core";
import { setLocale, t } from "./i18n";

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
    poll_interval_ms: number;
  };
  security: {
    encryption_enabled: boolean;
  };
  ui: {
    language: string;
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

type TransferProgress = {
  id: string;
  direction: string;
  peer: string;
  item_kind: string;
  item_label: string;
  item_summary: string;
  item_id: string;
  transferred_bytes: number;
  total_bytes: number;
  percent: number;
  status: string;
  updated_at_ms: number;
  error?: string | null;
};

const getInput = (id: string): HTMLInputElement =>
  document.querySelector(`#${id}`) as HTMLInputElement;
const getText = (id: string): HTMLElement =>
  document.querySelector(`#${id}`) as HTMLElement;
const MIN_MAX_ITEM_MB = 1;
const MAX_MAX_ITEM_MB = 1000;

let settings: Settings;
let lastDiscovered: DiscoveredDevice[] = [];
let currentStatus: RuntimeStatus | null = null;
let statusTimer: number | null = null;
let transferTimer: number | null = null;
let observedMemberCount = 1;
let networkOptions: NetworkInterfaceOption[] = [];
let membersExpanded = false;
const expandedTransferIds = new Set<string>();
const transferPreviewScrollTops = new Map<string, number>();
let isTransferPreviewInteracting = false;
let transferPreviewIdleTimer: number | null = null;
let networkRefreshTimer: number | null = null;
let draftSelectedNetworkIp: string | null = null;
let draftLanguage: string | null = null;

async function tauriInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauri()) {
    return invoke<T>(command, args);
  }
  return mockInvoke<T>(command, args);
}

function mockInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const selectedLocalIp =
    typeof args?.selectedLocalIp === "string" ? (args.selectedLocalIp as string) : "192.168.0.107";

  switch (command) {
    case "get_settings":
      return Promise.resolve({
        limits: { max_item_bytes: 100 * 1024 * 1024 },
        sync: {
          device_id: "mock-device",
          shared_code: "666666",
          enabled: true,
          local_ip: selectedLocalIp,
          listen_port: 32910,
          poll_interval_ms: 900,
        },
        security: { encryption_enabled: true },
        ui: { language: "zh-CN" },
      } as T);
    case "list_network_interfaces":
      return Promise.resolve(
        [
          { name: "en0", ip: "192.168.0.107", label: "en0 (192.168.0.107)" },
          { name: "Wi-Fi", ip: "192.168.0.101", label: "Wi-Fi (192.168.0.101)" },
        ] as T,
      );
    case "sync_status":
      return Promise.resolve({
        running: true,
        device_id: "mock-device",
        device_name: "This Mac (Preview)",
        local_ip: selectedLocalIp,
        shared_code: "666666",
        last_error: null,
        recent_log_count: 0,
        peer_count: 2,
      } as T);
    case "cached_devices":
    case "discover_devices":
      return Promise.resolve(
        [
          {
            device_id: "mock-peer-1",
            device_name: "DESKTOP-OIUVLC2",
            addr: "192.168.0.105",
            port: 32910,
          },
        ] as T,
      );
    case "get_transfer_progress":
      return Promise.resolve([] as unknown as T);
    case "get_runtime_logs":
      return Promise.resolve([] as unknown as T);
    case "set_settings":
    case "start_sync":
    case "clear_runtime_logs":
      return Promise.resolve(undefined as T);
    default:
      return Promise.reject(new Error(`Not available outside Tauri: ${command}`));
  }
}

function getSelectedNetworkIp(): string {
  if (draftSelectedNetworkIp !== null) {
    return draftSelectedNetworkIp;
  }
  return (document.querySelector("#network-ip") as HTMLSelectElement | null)?.value.trim() ?? "";
}

async function loadSettings(): Promise<void> {
  settings = await tauriInvoke<Settings>("get_settings");
  networkOptions = await tauriInvoke<NetworkInterfaceOption[]>("list_network_interfaces");
  getInput("encryption-enabled").checked = settings.security.encryption_enabled;
  getInput("shared-code").value = settings.sync.shared_code;
  const mb = Math.max(1, Math.round(settings.limits.max_item_bytes / (1024 * 1024)));
  getInput("max-item-mb").value = String(mb);
  draftSelectedNetworkIp = settings.sync.local_ip;
  draftLanguage = (settings.ui?.language || "auto").trim() || "auto";
  setLocale(draftLanguage);
  renderLanguageOptions(draftLanguage);
  applyI18nStatic();
  renderNetworkOptions(draftSelectedNetworkIp);
}

function renderLanguageOptions(selected: string): void {
  const select = document.querySelector("#language") as HTMLSelectElement;
  const value = (selected || "auto").trim();
  const options = [
    `<option value="auto" ${value === "auto" ? "selected" : ""}>${escapeHtml(
      t("app.settings.language.auto"),
    )}</option>`,
    `<option value="zh-CN" ${value === "zh-CN" ? "selected" : ""}>${escapeHtml(
      t("app.settings.language.zh"),
    )}</option>`,
    `<option value="en-US" ${value === "en-US" ? "selected" : ""}>${escapeHtml(
      t("app.settings.language.en"),
    )}</option>`,
  ];
  select.innerHTML = options.join("");
}

function applyI18nStatic(): void {
  const setText = (id: string, key: string) => {
    const el = document.querySelector(`#${id}`) as HTMLElement | null;
    if (el) el.textContent = t(key);
  };

  setText("i18n-domain-kicker", "app.domain.kicker");
  setText("i18n-domain-title", "app.domain.title");
  setText("refresh-domain", "app.domain.refresh");
  setText("i18n-devices-label", "app.domain.devices");
  setText("i18n-status-kicker", "app.status.kicker");
  setText("i18n-status-title", "app.status.title");
  setText("i18n-status-label", "app.status.label");
  setText("i18n-members-label", "app.status.members");
  setText("i18n-transfer-label", "app.transfer.title");
  setText("i18n-settings-kicker", "app.settings.kicker");
  setText("i18n-settings-title", "app.settings.title");
  setText("save-settings", "app.settings.save");
  setText("i18n-shared-code-label", "app.settings.shared_code");
  setText("i18n-network-label", "app.settings.network");
  setText("i18n-max-mb-label", "app.settings.max_mb");
  setText("i18n-language-label", "app.settings.language");
  setText("i18n-settings-hint", "app.settings.hint");
  setText("config-feedback", "app.settings.initial_feedback");
  setText("i18n-advanced-summary", "app.advanced.summary");
  setText("i18n-security-title", "app.security.title");
  setText("i18n-encrypt-label", "app.security.encrypt");
  setText("i18n-logs-title", "app.logs.title");
  setText("refresh-logs", "app.logs.refresh");
  setText("clear-logs", "app.logs.clear");

  getInput("shared-code").placeholder = t("app.settings.shared_code.placeholder");
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
    `<option value="" ${effectiveSelected ? "" : "selected"}>${escapeHtml(
      t("app.settings.network.auto"),
    )}</option>`,
    ...orderedOptions.map((option) => {
      const suffix = [
        option.ip === recommendedIp ? t("app.settings.network.recommended") : "",
        option.ip === activeIp ? t("app.settings.network.active") : "",
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
  const mb = Number(getInput("max-item-mb").value);
  const normalizedMb = Math.max(
    MIN_MAX_ITEM_MB,
    Math.min(MAX_MAX_ITEM_MB, Number.isFinite(mb) ? Math.round(mb) : MIN_MAX_ITEM_MB),
  );
  getInput("max-item-mb").value = String(normalizedMb);
  const max_item_bytes = normalizedMb * 1024 * 1024;

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
    },
    security: {
      ...settings.security,
      encryption_enabled: getInput("encryption-enabled").checked,
    },
    ui: {
      ...settings.ui,
      language: (document.querySelector("#language") as HTMLSelectElement).value.trim() || "auto",
    },
  };
}

function markConfigDirty(): void {
  getText("config-feedback").textContent = t("app.settings.dirty");
}

async function saveSettings(): Promise<void> {
  if (!validateSharedCode()) {
    return;
  }
  const button = document.querySelector("#save-settings") as HTMLButtonElement | null;
  const feedback = getText("config-feedback");
  if (button) {
    button.disabled = true;
    button.textContent = t("app.settings.saving");
  }
  feedback.textContent = t("app.settings.saving_feedback");
  settings = collectSettings();
  try {
    await tauriInvoke("set_settings", { next: settings });
    await tauriInvoke("start_sync");
    draftSelectedNetworkIp = settings.sync.local_ip;
    draftLanguage = settings.ui.language;
    setLocale(draftLanguage);
    renderLanguageOptions(draftLanguage);
    applyI18nStatic();
    observedMemberCount = 1;
    await refreshDomain();
    await refreshLogs();
    feedback.textContent = t("app.settings.saved_feedback");
  } catch (error) {
    feedback.textContent = t("app.settings.save_failed", { error: String(error) });
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = t("app.settings.save");
    }
  }
}

async function refreshStatus(): Promise<void> {
  const status = await tauriInvoke<RuntimeStatus>("sync_status", {
    selectedLocalIp: getSelectedNetworkIp() || null,
  });
  currentStatus = status;
  renderNetworkOptions(getSelectedNetworkIp());
  getText("status-running").textContent = `${t("app.status.label")}: ${
    status.running ? t("app.status.running") : t("app.status.stopped")
  }`;
  observedMemberCount = Math.max(observedMemberCount, status.peer_count, 1);
  getText("status-peer-count").textContent = `${t("app.status.members")}: ${observedMemberCount}`;
}

async function startSync(): Promise<void> {
  await tauriInvoke("start_sync");
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
  getText("status-peer-count").textContent = `${t("app.status.members")}: ${observedMemberCount}`;
  const selfName = currentStatus?.device_name || t("app.self.device");
  const selfIp = currentStatus?.local_ip?.trim();
  const selfMeta = selfIp ? `${t("app.self")} · ${selfIp}` : t("app.self");
  const remoteRows = devices.length
    ? devices
        .map((device) =>
          renderDeviceRow(device.device_name, device.addr, t("app.domain.member_tag")),
        )
        .join("")
    : `<p class="empty-members">${escapeHtml(t("app.domain.empty"))}</p>`;
  container.innerHTML = `
    <details class="device-list-tree" ${membersExpanded ? "open" : ""}>
      <summary class="device-list-summary">
        ${renderDeviceRow(selfName, selfMeta, t("app.domain.self_tag"), "device-row-self")}
        <span class="device-list-toggle" aria-hidden="true">${escapeHtml(
          membersExpanded ? t("app.domain.collapse") : t("app.domain.view_all"),
        )}</span>
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
        toggle.textContent = details.open ? t("app.domain.collapse") : t("app.domain.view_all");
      }
    });
  }
  renderNetworkOptions(getSelectedNetworkIp());
  feedback.textContent = devices.length
    ? t("app.domain.online_total", { count: devices.length + 1 })
    : t("app.domain.only_self");
}

async function scanDevices(): Promise<void> {
  if (!validateSharedCode()) {
    return;
  }
  const button = document.querySelector("#refresh-domain") as HTMLButtonElement | null;
  const feedback = getText("scan-feedback");
  if (button) {
    button.disabled = true;
    button.textContent = t("app.domain.scanning");
  }
  feedback.textContent = t("app.scan.scanning");
  try {
    const devices = await tauriInvoke<DiscoveredDevice[]>("discover_devices", {
      selectedLocalIp: getSelectedNetworkIp() || null,
    });
    renderDevices(devices);
    feedback.textContent =
      devices.length > 0
        ? t("app.scan.done_found", { count: devices.length })
        : t("app.scan.done_none");
  } catch (error) {
    feedback.textContent = t("app.scan.failed", { error: String(error) });
    throw error;
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = t("app.domain.refresh");
    }
  }
}

async function refreshCachedDevices(): Promise<void> {
  const devices = await tauriInvoke<DiscoveredDevice[]>("cached_devices", {
    selectedLocalIp: getSelectedNetworkIp() || null,
  });
  renderDevices(devices);
}

async function refreshDomain(): Promise<void> {
  await scanDevices();
  await refreshStatus();
}

function scheduleNetworkRefresh(): void {
  if (networkRefreshTimer !== null) {
    window.clearTimeout(networkRefreshTimer);
  }
  getText("scan-feedback").textContent = t("app.network.switched");
  networkRefreshTimer = window.setTimeout(() => {
    networkRefreshTimer = null;
    void refreshDomain().catch((error) => {
      getText("scan-feedback").textContent = t("app.refresh.failed", { error: String(error) });
    });
  }, 180);
}

function validateSharedCode(): boolean {
  const code = getInput("shared-code").value.trim();
  if (!/^\d{6}$/.test(code)) {
    getText("scan-feedback").textContent = t("app.settings.code_invalid");
    getText("config-feedback").textContent = t("app.settings.code_invalid_save");
    return false;
  }
  return true;
}

async function refreshLogs(): Promise<void> {
  const logs = await tauriInvoke<RuntimeLog[]>("get_runtime_logs", { limit: 300 });
  const lines = logs.map((log) => {
    const stamp = new Date(log.ts_ms).toLocaleTimeString();
    return `[${stamp}] [${log.level}] ${log.message}`;
  });
  getText("runtime-logs").textContent = lines.join("\n");
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function transferStatusText(status: string): string {
  switch (status) {
    case "sending":
      return t("transfer.status.sending");
    case "receiving":
      return t("transfer.status.receiving");
    case "queued":
      return t("transfer.status.pending_apply");
    case "applying":
      return t("transfer.status.applying");
    case "retrying":
      return t("transfer.status.retrying");
    case "completed":
      return t("transfer.status.completed");
    case "failed":
      return t("transfer.status.failed");
    case "received":
      return t("transfer.status.received");
    default:
      return status;
  }
}

function transferDirectionText(direction: string): string {
  return direction === "send" ? t("transfer.send") : t("transfer.recv");
}

function displayItemLabel(transfer: TransferProgress): string {
  switch (transfer.item_kind) {
    case "text":
      return t("transfer.label.text");
    case "html":
      return t("transfer.label.html");
    case "rtf":
      return t("transfer.label.rtf");
    case "image_png":
      return t("transfer.label.image");
    case "file_bundle":
      return transfer.item_label === "文本文件"
        ? t("transfer.label.text_file")
        : t("transfer.label.file");
    default:
      return transfer.item_label || t("transfer.label.unknown");
  }
}

function isTextPreviewTransfer(transfer: TransferProgress): boolean {
  return transfer.item_kind === "text" || transfer.item_kind === "html" || transfer.item_kind === "rtf";
}

function textPreviewLabel(transfer: TransferProgress): string {
  switch (transfer.item_kind) {
    case "html":
      return t("transfer.preview.html");
    case "rtf":
      return t("transfer.preview.rtf");
    default:
      return t("transfer.preview.text");
  }
}

function renderTransferSummary(transfer: TransferProgress): string {
  const summary = transfer.item_summary || `${displayItemLabel(transfer)}`;
  if (isTextPreviewTransfer(transfer)) {
    const expanded = expandedTransferIds.has(transfer.id);
    const shouldToggle = summary.length > 180 || summary.includes("\n");
    return `<div class="transfer-preview ${expanded ? "is-expanded" : ""}">
      <div class="transfer-preview-head">
        <span class="transfer-preview-label">${escapeHtml(textPreviewLabel(transfer))}</span>
        ${
          shouldToggle
            ? `<button class="transfer-preview-toggle" type="button" data-transfer-id="${escapeHtml(
                transfer.id,
              )}">${expanded ? escapeHtml(t("app.transfer.collapse")) : escapeHtml(t("app.transfer.expand"))}</button>`
            : ""
        }
      </div>
      <div class="transfer-preview-content" data-transfer-id="${escapeHtml(transfer.id)}" tabindex="0">${escapeHtml(
      summary,
    )}</div></div>`;
  }
  return `<p class="transfer-title">${escapeHtml(summary)}</p>`;
}

function holdTransferPreviewRefresh(): void {
  isTransferPreviewInteracting = true;
  if (transferPreviewIdleTimer !== null) {
    window.clearTimeout(transferPreviewIdleTimer);
  }
}

function releaseTransferPreviewRefresh(delayMs = 500): void {
  if (transferPreviewIdleTimer !== null) {
    window.clearTimeout(transferPreviewIdleTimer);
  }
  transferPreviewIdleTimer = window.setTimeout(() => {
    isTransferPreviewInteracting = false;
    transferPreviewIdleTimer = null;
    void refreshTransferProgress();
  }, delayMs);
}

function renderTransferProgress(transfers: TransferProgress[]): void {
  const container = getText("transfer-progress-list");
  container.querySelectorAll<HTMLElement>(".transfer-preview-content").forEach((element) => {
    const id = element.dataset.transferId;
    if (id) {
      transferPreviewScrollTops.set(id, element.scrollTop);
    }
  });
  if (!transfers.length) {
    container.innerHTML = `<p class="empty-members">${escapeHtml(t("app.transfer.empty"))}</p>`;
    transferPreviewScrollTops.clear();
    return;
  }
  const activeTransferIds = new Set(transfers.map((transfer) => transfer.id));
  expandedTransferIds.forEach((id) => {
    if (!activeTransferIds.has(id)) {
      expandedTransferIds.delete(id);
    }
  });
  transferPreviewScrollTops.forEach((_, id) => {
    if (!activeTransferIds.has(id)) {
      transferPreviewScrollTops.delete(id);
    }
  });

  container.innerHTML = transfers
    .map((transfer) => {
      const error = transfer.error ? `<p class="transfer-error">${escapeHtml(transfer.error)}</p>` : "";
      return `
        <article class="transfer-card">
          <div class="transfer-head">
            <strong>${escapeHtml(`${transferDirectionText(transfer.direction)} ${transfer.peer}`)}</strong>
            <span class="transfer-badge transfer-${escapeHtml(transfer.status)}">${escapeHtml(
              transferStatusText(transfer.status),
            )}</span>
          </div>
          ${renderTransferSummary(transfer)}
          <p class="transfer-meta">${escapeHtml(
            `${displayItemLabel(transfer)} · ${formatBytes(transfer.transferred_bytes)} / ${formatBytes(
              transfer.total_bytes,
            )}`,
          )}</p>
          <div class="transfer-bar"><span style="width: ${transfer.percent}%"></span></div>
          <p class="transfer-meta">${escapeHtml(`${transfer.percent}%`)}</p>
          ${error}
        </article>
      `;
    })
    .join("");
  container.querySelectorAll<HTMLButtonElement>(".transfer-preview-toggle").forEach((button) => {
    button.addEventListener("click", () => {
      const id = button.dataset.transferId;
      if (!id) return;
      if (expandedTransferIds.has(id)) {
        expandedTransferIds.delete(id);
      } else {
        expandedTransferIds.add(id);
      }
      renderTransferProgress(transfers);
    });
  });
  container.querySelectorAll<HTMLElement>(".transfer-preview-content").forEach((element) => {
    const id = element.dataset.transferId;
    if (!id) return;
    element.scrollTop = transferPreviewScrollTops.get(id) ?? 0;
    element.addEventListener("mouseenter", () => holdTransferPreviewRefresh());
    element.addEventListener("mouseleave", () => releaseTransferPreviewRefresh());
    element.addEventListener("focus", () => holdTransferPreviewRefresh());
    element.addEventListener("blur", () => releaseTransferPreviewRefresh(150));
    element.addEventListener("wheel", () => holdTransferPreviewRefresh(), { passive: true });
    element.addEventListener("scroll", () => {
      holdTransferPreviewRefresh();
      transferPreviewScrollTops.set(id, element.scrollTop);
      releaseTransferPreviewRefresh(700);
    });
  });
}

async function refreshTransferProgress(): Promise<void> {
  if (isTransferPreviewInteracting) {
    return;
  }
  const transfers = await tauriInvoke<TransferProgress[]>("get_transfer_progress");
  renderTransferProgress(transfers);
}

async function clearLogs(): Promise<void> {
  await tauriInvoke("clear_runtime_logs");
  await refreshLogs();
  await refreshStatus();
}

async function boot(): Promise<void> {
  await loadSettings();
  observedMemberCount = 1;
  await refreshStatus();
  await refreshTransferProgress();
  await refreshLogs();
  renderDevices(lastDiscovered);
  await startSync();
  await refreshDomain();
  if (statusTimer !== null) {
    window.clearInterval(statusTimer);
  }
  if (transferTimer !== null) {
    window.clearInterval(transferTimer);
  }
  statusTimer = window.setInterval(() => {
    void Promise.all([refreshStatus(), refreshCachedDevices()]);
  }, 1200);
  transferTimer = window.setInterval(() => {
    void refreshTransferProgress();
  }, 180);
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
  ].forEach((id) => {
    getInput(id).addEventListener("input", markConfigDirty);
  });
  (document.querySelector("#network-ip") as HTMLSelectElement).addEventListener(
    "change",
    (event) => {
      draftSelectedNetworkIp = (event.currentTarget as HTMLSelectElement).value.trim();
      markConfigDirty();
      scheduleNetworkRefresh();
    },
  );
  (document.querySelector("#language") as HTMLSelectElement).addEventListener("change", (event) => {
    draftLanguage = (event.currentTarget as HTMLSelectElement).value.trim() || "auto";
    setLocale(draftLanguage);
    renderLanguageOptions(draftLanguage);
    applyI18nStatic();
    markConfigDirty();
    renderDevices(lastDiscovered);
    void refreshTransferProgress();
    void refreshStatus();
  });
  getInput("encryption-enabled").addEventListener("change", markConfigDirty);

  boot().catch((error) => {
    getText("scan-feedback").textContent = t("app.boot.failed", { error: String(error) });
  });
});
