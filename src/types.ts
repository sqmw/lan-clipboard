export type Settings = {
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
    launch_at_login: boolean;
  };
};

export type SettingsUpdate = {
  max_item_bytes: number;
  shared_code: string;
  local_ip: string;
  language: string;
  launch_at_login: boolean;
};

export type SettingsNotice = {
  kind: "legacy_pairing_migrated" | "invalid_settings_recovered";
  backup_file: string;
};

export type RuntimeStatus = {
  running: boolean;
  device_id: string;
  device_name: string;
  local_ip?: string | null;
  last_error: string | null;
  settings_notice: SettingsNotice | null;
  recent_log_count: number;
  peer_count: number;
};

export type DiscoveredDevice = {
  device_id: string;
  device_name: string;
  addr: string;
  port: number;
};

export type NetworkInterfaceOption = {
  name: string;
  ip: string;
  label: string;
};

export type RuntimeLog = {
  ts_ms: number;
  level: string;
  message: string;
};
