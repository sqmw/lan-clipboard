import { invoke } from "@tauri-apps/api/core";
import {
  disable as disableAutostart,
  enable as enableAutostart,
  isEnabled as isAutostartEnabled,
} from "@tauri-apps/plugin-autostart";
import {
  clearDiscoveredDevices,
  mergeDiscoveredDevices,
  renderDevices as renderDeviceList,
  resetDeviceRenderCache,
  selectRecommendedNetworkIp,
} from "./deviceList";
import { setLocale, t } from "./i18n";
import {
  applyI18nStatic,
  collectSettingsUpdate,
  getInput,
  getText,
  markConfigDirty,
  populateSettingsForm,
  renderLanguageOptions,
  renderNetworkOptions as renderNetworkSelect,
  validateMaxItemSize,
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
  SettingsNotice,
  SettingsUpdate,
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
let manualRefreshPromise: Promise<void> | null = null;
let manualRefreshGeneration: number | null = null;
let deviceRefreshGeneration = 0;
let deviceCacheAcceptedGeneration = -1;
let lastStatusKey = "";
let lastTransfersKey = "";
let settingsReady = false;
let settingsLoadPromise: Promise<void> | null = null;
let settingsRecoveryRunning = false;
let saveSettingsRunning = false;

function getSelectedNetworkIp(): string {
  if (draftSelectedNetworkIp !== null) {
    return draftSelectedNetworkIp;
  }
  return (document.querySelector("#network-ip") as HTMLSelectElement | null)?.value.trim() ?? "";
}

function applySettingsUpdate(current: Settings, update: SettingsUpdate): Settings {
  return {
    ...current,
    limits: { max_item_bytes: update.max_item_bytes },
    sync: {
      ...current.sync,
      shared_code: update.shared_code,
      enabled: true,
      local_ip: update.local_ip,
    },
    security: { encryption_enabled: true },
    ui: {
      language: update.language,
      launch_at_login: update.launch_at_login,
    },
  };
}

function discoverySettingsChanged(previous: Settings, next: Settings): boolean {
  return (
    previous.sync.device_id !== next.sync.device_id ||
    previous.sync.shared_code !== next.sync.shared_code ||
    previous.sync.enabled !== next.sync.enabled ||
    previous.sync.local_ip !== next.sync.local_ip ||
    previous.sync.listen_port !== next.sync.listen_port ||
    previous.sync.poll_interval_ms !== next.sync.poll_interval_ms ||
    previous.security.encryption_enabled !== next.security.encryption_enabled
  );
}

async function loadSettings(): Promise<void> {
  const [nextSettings, nextNetworkOptions] = await Promise.all([
    invoke<Settings>("get_settings"),
    invoke<NetworkInterfaceOption[]>("list_network_interfaces"),
  ]);
  networkOptions = nextNetworkOptions;
  applyLoadedSettings(nextSettings);
}

function applyLoadedSettings(nextSettings: Settings): void {
  settings = nextSettings;
  const formState = populateSettingsForm(settings);
  draftSelectedNetworkIp = formState.selectedNetworkIp;
  draftLanguage = formState.language;
  setLocale(draftLanguage);
  renderLanguageOptions(draftLanguage);
  applyI18nStatic();
  renderNetworkOptions();
}

async function ensureSettingsLoaded(): Promise<void> {
  if (settingsReady) {
    return;
  }
  if (settingsLoadPromise) {
    return settingsLoadPromise;
  }

  const request = loadSettings()
    .then(() => {
      settingsReady = true;
      setSettingsFormEnabled(true);
      renderDevices();
    })
    .finally(() => {
      if (settingsLoadPromise === request) {
        settingsLoadPromise = null;
      }
    });
  settingsLoadPromise = request;
  return request;
}

function renderNetworkOptions(): void {
  const activeIp = currentStatus?.local_ip?.trim() ?? "";
  const recommendedIp = selectRecommendedNetworkIp(networkOptions, currentStatus);
  renderNetworkSelect(getSelectedNetworkIp(), networkOptions, activeIp, recommendedIp);
}

function renderDevices(): void {
  if (!settingsReady) {
    return;
  }
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
  if (!settingsReady || saveSettingsRunning) {
    return;
  }
  if (!validateSharedCode() || !validateMaxItemSize()) {
    return;
  }

  saveSettingsRunning = true;
  const button = document.querySelector("#save-settings") as HTMLButtonElement | null;
  const feedback = getText("config-feedback");
  setSettingsFormEnabled(false);
  if (button) {
    button.textContent = t("app.settings.saving");
  }
  feedback.textContent = t("app.settings.saving_feedback");

  let persisted = false;
  try {
    const previousSettings = settings;
    const update = collectSettingsUpdate(previousSettings);
    await persistSettingsTransaction(update);
    persisted = true;
    let settingsRefreshError: unknown = null;
    let appliedSettings = applySettingsUpdate(previousSettings, update);
    try {
      appliedSettings = await invoke<Settings>("get_settings");
    } catch (error) {
      settingsRefreshError = error;
    }
    const discoveryChanged = discoverySettingsChanged(previousSettings, appliedSettings);
    applyLoadedSettings(appliedSettings);
    if (discoveryChanged) {
      invalidateDiscoveryState();
    }

    resetRefreshRenderCache();
    const refreshResults = await Promise.allSettled(
      discoveryChanged
        ? [refreshDomain(), refreshLogs()]
        : [refreshStatus(), refreshCachedDevices(), refreshLogs()],
    );
    const refreshFailure = firstRejected(refreshResults);
    if (settingsRefreshError) {
      feedback.textContent = t("app.settings.saved_with_refresh_warning", {
        error: String(settingsRefreshError),
      });
    } else if (refreshFailure) {
      feedback.textContent = t("app.settings.saved_with_refresh_warning", {
        error: String(refreshFailure.reason),
      });
    } else {
      feedback.textContent = t("app.settings.saved_feedback");
    }
  } catch (error) {
    if (persisted) {
      try {
        applyLoadedSettings(await invoke<Settings>("get_settings"));
      } catch {
        // The submitted values were persisted, but the backend could not be read back.
      }
      feedback.textContent = t("app.settings.saved_with_refresh_warning", {
        error: String(error),
      });
    } else {
      feedback.textContent = t("app.settings.save_failed", { error: String(error) });
    }
  } finally {
    saveSettingsRunning = false;
    setSettingsFormEnabled(settingsReady);
    if (button) {
      button.textContent = t("app.settings.save");
    }
  }
}

async function generatePairingKey(): Promise<void> {
  if (!settingsReady || saveSettingsRunning) {
    return;
  }
  if (!window.confirm(t("app.settings.generate_key_confirm"))) {
    return;
  }

  const button = document.querySelector("#generate-pairing-key") as HTMLButtonElement | null;
  if (button) {
    button.disabled = true;
    button.textContent = t("app.settings.generating_key");
  }
  try {
    const key = await invoke<string>("generate_pairing_key");
    const input = getInput("shared-code");
    input.value = key;
    input.setAttribute("aria-invalid", "false");
    getText("config-feedback").textContent = t("app.settings.generated_key_unsaved");
  } catch (error) {
    getText("config-feedback").textContent = t("app.settings.generate_key_failed", {
      error: String(error),
    });
  } finally {
    if (button) {
      button.disabled = !settingsReady || saveSettingsRunning;
      button.textContent = t("app.settings.generate_key");
    }
  }
}

async function syncLaunchAtLogin(enabled: boolean): Promise<void> {
  const active = await isAutostartEnabled();
  if (enabled === active) {
    return;
  }
  await setLaunchAtLogin(enabled);
}

async function setLaunchAtLogin(enabled: boolean): Promise<void> {
  if (enabled) {
    await enableAutostart();
  } else {
    await disableAutostart();
  }
}

async function persistSettingsTransaction(update: SettingsUpdate): Promise<void> {
  let previousLaunchAtLogin: boolean;
  try {
    previousLaunchAtLogin = await isAutostartEnabled();
  } catch (error) {
    throw new Error(t("app.settings.launch_state_read_failed", { error: String(error) }));
  }

  const launchStateChanged = previousLaunchAtLogin !== update.launch_at_login;
  if (launchStateChanged) {
    try {
      await setLaunchAtLogin(update.launch_at_login);
    } catch (applyError) {
      try {
        await setLaunchAtLogin(previousLaunchAtLogin);
      } catch (rollbackError) {
        throw new Error(
          t("app.settings.launch_apply_rollback_failed", {
            error: String(applyError),
            rollback_error: String(rollbackError),
          }),
        );
      }
      throw new Error(t("app.settings.launch_apply_failed", { error: String(applyError) }));
    }
  }

  try {
    await invoke("set_settings", { update });
  } catch (backendError) {
    let verificationError: unknown = null;
    try {
      const persistedSettings = await invoke<Settings>("get_settings");
      if (settingsMatchUpdate(persistedSettings, update)) {
        return;
      }
    } catch (error) {
      verificationError = error;
    }

    if (launchStateChanged) {
      try {
        await setLaunchAtLogin(previousLaunchAtLogin);
      } catch (rollbackError) {
        throw new Error(
          t("app.settings.backend_launch_rollback_failed", {
            error: String(backendError),
            verification_error: String(verificationError ?? "settings differ"),
            rollback_error: String(rollbackError),
          }),
        );
      }
    }
    if (verificationError) {
      throw new Error(
        t("app.settings.backend_commit_unknown", {
          error: String(backendError),
          verification_error: String(verificationError),
          launch_result: launchStateChanged
            ? t("app.settings.launch_restored")
            : t("app.settings.launch_unchanged"),
        }),
      );
    }
    if (launchStateChanged) {
      throw new Error(
        t("app.settings.backend_save_failed_launch_restored", {
          error: String(backendError),
        }),
      );
    }
    throw backendError;
  }
}

function settingsMatchUpdate(actual: Settings, update: SettingsUpdate): boolean {
  return (
    actual.limits.max_item_bytes === update.max_item_bytes &&
    actual.sync.shared_code === update.shared_code &&
    actual.sync.local_ip.trim() === update.local_ip.trim() &&
    (actual.ui.language.trim() || "auto") === (update.language.trim() || "auto") &&
    actual.ui.launch_at_login === update.launch_at_login
  );
}

function firstRejected<T>(
  results: PromiseSettledResult<T>[],
): PromiseRejectedResult | undefined {
  return results.find(
    (result): result is PromiseRejectedResult => result.status === "rejected",
  );
}

function setTextIfChanged(element: HTMLElement, value: string): void {
  if (element.textContent !== value) {
    element.textContent = value;
  }
}

function settingsNoticeMessage(notice: SettingsNotice | null): string {
  if (!notice) {
    return "";
  }
  switch (notice.kind) {
    case "legacy_pairing_migrated":
      return t("app.status.legacy_pairing_migrated", { file: notice.backup_file });
    case "invalid_settings_recovered":
      return t("app.status.invalid_settings_recovered", { file: notice.backup_file });
    default:
      return "";
  }
}

async function refreshStatus(): Promise<void> {
  if (statusRefreshRunning) {
    return;
  }
  statusRefreshRunning = true;
  try {
    const status = await invoke<RuntimeStatus>("sync_status");
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

  const runtimeNotice = getText("runtime-notice");
  const runtimeMessage = [status.last_error?.trim(), settingsNoticeMessage(status.settings_notice)]
    .filter((message): message is string => Boolean(message))
    .join("\n");
  setTextIfChanged(runtimeNotice, runtimeMessage);
  const noticeHidden = !runtimeMessage;
  if (runtimeNotice.hidden !== noticeHidden) {
    runtimeNotice.hidden = noticeHidden;
  }

  if (lastStatusKey !== nextStatusKey) {
    setTextIfChanged(getText("status-running"), `${t("app.status.label")}: ${
      status.running ? t("app.status.running") : t("app.status.stopped")
    }`);
    lastStatusKey = nextStatusKey;
  }
  if (previousLocalIp !== nextLocalIp) {
    renderNetworkOptions();
    renderDevices();
  }
}

async function scanDevices(): Promise<void> {
  if (!settingsReady) {
    return;
  }

  if (manualRefreshPromise) {
    if (manualRefreshGeneration === deviceRefreshGeneration) {
      return manualRefreshPromise;
    }
  }

  const refreshGeneration = deviceRefreshGeneration;
  manualRefreshGeneration = refreshGeneration;
  const request = performDeviceScan(refreshGeneration);
  manualRefreshPromise = request;
  try {
    await request;
  } finally {
    if (manualRefreshPromise === request) {
      manualRefreshPromise = null;
      manualRefreshGeneration = null;
      const button = document.querySelector("#refresh-domain") as HTMLButtonElement | null;
      if (button) {
        button.disabled = !settingsReady || saveSettingsRunning;
        button.textContent = t("app.domain.refresh");
      }
    }
  }
}

async function performDeviceScan(refreshGeneration: number): Promise<void> {
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
    deviceCacheAcceptedGeneration = refreshGeneration;
    mergeDiscoveredDevices(devices);
    renderDevices();
    feedback.textContent =
      devices.length > 0
        ? t("app.scan.done_found", { count: devices.length })
        : t("app.scan.done_none");
  } catch (error) {
    if (refreshGeneration !== deviceRefreshGeneration) {
      return;
    }

    try {
      const cachedDevices = await invoke<DiscoveredDevice[]>("cached_devices", {
        selectedLocalIp: getSelectedNetworkIp() || null,
      });
      if (refreshGeneration !== deviceRefreshGeneration) {
        return;
      }
      mergeDiscoveredDevices(cachedDevices);
      renderDevices();
    } catch {
      // Preserve the last in-memory device view when even the cache lookup fails.
    }
    deviceCacheAcceptedGeneration = refreshGeneration;
    feedback.textContent = t("app.scan.failed", { error: String(error) });
  }
}

async function refreshCachedDevices(): Promise<void> {
  if (
    !settingsReady ||
    deviceCacheAcceptedGeneration !== deviceRefreshGeneration ||
    (manualRefreshPromise && manualRefreshGeneration === deviceRefreshGeneration)
  ) {
    return;
  }
  const refreshGeneration = deviceRefreshGeneration;
  const devices = await invoke<DiscoveredDevice[]>("cached_devices", {
    selectedLocalIp: getSelectedNetworkIp() || null,
  });
  if (
    refreshGeneration !== deviceRefreshGeneration ||
    (manualRefreshPromise && manualRefreshGeneration === deviceRefreshGeneration)
  ) {
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
      void refreshTransferProgress().catch(reportRefreshFailure);
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
  startTimers();
  await ensureSettingsLoaded();
  try {
    await syncLaunchAtLogin(Boolean(settings.ui?.launch_at_login));
  } catch (error) {
    getText("config-feedback").textContent = t("app.settings.launch_at_login_failed", {
      error: String(error),
    });
  }
  const results = await Promise.allSettled([
    refreshStatus(),
    refreshTransferProgress(),
    refreshLogs(),
    refreshDomain(),
  ]);
  const failure = firstRejected(results);
  if (failure) {
    getText("scan-feedback").textContent = t("app.refresh.failed", {
      error: String(failure.reason),
    });
  }
}

function startTimers(): void {
  if (statusTimer !== null) {
    window.clearInterval(statusTimer);
  }
  if (transferTimer !== null) {
    window.clearInterval(transferTimer);
  }
  statusTimer = window.setInterval(() => {
    if (!settingsReady) {
      void recoverSettings();
    }
    void refreshStatus().catch(reportRefreshFailure);
    if (
      settingsReady &&
      deviceCacheAcceptedGeneration !== deviceRefreshGeneration
    ) {
      void scanDevices().catch(reportRefreshFailure);
    } else {
      void refreshCachedDevices().catch(reportRefreshFailure);
    }
  }, 1800);
  transferTimer = window.setInterval(() => {
    void refreshTransferProgress().catch(reportRefreshFailure);
  }, 500);
}

async function recoverSettings(): Promise<void> {
  if (settingsReady || settingsRecoveryRunning) {
    return;
  }
  settingsRecoveryRunning = true;
  try {
    await ensureSettingsLoaded();
    const results = await Promise.allSettled([refreshDomain(), refreshLogs()]);
    const failure = firstRejected(results);
    if (failure) {
      throw failure.reason;
    }
  } catch (error) {
    getText("scan-feedback").textContent = t("app.boot.failed", { error: String(error) });
  } finally {
    settingsRecoveryRunning = false;
  }
}

function reportRefreshFailure(error: unknown): void {
  getText("scan-feedback").textContent = t("app.refresh.failed", { error: String(error) });
}

function invalidateDiscoveryState(): void {
  deviceRefreshGeneration += 1;
  clearDiscoveredDevices();
  resetRefreshRenderCache();
  renderDevices();
}

function setSettingsFormEnabled(enabled: boolean): void {
  [
    "save-settings",
    "refresh-domain",
    "shared-code",
    "generate-pairing-key",
    "language",
    "network-ip",
    "max-item-mb",
    "launch-at-login",
  ].forEach((id) => {
    const control = document.querySelector(`#${id}`) as
      | HTMLButtonElement
      | HTMLInputElement
      | HTMLSelectElement
      | null;
    if (control) {
      control.disabled = !enabled;
    }
  });
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
      invalidateDiscoveryState();
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
    void refreshTransferProgress().catch(reportRefreshFailure);
    void refreshStatus().catch(reportRefreshFailure);
  });
  getInput("launch-at-login").addEventListener("change", markConfigDirty);
}

function bindActionButtons(): void {
  document.querySelector("#save-settings")?.addEventListener("click", () => {
    void saveSettings();
  });
  document.querySelector("#refresh-domain")?.addEventListener("click", () => {
    void refreshDomain().catch(reportRefreshFailure);
  });
  document.querySelector("#generate-pairing-key")?.addEventListener("click", () => {
    void generatePairingKey();
  });
  document.querySelector("#refresh-logs")?.addEventListener("click", () => {
    void refreshLogs().catch(reportRefreshFailure);
  });
  document.querySelector("#clear-logs")?.addEventListener("click", () => {
    void clearLogs().catch(reportRefreshFailure);
  });
}

window.addEventListener("DOMContentLoaded", () => {
  setSettingsFormEnabled(false);
  bindActionButtons();
  bindSettingsInputs();
  void boot().catch((error) => {
    getText("scan-feedback").textContent = t("app.boot.failed", { error: String(error) });
  });
});
