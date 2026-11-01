# Octra

**Local-first Unified Gateway** — 统一管理 API、LLM、MCP 服务，并提供跨平台桌面应用。

当前仓库是 Octra 的可运行基础骨架：独立 Daemon（Axum 控制面 + SQLite）、管理 CLI、Tauri 2 桌面端，以及 Rust/TypeScript 双 Workspace。LLM 协议转换、Pingora 数据面、MCP Gateway 等按架构文档中的 Phase 逐步实现。

## 仓库布局

```text
octra/
├── apps/
│   └── desktop/            # Tauri 2 + React + Vite 桌面端
│       └── src-tauri/      # Tauri Rust Host（Sidecar 管理的落点）
├── crates/
│   ├── octra-core/         # 领域类型与错误
│   ├── octra-config/       # 配置与本地目录解析
│   ├── octra-storage/      # SQLite + SQLx Migrations
│   ├── octra-observability/# tracing 初始化
│   ├── octra-gateway-runtime/ # Runtime Snapshot + 原子重载
│   ├── octra-gateway-api/  # Axum 控制面（Bearer 认证）
│   ├── octra-daemon/       # 独立 Gateway Daemon
│   └── octra-cli/          # 命令行管理
├── packages/
│   ├── types/              # 共享 TypeScript 类型
│   ├── api-client/         # 控制面 API Client
│   ├── ui/                 # 共享 UI 组件
│   ├── tsconfig/           # 共享 tsconfig
│   └── design-tokens/      # 设计令牌
├── migrations/             # SQLx 迁移
├── configs/default.toml    # Daemon 默认配置
├── scripts/
└── tests/                  # 集成 / 协议兼容 / 跨平台测试
```

## 快速开始

前置：Rust 1.97+、Node 24+、pnpm 10+。

```bash
pnpm install

# 终端 1：启动 Daemon（控制面 http://127.0.0.1:8081）
pnpm daemon

# 终端 2：查看状态
pnpm cli status
pnpm cli services

# 桌面端（需要 Tauri 依赖，见下）
pnpm dev

# 仅浏览器调试前端
pnpm dev:web
```

Daemon 首次启动会在 `$OCTRA_HOME`（默认 `~/.octra`）下创建：

| 文件 | 说明 |
|---|---|
| `config.toml` | Daemon 配置 |
| `octra.db` | SQLite 数据库（WAL） |
| `control.token` | 控制面 Bearer Token，Unix 下权限 `0600` |

控制面默认只绑定回环地址，且所有接口强制 Bearer 认证。CLI 与桌面端从 `control.token` 读取凭据；`OCTRA_CONTROL_TOKEN` 可覆盖。

环境变量：`OCTRA_HOME`、`OCTRA_CONTROL_BIND`、`OCTRA_LOG_LEVEL`、`OCTRA_LOG_FORMAT`（`pretty` / `json`）、`OCTRA_CONTROL_URL`（CLI）。

## 当前已实现

- 独立 Daemon 生命周期与优雅关闭。
- Axum 控制面：`/api/v1/system/health`、`/api/v1/system/config`、`/api/v1/services`、`/api/v1/routes`、`/api/v1/system/reload`。
- 常量时间 Bearer Token 认证，Token 随机生成并落盘。
- SQLite + SQLx 迁移，WAL / foreign_keys / busy_timeout。
- Runtime Snapshot + 原子替换，代理热路径不直接查库（架构文档 13.2 的落点）。
- CLI：`status`、`services`、`routes`、`reload`、`doctor`。
- Tauri 2 + React 桌面端骨架，Dashboard 通过 Tauri command 访问 Daemon。
- 双 Workspace、CI（Rust fmt/clippy/test；前端 typecheck/test/build）。

## 尚未实现（按路线图推进）

- Pingora 数据面（API Gateway 实际转发）。
- LLM Canonical IR、协议适配器、Provider 路由。
- Agent Discovery / Config Adapter / Bridge。
- MCP Gateway 与 Tool ACL。
- 插件运行时（Native / WASM）。
- OS Keychain Secrets 集成。
- 跨平台打包、签名与公证。

## 安全边界

- SQLite 只保存 Secret 引用，不保存明文。
- 控制面强制认证，默认仅回环地址。
- Daemon 不嵌入 Tauri 主进程，可独立运行与测试。
- 当前为开发骨架；生产使用前需完成 Secrets、权限与打包加固。

## 校验

```bash
./scripts/check.sh
```

或分别执行：

```bash
cargo fmt --all --check
cargo clippy --workspace --exclude octra-desktop --all-targets -- -D warnings
cargo test --workspace --exclude octra-desktop
pnpm typecheck && pnpm test && pnpm build
```

Tauri 桌面端需要系统 WebView 依赖（macOS 为 Xcode Command Line Tools，Linux 为 WebKitGTK），因此 CI 的 Rust 任务暂不编译 `octra-desktop`。
