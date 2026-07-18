# Development Guide

## 入口与边界

- 当前能力与已知限制：`docs/status.md`
- 线协议、队列与测试参数：`docs/protocol.md`
- 威胁模型、资源上限与迁移：`docs/security.md`
- 模块所有权：`docs/architecture.md`
- 唯一任务主记录：`docs/todo.md`

运行数据不得写入仓库或同步工作区。settings 使用 Tauri 的用户级配置目录；staging 与 runtime log 使用运行时发现的用户级应用缓存目录。

## 工具链

仓库通过 `mise.toml` 声明基线：

| 工具 | 版本 |
| --- | ---: |
| Node.js | `24.6.0` |
| pnpm | `11.5.0` |
| Rust | `1.93.0` |

推荐先执行：

```sh
mise install
mise exec -- make install
```

没有 `mise` 时也可使用等价版本直接运行 `make` / `pnpm` / `cargo`，但提交前必须记录与基线的差异。

## Make 入口

`make` 或 `make help` 显示全部目标。

| 命令 | 作用 |
| --- | --- |
| `make install` | 严格按 `pnpm-lock.yaml` 安装前端与 Tauri CLI 依赖 |
| `make dev` | 启动 Tauri 开发应用 |
| `make dev-web` | 只启动 Vite 前端 |
| `make build-web` | TypeScript 检查并构建前端 |
| `make build` | 使用锁文件构建正式 Tauri bundle（Cargo runner 传入 `--locked`） |
| `make check` | 前端构建、Rust fmt check、全 target/feature 严格 Clippy |
| `make test` | 以 `--locked --all-targets --all-features` 运行 Rust 测试 |
| `make audit-web` | 通过 npm 官方 registry 审计依赖 |
| `make verify` | 顺序执行 check、test、前端依赖审计 |

Windows 没有 `make` 时使用等价命令：

```powershell
pnpm build
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo check --manifest-path src-tauri/Cargo.toml --locked --all-targets --all-features
cargo test --manifest-path src-tauri/Cargo.toml --locked --all-targets --all-features
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets --all-features -- -D warnings
pnpm audit --registry=https://registry.npmjs.org --audit-level moderate
```

CI 在标准 `macos-15` arm64 与 `windows-2025` x64 runner 上执行同一组检查，并使用 `cargo-deny` 对 `src-tauri/Cargo.lock` 执行 RustSec advisories 检查；第三方 actions 固定到已核对的 peeled commit，并在行尾保留版本标签索引。Rust 审计不混入本地默认 `make verify`，避免未安装审计工具时把编译验证误判为失败；需要本地复现时先安装与 CI 对齐的 `cargo-deny`，再运行 `cargo deny --manifest-path src-tauri/Cargo.toml check advisories`。

## 开发运行约定

- 后端在 Tauri `setup()` 中加载配置并启动同步，不依赖窗口或前端 IPC。
- 启动时只创建托盘/菜单栏；主窗口按需创建，关闭窗口只隐藏。
- macOS 与 Windows 都使用 clipboard change token + 后台轮询；生产轮询从 `50ms` 活跃值退避到 `500ms` 空闲值。
- `sync_status` 只读取状态；手动发现走 `discover_devices`，缓存读取走 `cached_devices`。
- 保存只提交最小 `SettingsUpdate`；device UUID、端口、轮询字段和加密不由前端覆盖。

显式选择的本机 IP 若格式错误或已不属于本机，运行时启动会失败并显示错误，不会静默改绑 `0.0.0.0`。自动网络模式才允许系统选择可用接口。

## v5 配对与迁移

- 应用 `2.0.1` 使用 TCP 协议 `v5`，不兼容 `v4`，也没有明文或旧协议 fallback。
- 同一共享域的所有设备必须一起升级；从 v4 升级时可保留原有的同一 26 位配对密钥。
- 旧 6 位配置会精确备份为 `settings.legacy-v3*.json`，再生成新密钥。
- 损坏或不安全配置会精确备份为 `settings.invalid-v4*.json`，再恢复安全默认值。
- 备份用于取证与整体回退，不应复制回只升级了部分设备的混合环境。

## 可调参数与快速测试

用户可调的 `max_item_bytes` 在 UI 中显示为“最大同步内容大小（MB）”，默认 `256KiB`，范围 `1 byte..=1000MiB`。它是文件的唯一总量上限，也是 PNG 的同步上限；PNG 另有明确的 `80MiB` 编码输入安全边界，用于限制解码内存风险。UI 以 MiB 展示，但提交与回读必须保持精确 byte 值。文本/HTML/RTF 仍受内部 `8MiB` 控制帧上限约束。

快速回归建议：

| 场景 | 测试值 | 预期 |
| --- | ---: | --- |
| byte 往返下界 | `1 B` | 保存后仍为 `1 B` |
| 默认值 | `256 KiB` | 无关设置保存不放大到 `1 MiB` |
| 边界前一位 | `1 MiB - 1 B` | 精确回读 |
| 普通文件 | `1 MiB` | 小文件流成功 |
| 用户上限 | `1000 MiB` | 配置可保存；无需在单测生成同体积 fixture |
| 超限 | 当前上限 `+1 B` | 本地保留、不广播、显示错误 |

生产时间与并发值保留在代码常量中；测试通过 loopback、小 payload、直接 codec 和内部可注入期限覆盖状态转换，不修改生产值。关键生产值包括：握手绝对总期限/连接超时 `2s`、帧 idle `30s` 与按大小计算的 `8s..120s` 总期限、PNG/文件接收 idle `30s` 与 `31s..30min` 总期限、UDP `500ms`、后台发现 `3s`、成员发现 TTL `30s`、后台/手动单次发现预算 `900/2200ms`、发送 peer 并发 `8`、入站连接 `16`/单 IP `4`、同时大载荷接收 `2`。

无 peer 的首发不会直接丢弃内容，而是复用 `30ms..500ms`、最多 `24` 次的发送退避。当前线性退避在约 `8.1s` 内耗尽；`30s` 是队列年龄上限而不是该路径的等待承诺。这个窗口覆盖正常的 `3s` mDNS / `500ms` UDP 收敛；若产品需要更长的离线发现宽限期，应单独调整策略并复核 UI 失败反馈。

快速 timeout 回归使用 `ReadDeadline`、`FileReceiveTimeouts` 或 loopback trickle fixture 注入毫秒级测试值；切回生产不需要配置迁移，因为生产值仍由常量计算。回归至少覆盖：握手/帧 trickle 不延长绝对期限、小文件预算为 `31s`、超大声明被封顶为 `30min`。

## 最小自动验证

macOS 或 Linux 开发机：

```sh
make verify
pnpm exec tauri info
git diff --check
```

Windows 必须在实际 Windows 工作区再跑等价验证；本机缺少 Windows Rust target 标准库时，不能把失败的交叉检查记为 Windows 已通过。双端目录由 Syncthing 管理时，先等待同步收敛再验证，不手工复制仓库，也不把 `target/`、`node_modules/`、日志、配置或 staging 纳入同步。

## 人工回归矩阵

自动测试不会改写用户剪贴板。发布前在隔离的测试剪贴板环境完成：

1. 同版本 macOS ↔ Windows 重新配对，错误密钥连接必须在握手阶段失败。
2. 双向复制纯文本、PNG、HTML、RTF；rich target 与纯文本 target 都能粘贴。
3. 双向复制小文件、目录、空目录；名称与层级保持，symlink/reparse point 明确拒绝。
4. 传输中停止同步或修改网络；worker 有限退出，未完成 staging 被删除。
5. 多 peer 中一台失败时，只重试失败 peer，成功 peer 不重复收到。
6. 显式选择失效 IP 时保存/启动失败且旧运行配置恢复。
7. 保持一台 peer 的 UDP 成员记录后，使一次后台或手动 mDNS 扫描返回空/局部结果：TTL 内设备列表不应退化为仅本机；此时复制的新文本应在有界发现重试后到达对端。
8. 复制文件或目录后，分别在 macOS Finder 桌面/文件夹和 Windows Explorer 桌面/文件夹粘贴，确认文件实际创建且名称、层级和内容正确。
9. 将文件复制结果粘贴到文本输入框，记录为“不支持文件目标语义”；不得把文本框失败误判为 Finder / Explorer 文件写回成功标准。

文件定向发送尚未实现，验收前不得把“广播到所有成员”当作“发送给指定成员”。

## 日志与定位

- UI 的“高级 / 日志”读取内存日志并支持清空。
- 文件日志位于运行时发现的 per-user app cache `runtime` 子目录，单文件最大 `2MiB` 并保留一份轮转；不要在文档或脚本中写死绝对路径。
- 单条日志最多 `8KiB`，配对密钥、派生密钥、nonce 和完整剪贴板正文不得写日志。

常用关键字：

- `profile local_clipboard`：读取、建模与指纹耗时。
- `profile file_send` / `profile file_recv`：文件数据面耗时与吞吐。
- `profile clipboard_apply`：写回系统剪贴板耗时。
- `peer handshake failed`：版本、密钥或认证不匹配。
- `discard incomplete inbound file`：断链、停止或完整性失败后的丢弃。
- `write peer failed`：连接或写超时。

调试高负载或大文件时，先使用小 fixture 验证控制流，再逐级增大；正式启动与长时间压测由用户确认后执行。

## 吞吐基准计划

当前没有可比较的网络带宽基线，不能把任一次 clipboard 传输速度直接当作协议极限。性能优化前应在同一 macOS ↔ Windows 链路依次测量：

1. 裸 TCP：固定 `64MiB`、`256MiB` 缓冲，记录单向有效 MiB/s。
2. 加密 raw 流：使用相同字节和会话，记录加密、写 socket、读 socket、解密的分段耗时。
3. 文件与 PNG：分别记录源读取/PNG 获取、数据面、接收校验、系统剪贴板写入耗时。
4. 分片对比：至少比较 `256KiB`、`1MiB`、`4MiB`；每组多次运行，记录中位数、最低值和 CPU/内存峰值。

只有裸 TCP 接近链路速率而加密 raw 流明显较低时，才优先优化 codec/分片；若 raw 流接近裸 TCP，则应把优化重点放在文件读取、PNG 编解码或系统剪贴板写入。多 TCP 流并行属于最后一步，因为它会改变拥塞、公平性、取消和单槽位最新值语义。

## 发行一致性

发布前必须保持以下版本一致：

- `package.json`
- `src-tauri/Cargo.toml`
- `src-tauri/tauri.conf.json`
- `src-tauri/Cargo.lock` 中本包条目

正式产物、升级说明、回退方法、双平台验证与人工 smoke 结果必须进入发布记录；本轮代码验证通过不等于已经发布。
