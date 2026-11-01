# Octra

[English](./README.md) | 简体中文

> 本文为中文翻译。英文版 [README.md](./README.md) 是默认版本，也是唯一事实来源；两者冲突时以英文版为准。

**Local-first Unified Gateway** — 统一管理 API、LLM、MCP 服务，并提供跨平台桌面应用。

当前仓库是 Octra 的可运行基础：独立 Daemon（统一数据面 + 认证控制面 + SQLite）、管理 CLI、Tauri 2 桌面端，以及 Rust/TypeScript 双 Workspace。API、LLM、MCP 三个 Gateway 的核心能力已实现；Agent 集成、插件与生产加固仍在路线图中。

## 仓库布局

```text
octra/
├── apps/
│   └── desktop/            # Tauri 2 + React + Vite 桌面端
│       └── src-tauri/      # Tauri Rust Host（Sidecar 管理的落点）
├── crates/
│   ├── octra-core/         # 领域类型与错误
│   ├── octra-config/       # 配置与本地目录解析
│   ├── octra-storage/      # SQLite + SQLx Migrations 与 Repository
│   ├── octra-observability/# tracing 初始化
│   ├── octra-data-plane/   # 共享 HTTP、Header、SSE 工具
│   ├── octra-api-core/     # API Gateway 领域类型
│   ├── octra-api-gateway/  # API 代理 + Service/Route/Upstream CRUD
│   ├── octra-gateway-runtime/ # API Runtime Snapshot、匹配器、负载均衡
│   ├── octra-gateway-api/  # 共享控制面（健康检查、认证、CORS）
│   ├── octra-llm-core/     # Canonical LLM IR
│   ├── octra-llm-protocol/ # OpenAI/Anthropic 协议适配
│   ├── octra-llm-gateway/  # LLM 路由、Provider、数据面与控制面
│   ├── octra-mcp-core/     # MCP JSON-RPC 类型
│   ├── octra-mcp-gateway/  # MCP stdio/HTTP 客户端、/mcp 端点、控制面
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

# 终端 1：启动 Daemon
# 控制面 http://127.0.0.1:8081，数据面 http://127.0.0.1:8080
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

控制面默认只绑定回环地址，且所有接口强制 Bearer 认证。数据面默认也绑定回环地址；需要其他网卡时修改 `gateway.listen_bind`。CLI 与桌面端从 `control.token` 读取凭据；`OCTRA_CONTROL_TOKEN` 可覆盖。

环境变量：`OCTRA_HOME`、`OCTRA_CONTROL_BIND`、`OCTRA_GATEWAY_BIND`、`OCTRA_LOG_LEVEL`、`OCTRA_LOG_FORMAT`（`pretty` / `json`）、`OCTRA_CONTROL_TOKEN`、`OCTRA_CONTROL_URL`（CLI）。

## 当前已实现

- 独立 Daemon，两个监听端口，统一优雅关闭。
- 认证控制面：健康检查/配置/重载，以及 services、routes、upstreams、targets、consumers、credentials、LLM providers/models/routing groups/routes/virtual keys、MCP servers/tools/ACLs/keys 的 CRUD。
- 常量时间 Bearer Token 认证，Token 随机生成并落盘。
- SQLite + SQLx 迁移，WAL / foreign_keys / busy_timeout；Runtime Snapshot 让热路径不直接查库。
- API Gateway：Host/Path/Method/Header 路由、最长前缀、轮询与加权负载均衡、路径剥离、超时、重试、API Key 认证、流式代理、访问日志与指标。
- LLM Gateway：Canonical IR、OpenAI ↔ Anthropic 请求/响应/SSE 转换、虚拟模型、优先级/权重路由与回退、OpenAI-compatible 与 Anthropic Provider（`env:` / `file:` Secret 引用）、虚拟密钥哈希存储、用量记录、`count_tokens`。
- MCP Gateway：Streamable HTTP 与 stdio 上游、工具发现与聚合、`/mcp` JSON-RPC 会话、按 Consumer 的 Tool ACL、API Key、调用审计。
- CLI：`status`、`services`、`routes`、`reload`、`doctor`。
- Tauri 2 + React 桌面端骨架，Dashboard 通过 Tauri command 访问 Daemon。
- 双 Workspace、CI（Rust fmt/clippy/test；前端 typecheck/test/build）。

## 尚未实现（按路线图推进）

- Pingora 代理引擎（当前统一数据面使用 Axum + reqwest）。
- OpenAI Responses 与 Gemini 协议适配。
- Agent Discovery / Config Adapter / Bridge。
- 插件运行时（Native / WASM）。
- OS Keychain Secrets 集成。
- 独立 Telemetry 数据库与保留策略 Worker。
- 跨平台打包、签名与公证。

## 安全边界

- SQLite 只保存 Secret 引用与 API Key 哈希，不保存明文凭据。
- 控制面强制认证，默认仅回环地址。
- 数据面默认仅回环地址；API 服务可选认证，LLM/MCP 在配置密钥后强制认证。
- Provider Secret 通过 `env:` 或 `file:` 引用在请求时解析，不写入日志。
- Daemon 不嵌入 Tauri 主进程，可独立运行与测试。
- 当前仍为开发版本；生产使用前需完成 Secrets、权限与打包加固。

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

## License

基于 [MIT License](./LICENSE) 发布。
