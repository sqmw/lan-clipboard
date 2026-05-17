# Dev Guide

## 启动

- 安装依赖：`pnpm install`
- 开发运行：`pnpm tauri dev`
- Windows 侧依赖 `package.json` 中的 `pnpm exec tauri` 脚本桥接本地 CLI；若 `node_modules` 为空，先重新执行 `pnpm install`

## 回归验证清单（M0）

- macOS 与 Windows 两端都能启动应用
- 设置页能修改 `max_item_bytes` 并落盘（重启后仍生效）
- 可通过“扫描局域网设备（mDNS）”发现候选设备并加入 peers
- 两端启用加密并设置一致配对码时可正常同步
- 两端配对码不一致时，同步失败且日志出现解密失败提示
- 点击“保存配置 + 开始同步”后状态变为运行中
- 复制文本：另一端收到并写入剪贴板（已实现基础防回环）
- 复制图片：另一端收到并写入剪贴板（超限会被阻止）

## M0 测试态参数（建议）

- `poll_interval_ms`：测试态 `500`，默认 `900`
- `max_item_bytes`：测试态 `65536`（便于构造超限用例）

## 常见联调问题

- 两端都“已停止”：检查是否点击“开始同步”
- 一端有错误：查看“最近错误”，常见是端口被占用或 peer 地址不可达
- 复制无反应：确认双端都在同一网段，且防火墙允许监听端口入站
- 扫描不到设备：确认两端都打开应用，且路由器/AP 未阻止组播流量
- 设备码连不上：先手动扫描一次，确保设备出现在列表里，再输入 6 位设备码连接
- 日志提示 `decrypt failed`：两端配对码不一致，或某端仍在发送未加密消息
- 日志提示 `received plain frame but encryption enabled`：双方加密开关不一致
- 若历史版本扫描设备时反复出现 `mdns_sd::service_daemon ... closed channel`，更新到当前版本；当前扫描流程已改为先 `stop_browse` 再 `shutdown`
