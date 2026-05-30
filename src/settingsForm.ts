import { t } from "./i18n";
import { escapeHtml } from "./html";
import { isPrivateIpv4 } from "./deviceList";
import type { NetworkInterfaceOption, Settings } from "./types";

const MIN_MAX_ITEM_MB = 1;
const MAX_MAX_ITEM_MB = 1000;

export function getInput(id: string): HTMLInputElement {
  return document.querySelector(`#${id}`) as HTMLInputElement;
}

export function getText(id: string): HTMLElement {
  return document.querySelector(`#${id}`) as HTMLElement;
}

export function populateSettingsForm(settings: Settings): { selectedNetworkIp: string; language: string } {
  getInput("encryption-enabled").checked = settings.security.encryption_enabled;
  getInput("launch-at-login").checked = Boolean(settings.ui?.launch_at_login);
  getInput("shared-code").value = settings.sync.shared_code;
  const mb = Math.max(1, Math.round(settings.limits.max_item_bytes / (1024 * 1024)));
  getInput("max-item-mb").value = String(mb);
  return {
    selectedNetworkIp: settings.sync.local_ip,
    language: (settings.ui?.language || "auto").trim() || "auto",
  };
}

export function renderLanguageOptions(selected: string): void {
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

export function applyI18nStatic(): void {
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
  setText("i18n-startup-title", "app.settings.startup");
  setText("i18n-launch-at-login-label", "app.settings.launch_at_login");
  setText("i18n-background-hint", "app.settings.background_hint");
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

export function renderNetworkOptions(
  selectedIp: string,
  networkOptions: NetworkInterfaceOption[],
  activeIp: string,
  recommendedIp: string,
): void {
  const select = document.querySelector("#network-ip") as HTMLSelectElement;
  const normalized = selectedIp.trim();
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

export function collectSettings(settings: Settings): Settings {
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
      launch_at_login: getInput("launch-at-login").checked,
    },
  };
}

export function markConfigDirty(): void {
  getText("config-feedback").textContent = t("app.settings.dirty");
}

export function validateSharedCode(): boolean {
  const code = getInput("shared-code").value.trim();
  if (!/^\d{6}$/.test(code)) {
    getText("scan-feedback").textContent = t("app.settings.code_invalid");
    getText("config-feedback").textContent = t("app.settings.code_invalid_save");
    return false;
  }
  return true;
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
