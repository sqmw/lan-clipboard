import { invoke } from "@tauri-apps/api/core";
import {
  disable as disableAutostart,
  enable as enableAutostart,
  isEnabled as isAutostartEnabled,
} from "@tauri-apps/plugin-autostart";
import {
  mergeDiscoveredDevices,
  renderDevices as renderDeviceList,
  resetDeviceRenderCache,
  selectRecommendedNetworkIp,
} from "./deviceList";
import { setLocale, t } from "./i18n";
import {
  applyI18nStatic,
  collectSettings,
  getInput,
  getText,
  markConfigDirty,
  populateSettingsForm,
  renderLanguageOptions,
  renderNetworkOptions as renderNetworkSelect,
  validateSharedCode,
} from "./settingsForm";
import {
  isTransferPreviewBusy,
  renderTransferProgress,
  type TransferProgress,
} from "./transferProgress";
import type {
  DiscoveredDevice,
  NetworkInterfaceOption,
  RuntimeLog,
  RuntimeStatus,
  Settings,
} from "./types";

let settings: Settings;
let currentStatus: RuntimeStatus | null = null;
let statusTimer: number | null = null;
let transferTimer: number | null = null;
let networkOptions: NetworkInterfaceOption[] = [];
let networkRefreshTimer: number | null = null;
let draftSelectedNetworkIp: string | null = null;
let draftLanguage: string | null = null;
let statusRefreshRunning = false;
let transferRefreshRunning = false;
let manualRefreshRunning = false;
let deviceRefreshGeneration = 0;
let lastStatusKey = "";
let lastTransfersKey = "";

function getSelectedNetworkIp(): string {
  if (draftSelectedNetworkIp !== null) {
    return draftSelectedNetworkIp;
  }
  return (document.querySelector("#network-ip") as HTMLSelectElement | null)?.value.trim() ?? "";
}

async function loadSettings(): Promise<void> {
  settings = await invoke<Settings>("get_settings");
  networkOptions = await invoke<NetworkInterfaceOption[]>("list_network_interfaces");
  const formState = populateSettingsForm(settings);
  draftSelectedNetworkIp = formState.selectedNetworkIp;
  draftLanguage = formState.language;
  setLocale(draftLanguage);
  renderLanguageOptions(draftLanguage);
  applyI18nStatic();
  renderNetworkOptions();
}

function renderNetworkOptions(): void {
  const activeIp = currentStatus?.local_ip?.trim() ?? "";
  const recommendedIp = selectRecommendedNetworkIp(networkOptions, currentStatus);
  renderNetworkSelect(getSelectedNetworkIp(), networkOptions, activeIp, recommendedIp);
}

function renderDevices(): void {
  renderDeviceList({
    container: getText("discovered-devices"),
    feedback: getText("scan-feedback"),
    memberCount: getText("status-peer-count"),
    status: currentStatus,
    settings,
    refreshNetworkOptions: renderNetworkOptions,
  });
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
  settings = collectSettings(settings);

  try {
    await invoke("set_settings", { next: settings });
    await syncLaunchAtLogin(settings.ui.launch_at_login);
    await invoke("start_sync");
    draftSelectedNetworkIp = settings.sync.local_ip;
    draftLanguage = settings.ui.language;
    setLocale(draftLanguage);
    renderLanguageOptions(draftLanguage);
    applyI18nStatic();
    resetRefreshRenderCache();
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

async function syncLaunchAtLogin(enabled: boolean): Promise<void> {
  const active = await isAutostartEnabled();
  if (enabled === active) {
    return;
  }
  if (enabled) {
    await enableAutostart();
  } else {
    await disableAutostart();
  }
}

async function refreshStatus(): Promise<void> {
  if (statusRefreshRunning) {
    return;
  }
  statusRefreshRunning = true;
  const refreshGeneration = deviceRefreshGeneration;
  try {
    const status = await invoke<RuntimeStatus>("sync_status", {
      selectedLocalIp: getSelectedNetworkIp() || null,
    });
    if (refreshGeneration !== deviceRefreshGeneration) {
      return;
    }
    applyRuntimeStatus(status);
  } finally {
    statusRefreshRunning = false;
  }
}

function applyRuntimeStatus(status: RuntimeStatus): void {
  const nextStatusKey = JSON.stringify(status);
  const previousLocalIp = currentStatus?.local_ip?.trim() ?? "";
  const nextLocalIp = status.local_ip?.trim() ?? "";
  currentStatus = status;

  if (lastStatusKey !== nextStatusKey) {
    getText("status-running").textContent = `${t("app.status.label")}: ${
      status.running ? t("app.status.running") : t("app.status.stopped")
    }`;
    lastStatusKey = nextStatusKey;
  }
  if (previousLocalIp !== nextLocalIp) {
    renderNetworkOptions();
    renderDevices();
  }
}

async function scanDevices(): Promise<void> {
  if (manualRefreshRunning || !validateSharedCode()) {
    return;
  }

  manualRefreshRunning = true;
  const refreshGeneration = ++deviceRefreshGeneration;
  const button = document.querySelector("#refresh-domain") as HTMLButtonElement | null;
  const feedback = getText("scan-feedback");
  if (button) {
    button.disabled = true;
    button.textContent = t("app.domain.scanning");
  }
  feedback.textContent = t("app.scan.scanning");

  try {
    const devices = await invoke<DiscoveredDevice[]>("discover_devices", {
      selectedLocalIp: getSelectedNetworkIp() || null,
    });
    if (refreshGeneration !== deviceRefreshGeneration) {
      return;
    }
    mergeDiscoveredDevices(devices);
    renderDevices();
    feedback.textContent =
      devices.length > 0
        ? t("app.scan.done_found", { count: devices.length })
        : t("app.scan.done_none");
  } catch (error) {
    feedback.textContent = t("app.scan.failed", { error: String(error) });
    throw error;
  } finally {
    manualRefreshRunning = false;
    if (button) {
      button.disabled = false;
      button.textContent = t("app.domain.refresh");
    }
  }
}

async function refreshCachedDevices(): Promise<void> {
  if (manualRefreshRunning) {
    return;
  }
  const refreshGeneration = deviceRefreshGeneration;
  const devices = await invoke<DiscoveredDevice[]>("cached_devices", {
    selectedLocalIp: getSelectedNetworkIp() || null,
  });
  if (refreshGeneration !== deviceRefreshGeneration || manualRefreshRunning) {
    return;
  }
  mergeDiscoveredDevices(devices);
  renderDevices();
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

async function refreshLogs(): Promise<void> {
  const logs = await invoke<RuntimeLog[]>("get_runtime_logs", { limit: 300 });
  const lines = logs.map((log) => {
    const stamp = new Date(log.ts_ms).toLocaleTimeString();
    return `[${stamp}] [${log.level}] ${log.message}`;
  });
  getText("runtime-logs").textContent = lines.join("\n");
}

async function refreshTransferProgress(): Promise<void> {
  if (isTransferPreviewBusy() || transferRefreshRunning) {
    return;
  }
  transferRefreshRunning = true;
  try {
    const transfers = await invoke<TransferProgress[]>("get_transfer_progress");
    const nextTransfersKey = JSON.stringify(
      transfers.map((transfer) => [
        transfer.id,
        transfer.status,
        transfer.percent,
        transfer.transferred_bytes,
        transfer.total_bytes,
        transfer.item_summary,
        transfer.error ?? "",
      ]),
    );
    if (lastTransfersKey === nextTransfersKey) {
      return;
    }
    lastTransfersKey = nextTransfersKey;
    renderTransferProgress(getText("transfer-progress-list"), transfers, () => {
      void refreshTransferProgress();
    });
  } finally {
    transferRefreshRunning = false;
  }
}

async function clearLogs(): Promise<void> {
  await invoke("clear_runtime_logs");
  await refreshLogs();
  await refreshStatus();
}

async function boot(): Promise<void> {
  await loadSettings();
  try {
    await syncLaunchAtLogin(Boolean(settings.ui?.launch_at_login));
  } catch (error) {
    getText("config-feedback").textContent = t("app.settings.launch_at_login_failed", {
      error: String(error),
    });
  }
  await refreshStatus();
  await refreshTransferProgress();
  await refreshLogs();
  renderDevices();
  await refreshDomain();
  startTimers();
}

function startTimers(): void {
  if (statusTimer !== null) {
    window.clearInterval(statusTimer);
  }
  if (transferTimer !== null) {
    window.clearInterval(transferTimer);
  }
  statusTimer = window.setInterval(() => {
    void refreshStatus();
    void refreshCachedDevices();
  }, 1800);
  transferTimer = window.setInterval(() => {
    void refreshTransferProgress();
  }, 500);
}

function resetRefreshRenderCache(): void {
  lastStatusKey = "";
  lastTransfersKey = "";
  resetDeviceRenderCache();
}

function bindSettingsInputs(): void {
  ["shared-code", "max-item-mb"].forEach((id) => {
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
    resetRefreshRenderCache();
    markConfigDirty();
    renderDevices();
    void refreshTransferProgress();
    void refreshStatus();
  });
  getInput("encryption-enabled").addEventListener("change", markConfigDirty);
  getInput("launch-at-login").addEventListener("change", markConfigDirty);
}

function bindActionButtons(): void {
  document.querySelector("#save-settings")?.addEventListener("click", () => {
    void saveSettings();
  });
  document.querySelector("#refresh-domain")?.addEventListener("click", () => {
    void refreshDomain();
  });
  document.querySelector("#refresh-logs")?.addEventListener("click", refreshLogs);
  document.querySelector("#clear-logs")?.addEventListener("click", clearLogs);
}

window.addEventListener("DOMContentLoaded", () => {
  bindActionButtons();
  bindSettingsInputs();
  boot().catch((error) => {
    getText("scan-feedback").textContent = t("app.boot.failed", { error: String(error) });
  });
});
