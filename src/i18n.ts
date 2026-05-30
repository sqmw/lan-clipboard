type Locale = "zh-CN" | "en-US";

type Dict = Record<string, string>;

const DICTS: Record<Locale, Dict> = {
  "zh-CN": {
    "app.domain.kicker": "共享域",
    "app.domain.title": "查看当前共享域成员与同步状态",
    "app.domain.refresh": "刷新",
    "app.domain.devices": "设备列表",
    "app.domain.empty": "当前没有发现其他共享域成员。",

    "app.status.kicker": "运行状态",
    "app.status.title": "同步状态",
    "app.status.label": "状态",
    "app.status.members": "共享域成员（含本机）",

    "app.transfer.title": "传输进度",
    "app.transfer.empty": "当前没有进行中的传输任务。",
    "app.transfer.expand": "展开",
    "app.transfer.collapse": "收起",
    "transfer.stats.current": "当前",
    "transfer.stats.average": "平均",
    "transfer.stats.peak": "峰值",
    "transfer.stats.elapsed": "用时",
    "transfer.stats.remaining": "剩余",

    "app.settings.kicker": "设置",
    "app.settings.title": "同步配置",
    "app.settings.save": "保存配置",
    "app.settings.saving": "保存中...",
    "app.settings.shared_code": "共享码",
    "app.settings.shared_code.placeholder": "6位数字",
    "app.settings.network": "使用网络",
    "app.settings.network.auto": "自动选择最合适的局域网网络",
    "app.settings.network.recommended": "推荐",
    "app.settings.network.active": "当前使用",
    "app.settings.max_mb": "最大发送大小（MB）",
    "app.settings.language": "语言",
    "app.settings.startup": "后台与启动",
    "app.settings.launch_at_login": "开机启动",
    "app.settings.background_hint": "应用启动后默认在后台运行；关闭窗口会保留在菜单栏 / 托盘。",
    "app.settings.language.auto": "跟随系统",
    "app.settings.language.zh": "中文",
    "app.settings.language.en": "English",

    "app.settings.hint":
      "推荐家庭或小团队共用一个 6 位码；同网段内填写相同共享码的设备都会加入同一个共享域。多网卡场景可明确指定使用哪一个本机网络。",
    "app.settings.dirty": "有未保存修改，点击“保存配置”后才会生效。",
    "app.settings.saving_feedback": "正在保存配置并应用全局设置...",
    "app.settings.saved_feedback": "配置已保存，并已按新配置重新应用。",
    "app.settings.save_failed": "保存失败：{error}",
    "app.settings.launch_at_login_failed": "开机启动状态同步失败：{error}",
    "app.settings.code_invalid": "共享码必须是 6 位数字。",
    "app.settings.code_invalid_save": "共享码必须是 6 位数字，配置未保存。",

    "app.advanced.summary": "高级 / 日志",
    "app.security.title": "高级加密",
    "app.security.encrypt": "加密传输（会降低速度）",
    "app.logs.title": "运行日志",
    "app.logs.refresh": "刷新",
    "app.logs.clear": "清空",

    "app.status.running": "运行中",
    "app.status.stopped": "已停止",
    "app.self.device": "本机设备",
    "app.self": "本机",
    "app.domain.member_tag": "共享域内",
    "app.domain.self_tag": "本机",
    "app.domain.view_all": "查看全部",
    "app.domain.collapse": "收起",
    "app.domain.only_self": "当前共享域只有本机在线。",
    "app.domain.online_total": "当前共享域共有 {count} 台设备在线，展开可查看全部设备。",
    "app.domain.scanning": "扫描中...",
    "app.scan.scanning": "正在扫描局域网设备...",
    "app.scan.done_found": "扫描完成，发现 {count} 台其他共享域成员在线。",
    "app.scan.done_none": "扫描完成，当前只有本机在线。",
    "app.scan.failed": "扫描失败：{error}",
    "app.network.switched": "网络已切换，正在按当前选择刷新共享域...",
    "app.refresh.failed": "刷新失败：{error}",
    "app.boot.failed": "启动错误: {error}",

    "transfer.send": "发送到",
    "transfer.recv": "接收自",
    "transfer.status.sending": "发送中",
    "transfer.status.receiving": "接收中",
    "transfer.status.pending_apply": "等待写入",
    "transfer.status.applying": "写入剪贴板",
    "transfer.status.retrying": "重试中",
    "transfer.status.completed": "已完成",
    "transfer.status.failed": "失败",
    "transfer.status.received": "已接收",
    "transfer.preview.html": "HTML 内容",
    "transfer.preview.rtf": "RTF 内容",
    "transfer.preview.text": "文字内容",
    "transfer.label.text": "直接复制文字",
    "transfer.label.html": "HTML 富文本",
    "transfer.label.rtf": "RTF 富文本",
    "transfer.label.image": "图片",
    "transfer.label.file": "文件",
    "transfer.label.text_file": "文本文件",
    "transfer.label.unknown": "内容",

    "app.settings.initial_feedback": "修改共享码、同步配置或高级设置后，点击“保存配置”才会生效。",
  },
  "en-US": {
    "app.domain.kicker": "Shared Domain",
    "app.domain.title": "Members & Sync Status",
    "app.domain.refresh": "Refresh",
    "app.domain.devices": "Devices",
    "app.domain.empty": "No other members found in this shared domain.",

    "app.status.kicker": "Status",
    "app.status.title": "Sync Status",
    "app.status.label": "State",
    "app.status.members": "Members (incl. this device)",

    "app.transfer.title": "Transfers",
    "app.transfer.empty": "No active transfers.",
    "app.transfer.expand": "Expand",
    "app.transfer.collapse": "Collapse",
    "transfer.stats.current": "Current",
    "transfer.stats.average": "Average",
    "transfer.stats.peak": "Peak",
    "transfer.stats.elapsed": "Elapsed",
    "transfer.stats.remaining": "Remaining",

    "app.settings.kicker": "Settings",
    "app.settings.title": "Sync Settings",
    "app.settings.save": "Save",
    "app.settings.saving": "Saving...",
    "app.settings.shared_code": "Shared Code",
    "app.settings.shared_code.placeholder": "6 digits",
    "app.settings.network": "Network",
    "app.settings.network.auto": "Auto select best LAN network",
    "app.settings.network.recommended": "Recommended",
    "app.settings.network.active": "In use",
    "app.settings.max_mb": "Max send size (MB)",
    "app.settings.language": "Language",
    "app.settings.startup": "Background & Startup",
    "app.settings.launch_at_login": "Launch at login",
    "app.settings.background_hint":
      "The app starts in the background by default and stays in the tray / menu bar when the window is closed.",
    "app.settings.language.auto": "System",
    "app.settings.language.zh": "中文",
    "app.settings.language.en": "English",

    "app.settings.hint":
      "Use the same 6-digit shared code on devices in the same LAN to join one shared domain. For multi-NIC devices, pick the correct local IP.",
    "app.settings.dirty": "Unsaved changes. Click “Save” to apply.",
    "app.settings.saving_feedback": "Saving settings and applying...",
    "app.settings.saved_feedback": "Saved and applied.",
    "app.settings.save_failed": "Save failed: {error}",
    "app.settings.launch_at_login_failed": "Failed to sync launch-at-login state: {error}",
    "app.settings.code_invalid": "Shared code must be 6 digits.",
    "app.settings.code_invalid_save": "Shared code must be 6 digits. Not saved.",

    "app.advanced.summary": "Advanced / Logs",
    "app.security.title": "Encryption",
    "app.security.encrypt": "Encrypt transfers (slower)",
    "app.logs.title": "Runtime Logs",
    "app.logs.refresh": "Refresh",
    "app.logs.clear": "Clear",

    "app.status.running": "Running",
    "app.status.stopped": "Stopped",
    "app.self.device": "This device",
    "app.self": "Local",
    "app.domain.member_tag": "Member",
    "app.domain.self_tag": "Local",
    "app.domain.view_all": "View all",
    "app.domain.collapse": "Collapse",
    "app.domain.only_self": "Only this device is online.",
    "app.domain.online_total": "{count} devices online. Expand to view all.",
    "app.domain.scanning": "Scanning...",
    "app.scan.scanning": "Scanning LAN devices...",
    "app.scan.done_found": "Scan complete. Found {count} other member(s).",
    "app.scan.done_none": "Scan complete. Only this device is online.",
    "app.scan.failed": "Scan failed: {error}",
    "app.network.switched": "Network changed. Refreshing using current selection...",
    "app.refresh.failed": "Refresh failed: {error}",
    "app.boot.failed": "Boot error: {error}",

    "transfer.send": "Send to",
    "transfer.recv": "Receive from",
    "transfer.status.sending": "Sending",
    "transfer.status.receiving": "Receiving",
    "transfer.status.pending_apply": "Pending apply",
    "transfer.status.applying": "Applying",
    "transfer.status.retrying": "Retrying",
    "transfer.status.completed": "Completed",
    "transfer.status.failed": "Failed",
    "transfer.status.received": "Received",
    "transfer.preview.html": "HTML content",
    "transfer.preview.rtf": "RTF content",
    "transfer.preview.text": "Text content",
    "transfer.label.text": "Plain text",
    "transfer.label.html": "HTML rich text",
    "transfer.label.rtf": "RTF rich text",
    "transfer.label.image": "Image",
    "transfer.label.file": "File",
    "transfer.label.text_file": "Text file",
    "transfer.label.unknown": "Content",

    "app.settings.initial_feedback": "After changing settings, click “Save” to apply.",
  },
};

let currentLocale: Locale = "zh-CN";

function systemLocale(): Locale {
  const lang = (navigator.language || "").toLowerCase();
  return lang.startsWith("zh") ? "zh-CN" : "en-US";
}

export function setLocale(value: string): Locale {
  const next = value.trim();
  currentLocale = next === "en-US" ? "en-US" : next === "zh-CN" ? "zh-CN" : systemLocale();
  return currentLocale;
}

export function getLocale(): Locale {
  return currentLocale;
}

export function t(key: string, vars?: Record<string, string | number>): string {
  const dict = DICTS[currentLocale] ?? DICTS["zh-CN"];
  const template = dict[key] ?? DICTS["zh-CN"][key] ?? key;
  if (!vars) return template;
  return template.replace(/\{(\w+)\}/g, (_, name: string) => String(vars[name] ?? `{${name}}`));
}
