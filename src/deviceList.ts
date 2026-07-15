import { t } from "./i18n";
import { escapeHtml } from "./html";
import type { DiscoveredDevice, NetworkInterfaceOption, RuntimeStatus, Settings } from "./types";

class FifoPool<T> {
  private readonly items: Array<T | undefined>;
  private head = 0;
  private sizeValue = 0;

  constructor(private readonly capacity: number) {
    if (!Number.isInteger(capacity) || capacity <= 0) {
      throw new Error("FifoPool capacity must be a positive integer");
    }
    this.items = new Array<T | undefined>(capacity);
  }

  push(item: T): void {
    const writeIndex = (this.head + this.sizeValue) % this.capacity;
    this.items[writeIndex] = item;

    if (this.sizeValue < this.capacity) {
      this.sizeValue += 1;
      return;
    }

    this.head = (this.head + 1) % this.capacity;
  }

  replaceAll(items: T[]): void {
    this.clear();
    for (const item of items) {
      this.push(item);
    }
  }

  values(): T[] {
    const result: T[] = [];
    for (let offset = 0; offset < this.sizeValue; offset += 1) {
      const item = this.items[(this.head + offset) % this.capacity];
      if (item !== undefined) {
        result.push(item);
      }
    }
    return result;
  }

  clear(): void {
    this.items.fill(undefined);
    this.head = 0;
    this.sizeValue = 0;
  }
}

type DiscoveredDeviceWithSeen = DiscoveredDevice & {
  lastSeen: number;
};

type DeviceRenderContext = {
  container: HTMLElement;
  feedback: HTMLElement;
  memberCount: HTMLElement;
  status: RuntimeStatus | null;
  settings: Settings;
  refreshNetworkOptions: () => void;
};

const DISCOVERED_DEVICE_POOL_CAPACITY = 100;
const DISCOVERED_DEVICE_STALE_MS = 5000;
const lastDiscovered = new FifoPool<DiscoveredDeviceWithSeen>(DISCOVERED_DEVICE_POOL_CAPACITY);
let membersExpanded = false;
let lastDevicesKey = "";
let lastRenderedSelfKey = "";

export function mergeDiscoveredDevices(devices: DiscoveredDevice[]): void {
  const now = Date.now();
  const existingDevices = lastDiscovered.values();
  const existingById = new Map(existingDevices.map((device) => [device.device_id, device]));

  for (const device of devices) {
    const existing = existingById.get(device.device_id);
    if (existing) {
      existing.device_name = device.device_name;
      existing.addr = device.addr;
      existing.port = device.port;
      existing.lastSeen = now;
      continue;
    }

    lastDiscovered.push({ ...device, lastSeen: now });
  }

  lastDiscovered.replaceAll(
    lastDiscovered.values().filter((device) => now - device.lastSeen <= DISCOVERED_DEVICE_STALE_MS),
  );
}

export function discoveredDevices(): DiscoveredDevice[] {
  return lastDiscovered.values();
}

export function clearDiscoveredDevices(): void {
  lastDiscovered.clear();
  membersExpanded = false;
  resetDeviceRenderCache();
}

export function resetDeviceRenderCache(): void {
  lastDevicesKey = "";
  lastRenderedSelfKey = "";
}

export function renderDevices(context: DeviceRenderContext): number {
  const devices = lastDiscovered.values();
  const observedMemberCount = Math.max(1, devices.length + 1);
  const memberCountText = `${t("app.status.members")}: ${observedMemberCount}`;
  if (context.memberCount.textContent !== memberCountText) {
    context.memberCount.textContent = memberCountText;
  }

  const selfName = getLocalDeviceDisplayName(context.status, context.settings);
  const selfIp = context.status?.local_ip?.trim();
  const selfMeta = selfIp ? `${t("app.self")} · ${selfIp}` : t("app.self");
  const selfRenderKey = `${selfName}|${selfMeta}|${membersExpanded ? "1" : "0"}`;
  const orderedDevices = orderDevicesWithLocalFirst(devices, context.status, context.settings);
  const devicesKey = JSON.stringify(
    orderedDevices.map((device) => [device.device_id, device.device_name, device.addr, device.port]),
  );

  if (lastDevicesKey === devicesKey && lastRenderedSelfKey === selfRenderKey) {
    updateDeviceFeedback(context.feedback, devices.length);
    return observedMemberCount;
  }

  lastDevicesKey = devicesKey;
  lastRenderedSelfKey = selfRenderKey;
  const remoteRows = orderedDevices.length
    ? orderedDevices
        .map((device) =>
          renderDeviceRow(
            getDeviceDisplayName(device, context.status, context.settings),
            device.addr,
            t("app.domain.member_tag"),
          ),
        )
        .join("")
    : `<p class="empty-members">${escapeHtml(t("app.domain.empty"))}</p>`;

  context.container.innerHTML = `
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
  const details = context.container.querySelector("details");
  if (details) {
    details.addEventListener("toggle", () => {
      membersExpanded = details.open;
      const toggle = details.querySelector(".device-list-toggle");
      if (toggle) {
        toggle.textContent = details.open ? t("app.domain.collapse") : t("app.domain.view_all");
      }
    });
  }
  context.refreshNetworkOptions();
  updateDeviceFeedback(context.feedback, devices.length);
  return observedMemberCount;
}

export function selectRecommendedNetworkIp(
  networkOptions: NetworkInterfaceOption[],
  status: RuntimeStatus | null,
): string {
  const peerScopes = new Set(
    lastDiscovered
      .values()
      .map((device) => privateLanScopeKey(device.addr))
      .filter((value): value is string => Boolean(value)),
  );
  if (peerScopes.size > 0) {
    const matching = networkOptions.find((option) =>
      peerScopes.has(privateLanScopeKey(option.ip)),
    );
    if (matching) {
      return matching.ip;
    }
  }
  return status?.local_ip?.trim() || "";
}

export function privateLanScopeKey(ip: string): string {
  const parts = ip.trim().split(".").map(Number);
  if (
    parts.length !== 4 ||
    parts.some((part) => !Number.isInteger(part) || part < 0 || part > 255)
  ) {
    return "";
  }
  const [a, b] = parts;
  if (a === 10) return "rfc1918-10";
  if (a === 172 && b >= 16 && b <= 31) return "rfc1918-172";
  if (a === 192 && b === 168) return "rfc1918-192";
  // Do not invent a subnet for non-RFC1918 addresses without a real netmask.
  return "";
}

export function isPrivateIpv4(ip: string): boolean {
  return Boolean(privateLanScopeKey(ip));
}

function updateDeviceFeedback(feedback: HTMLElement, remoteCount: number): void {
  const message = remoteCount
    ? t("app.domain.online_total", { count: remoteCount + 1 })
    : t("app.domain.only_self");
  if (feedback.textContent !== message) {
    feedback.textContent = message;
  }
}

function renderDeviceRow(title: string, subtitle: string, tag: string, className = ""): string {
  const rowClass = className ? `device-row ${className}` : "device-row";
  return `<div class="${rowClass}"><span>${escapeHtml(title)} <em>${escapeHtml(subtitle)}</em></span><span class="device-tag">${escapeHtml(tag)}</span></div>`;
}

function isUuidLike(value: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value.trim());
}

function isDeviceNameFallback(deviceName: string): boolean {
  const trimmed = deviceName.trim();
  return (
    !trimmed ||
    trimmed === "当前设备" ||
    trimmed === "This device" ||
    trimmed === t("app.self.device") ||
    isUuidLike(trimmed)
  );
}

function getLocalDeviceDisplayName(status: RuntimeStatus | null, settings: Settings): string {
  const statusName = status?.device_name?.trim() ?? "";
  if (!isDeviceNameFallback(statusName)) {
    return statusName;
  }

  const localIds = new Set(
    [status?.device_id, settings?.sync?.device_id]
      .map((id) => id?.trim())
      .filter((id): id is string => Boolean(id)),
  );
  const discoveredLocalName = lastDiscovered
    .values()
    .find((device) => localIds.has(device.device_id.trim()) && !isDeviceNameFallback(device.device_name))
    ?.device_name.trim();

  return discoveredLocalName || statusName || t("app.self.device");
}

function getDeviceDisplayName(
  device: DiscoveredDevice,
  status: RuntimeStatus | null,
  settings: Settings,
): string {
  const localIds = new Set(
    [status?.device_id, settings?.sync?.device_id]
      .map((id) => id?.trim())
      .filter((id): id is string => Boolean(id)),
  );

  if (localIds.has(device.device_id.trim())) {
    return getLocalDeviceDisplayName(status, settings);
  }

  const deviceName = device.device_name.trim();
  return isDeviceNameFallback(deviceName) ? device.device_id : deviceName;
}

function orderDevicesWithLocalFirst(
  devices: DiscoveredDevice[],
  status: RuntimeStatus | null,
  settings: Settings,
): DiscoveredDevice[] {
  const localIds = new Set(
    [status?.device_id, settings?.sync?.device_id]
      .map((id) => id?.trim())
      .filter((id): id is string => Boolean(id)),
  );

  if (!localIds.size) {
    return devices;
  }

  return [...devices].sort((left, right) => {
    const leftIsLocal = localIds.has(left.device_id.trim());
    const rightIsLocal = localIds.has(right.device_id.trim());
    if (leftIsLocal === rightIsLocal) {
      return 0;
    }
    return leftIsLocal ? -1 : 1;
  });
}
