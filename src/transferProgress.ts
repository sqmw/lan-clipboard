import { t } from "./i18n";

export type TransferProgress = {
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

type TransferStats = {
  lastBytes: number;
  lastSeenMs: number;
  startedAtMs: number;
  speedBytesPerSecond: number;
  maxSpeedBytesPerSecond: number;
};

const expandedTransferIds = new Set<string>();
const transferPreviewScrollTops = new Map<string, number>();
const transferStatsById = new Map<string, TransferStats>();
let isTransferPreviewInteracting = false;
let transferPreviewIdleTimer: number | null = null;
let refreshTransferProgress: (() => void) | null = null;

export function isTransferPreviewBusy(): boolean {
  return isTransferPreviewInteracting;
}

export function renderTransferProgress(
  container: HTMLElement,
  transfers: TransferProgress[],
  requestRefresh: () => void,
): void {
  refreshTransferProgress = requestRefresh;
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

  pruneTransferUiState(transfers);
  container.innerHTML = transfers.map(renderTransferCard).join("");
  bindTransferPreviewToggles(container, transfers);
  bindTransferPreviewScroll(container);
}

function renderTransferCard(transfer: TransferProgress): string {
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
      <p class="transfer-meta">${escapeHtml(renderTransferStatsLine(transfer))}</p>
      ${error}
    </article>
  `;
}

function pruneTransferUiState(transfers: TransferProgress[]): void {
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
}

function bindTransferPreviewToggles(container: HTMLElement, transfers: TransferProgress[]): void {
  container.querySelectorAll<HTMLButtonElement>(".transfer-preview-toggle").forEach((button) => {
    button.addEventListener("click", () => {
      const id = button.dataset.transferId;
      if (!id) return;
      if (expandedTransferIds.has(id)) {
        expandedTransferIds.delete(id);
      } else {
        expandedTransferIds.add(id);
      }
      renderTransferProgress(container, transfers, refreshTransferProgress ?? (() => {}));
    });
  });
}

function bindTransferPreviewScroll(container: HTMLElement): void {
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

function renderTransferStatsLine(transfer: TransferProgress): string {
  const now = Date.now();
  const existing = transferStatsById.get(transfer.id);
  const stats = updateTransferStats(transfer, existing, now);
  transferStatsById.set(transfer.id, stats);

  const elapsedSeconds = (now - stats.startedAtMs) / 1000;
  const remainingBytes = Math.max(0, transfer.total_bytes - transfer.transferred_bytes);
  const etaSeconds = stats.speedBytesPerSecond > 0 ? remainingBytes / stats.speedBytesPerSecond : 0;
  const percent = `${transfer.percent}%`;
  const averageSpeedBytesPerSecond = elapsedSeconds > 0 ? transfer.transferred_bytes / elapsedSeconds : 0;
  const speedSummary = `${t("transfer.stats.current")} ${formatTransferSpeed(stats.speedBytesPerSecond)} · ${t(
    "transfer.stats.average",
  )} ${formatTransferSpeed(averageSpeedBytesPerSecond)} · ${t("transfer.stats.peak")} ${formatTransferSpeed(
    stats.maxSpeedBytesPerSecond,
  )}`;

  const timeSummary =
    transfer.status === "completed" || transfer.percent >= 100
      ? `${speedSummary} · ${t("transfer.stats.elapsed")} ${formatTransferDuration(elapsedSeconds)}`
      : `${speedSummary} · ${t("transfer.stats.elapsed")} ${formatTransferDuration(elapsedSeconds)} / ${t(
          "transfer.stats.remaining",
        )} ${formatTransferDuration(etaSeconds)}`;

  return `${percent} · ${timeSummary}`;
}

function updateTransferStats(
  transfer: TransferProgress,
  existing: TransferStats | undefined,
  now: number,
): TransferStats {
  if (!existing || transfer.transferred_bytes < existing.lastBytes || transfer.status === "queued") {
    return {
      lastBytes: transfer.transferred_bytes,
      lastSeenMs: now,
      startedAtMs: now,
      speedBytesPerSecond: 0,
      maxSpeedBytesPerSecond: 0,
    };
  }

  const elapsedSeconds = Math.max(0.001, (now - existing.lastSeenMs) / 1000);
  const bytesDelta = Math.max(0, transfer.transferred_bytes - existing.lastBytes);
  const instantSpeed = bytesDelta / elapsedSeconds;
  const speedBytesPerSecond =
    instantSpeed > 0
      ? existing.speedBytesPerSecond > 0
        ? existing.speedBytesPerSecond * 0.65 + instantSpeed * 0.35
        : instantSpeed
      : existing.speedBytesPerSecond;

  return {
    lastBytes: transfer.transferred_bytes,
    lastSeenMs: now,
    startedAtMs: existing.startedAtMs,
    speedBytesPerSecond,
    maxSpeedBytesPerSecond: Math.max(existing.maxSpeedBytesPerSecond, speedBytesPerSecond),
  };
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
    refreshTransferProgress?.();
  }, delayMs);
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatTransferDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "0s";
  const totalSeconds = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const secs = totalSeconds % 60;
  if (hours > 0) return `${hours}h ${minutes}m ${secs}s`;
  if (minutes > 0) return `${minutes}m ${secs}s`;
  return `${secs}s`;
}

function formatTransferSpeed(bytesPerSecond: number): string {
  if (!Number.isFinite(bytesPerSecond) || bytesPerSecond <= 0) return "0 B/s";
  if (bytesPerSecond < 1024) return `${bytesPerSecond.toFixed(0)} B/s`;
  if (bytesPerSecond < 1024 * 1024) return `${(bytesPerSecond / 1024).toFixed(1)} KB/s`;
  return `${(bytesPerSecond / (1024 * 1024)).toFixed(1)} MB/s`;
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
