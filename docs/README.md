# Docs Index

<p>
  <a href="README.zh-CN.md"><b>🇨🇳 中文</b></a> | <b>🇬🇧 English</b>
</p>

This folder focuses on two things:
- For humans: quickly find “what’s supported / how to develop / how to debug”
- For AI/agents: quickly locate “protocol / architecture / status / TODO / environment facts”

## Recommended Reading Order

### To understand current status quickly

1. `docs/status.md`: capabilities, limits, known constraints
2. `docs/architecture.md`: runtime structure and module boundaries
3. `docs/protocol.md`: wire protocol, scheduling, transfer, limits

### To develop / test / debug

1. `docs/dev.md`: boot, verification, logs, troubleshooting
2. `docs/todo.md`: milestones, risks, TODOs
3. `docs/status.md`: confirm the promised boundary before testing

### Security & environment

- `docs/security.md`: shared code and encryption strategy
- `docs/syncthing.md`: Syncthing mapping facts for macOS ↔ Windows in this environment

## Doc Responsibilities

- `docs/status.md`: what’s supported, what’s not, known issues
- `docs/architecture.md`: how modules/threads are split; control-plane vs data-plane
- `docs/protocol.md`: formats, scheduling and transfer rules
- `docs/dev.md`: how to run and debug; actionable steps only
- `docs/todo.md`: milestone/TODO index only

## Practical Suggestions

- Start with `docs/status.md` for external sharing
- Read `docs/architecture.md` + `docs/protocol.md` before changing networking/sync logic
- Use `docs/dev.md` for local testing and troubleshooting
