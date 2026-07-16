# Agent Entry

这是给代码 Agent 的低上下文路由，不是第二套 TODO 或架构正文。

## 先读

1. 当前工作区/会话注入的 `AGENTS.md` 与全局规则；仓库没有同名文件时仍以已注入规则为准。
2. `docs/todo.md`：唯一任务主记录、当前里程碑和验证状态。
3. 按任务选择最小专题：
   - 模块边界：`docs/architecture.md`
   - 线协议/队列：`docs/protocol.md`
   - 威胁模型/上限：`docs/security.md`
   - 当前能力：`docs/status.md`
   - 命令/测试/迁移：`docs/dev.md`

## 工程入口

- `Makefile`：安装、开发、构建、检查、测试与依赖审计的统一入口。
- `mise.toml`：Node、pnpm、Rust 版本基线。
- `src-tauri/src/lib.rs`：Tauri 初始化和 IPC 注册。
- `src-tauri/src/commands.rs`：最小设置 DTO、状态、发现、日志与进度 IPC。
- `src-tauri/src/settings.rs`：验证、原子保存和 v3→v4 配置迁移。
- `src-tauri/src/net/`：发现、握手、wire、sender/inbound、队列与生命周期。
- `src-tauri/src/clipboard/`：本机剪贴板格式、no-follow 文件访问、路径策略和平台适配。
- `src/main.ts`：前端启动与 IPC 编排；表单、设备、进度和样式已拆到子模块。

## 不变量

- 当前应用 `2.0.0` / TCP `v5`；不与 `v4` 互通，不允许明文 fallback。
- 本机 `ClipboardPayload` 可含路径但不实现 serde；线上 DTO 永远不能包含 `PathBuf`。
- 每条 TCP 连接必须先完成固定长度 PSK 握手，再读取任何长度前缀。
- `source_device_id`、session、control sequence、transfer UUID 和 chunk index 必须绑定验证。
- staging/log 使用 per-user app cache，不写仓库或同步工作区。
- 修改代码必须同步文档、验证、review，并把后续事项回写 `docs/todo.md`。

## 常用验证

```sh
make check
make test
make audit-web
pnpm exec tauri info
git diff --check
```

Windows 无 `make` 时使用 `docs/dev.md` 中的等价命令。真实剪贴板 smoke 会覆盖用户当前剪贴板，未经明确授权不要自动执行。
