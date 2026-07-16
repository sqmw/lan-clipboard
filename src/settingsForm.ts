import { t } from "./i18n";
import { escapeHtml } from "./html";
import { isPrivateIpv4 } from "./deviceList";
import type { NetworkInterfaceOption, Settings, SettingsUpdate } from "./types";

const BYTES_PER_MIB = 1024 * 1024;
const MIN_MAX_ITEM_BYTES = 1;
const MAX_MAX_ITEM_BYTES = 1000 * BYTES_PER_MIB;
const MIN_SHARED_CODE_UNIQUE_CHARACTERS = 10;
const OBVIOUS_SEQUENCE_LENGTH = 8;
const SHARED_CODE_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const SHARED_CODE_PATTERN = /^[A-HJ-NP-Z2-9]{26}$/;

export function getInput(id: string): HTMLInputElement {
  return document.querySelector(`#${id}`) as HTMLInputElement;
}

export function getText(id: string): HTMLElement {
  return document.querySelector(`#${id}`) as HTMLElement;
}

export function populateSettingsForm(settings: Settings): { selectedNetworkIp: string; language: string } {
  getInput("launch-at-login").checked = Boolean(settings.ui?.launch_at_login);
  getInput("shared-code").value = settings.sync.shared_code;
  getInput("max-item-mb").value = formatMaxItemMib(settings.limits.max_item_bytes);
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
  setText("generate-pairing-key", "app.settings.generate_key");
  setText("i18n-shared-code-label", "app.settings.shared_code");
  setText("i18n-network-label", "app.settings.network");
  setText("i18n-max-mb-label", "app.settings.max_mb");
  setText("i18n-max-mb-hint", "app.settings.max_mb_hint");
  setText("i18n-language-label", "app.settings.language");
  setText("i18n-startup-title", "app.settings.startup");
  setText("i18n-launch-at-login-label", "app.settings.launch_at_login");
  setText("i18n-background-hint", "app.settings.background_hint");
  setText("i18n-settings-hint", "app.settings.hint");
  setText("config-feedback", "app.settings.initial_feedback");
  setText("i18n-advanced-summary", "app.advanced.summary");
  setText("i18n-security-title", "app.security.title");
  setText("i18n-encrypt-label", "app.security.encrypt");
  setText("i18n-encrypt-status", "app.security.status");
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
  const effectiveSelected = normalized;
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

export function collectSettingsUpdate(_settings: Settings): SettingsUpdate {
  const max_item_bytes = readMaxItemBytes();
  getInput("max-item-mb").value = formatMaxItemMib(max_item_bytes);

  return {
    max_item_bytes,
    shared_code: normalizeSharedCode(getInput("shared-code").value),
    local_ip: (document.querySelector("#network-ip") as HTMLSelectElement).value.trim(),
    language: (document.querySelector("#language") as HTMLSelectElement).value.trim() || "auto",
    launch_at_login: getInput("launch-at-login").checked,
  };
}

export function markConfigDirty(): void {
  getText("config-feedback").textContent = t("app.settings.dirty");
}

export function validateSharedCode(): boolean {
  const input = getInput("shared-code");
  const code = normalizeSharedCode(input.value);
  input.value = code;
  if (!SHARED_CODE_PATTERN.test(code)) {
    input.setAttribute("aria-invalid", "true");
    getText("scan-feedback").textContent = t("app.settings.code_invalid");
    getText("config-feedback").textContent = t("app.settings.code_invalid_save");
    return false;
  }
  if (!isStrongSharedCode(code)) {
    input.setAttribute("aria-invalid", "true");
    getText("scan-feedback").textContent = t("app.settings.code_weak");
    getText("config-feedback").textContent = t("app.settings.code_weak_save");
    return false;
  }
  input.setAttribute("aria-invalid", "false");
  return true;
}

export function validateMaxItemSize(): boolean {
  const input = getInput("max-item-mb");
  try {
    readMaxItemBytes();
    input.setAttribute("aria-invalid", "false");
    return true;
  } catch {
    input.setAttribute("aria-invalid", "true");
    getText("config-feedback").textContent = t("app.settings.max_mb_invalid");
    return false;
  }
}

export function normalizeSharedCode(value: string): string {
  return value.replace(/[\s-]+/g, "").toUpperCase();
}

export function isStrongSharedCode(value: string): boolean {
  // This is only an obvious-pattern filter, not an entropy estimate. Keys
  // generated by the backend get their unpredictability from its CSPRNG.
  if (new Set(value).size < MIN_SHARED_CODE_UNIQUE_CHARACTERS) {
    return false;
  }
  for (let period = 1; period < value.length; period += 1) {
    let repeats = true;
    for (let index = period; index < value.length; index += 1) {
      if (value[index] !== value[index % period]) {
        repeats = false;
        break;
      }
    }
    if (repeats) {
      return false;
    }
  }
  return !hasObviousAlphabetSequence(value);
}

function hasObviousAlphabetSequence(value: string): boolean {
  const indices = [...value].map((character) => SHARED_CODE_ALPHABET.indexOf(character));
  if (indices.some((index) => index < 0)) {
    return true;
  }
  for (let start = 0; start + OBVIOUS_SEQUENCE_LENGTH <= indices.length; start += 1) {
    const window = indices.slice(start, start + OBVIOUS_SEQUENCE_LENGTH);
    const ascending = window
      .slice(1)
      .every(
        (index, offset) =>
          index === (window[offset] + 1) % SHARED_CODE_ALPHABET.length,
      );
    const descending = window
      .slice(1)
      .every(
        (index, offset) =>
          index ===
          (window[offset] + SHARED_CODE_ALPHABET.length - 1) % SHARED_CODE_ALPHABET.length,
      );
    if (ascending || descending) {
      return true;
    }
  }
  return false;
}

function readMaxItemBytes(): number {
  const input = getInput("max-item-mb");
  if (!input.value.trim()) {
    throw new Error("max item size is required");
  }
  const requestedMib = input.valueAsNumber;
  const requestedBytes = requestedMib * BYTES_PER_MIB;
  if (
    !Number.isFinite(requestedMib) ||
    !Number.isFinite(requestedBytes) ||
    requestedBytes < MIN_MAX_ITEM_BYTES ||
    requestedBytes > MAX_MAX_ITEM_BYTES
  ) {
    throw new Error("max item size is outside the supported range");
  }
  const roundedBytes = Math.round(requestedBytes);
  if (!Number.isSafeInteger(roundedBytes)) {
    throw new Error("max item size cannot be represented safely");
  }
  return roundedBytes;
}

function formatMaxItemMib(bytes: number): string {
  return (bytes / BYTES_PER_MIB).toFixed(20).replace(/\.?0+$/, "");
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
