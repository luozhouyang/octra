# Octra

[English](./README.md) | 简体中文

> 本文为中文翻译。英文版 [README.md](./README.md) 是默认版本，也是唯一事实来源；两者冲突时以英文版为准。

**Local-first Unified Gateway** — 统一管理 API、LLM、MCP 服务，并提供跨平台桌面应用。

当前仓库是 Octra 的可运行基础：独立 Daemon（统一数据面 + 认证控制面 + SQLite）、管理 CLI、Tauri 2 桌面端，以及 Rust/TypeScript 双 Workspace。API、LLM、MCP 三个 Gateway 的核心能力已实现；Agent 配置托管与插件运行时已落地，生产加固仍在路线图中。

三个 Gateway 分别参考 Kong（API Gateway）、yetone/magpie（LLM Gateway，Kong AI Gateway 2.x 作为控制面次级参考）和 Kong AI MCP Proxy（MCP Gateway）。采纳/不采纳边界与当前实现进度见 [`.livingdocs/architecture/reference-models.md`](./.livingdocs/architecture/reference-models.md)。

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
│   ├── octra-runtime/      # API Runtime Snapshot、匹配器、负载均衡
│   ├── octra-types/        # 共享控制面（健康检查、认证、CORS）
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

| 文件            | 说明                                    |
| --------------- | --------------------------------------- |
| `config.toml`   | Daemon 配置                             |
| `octra.db`      | SQLite 数据库（WAL）                    |
| `control.token` | 控制面 Bearer Token，Unix 下权限 `0600` |

控制面默认只绑定回环地址，且所有接口强制 Bearer 认证。数据面默认也绑定回环地址；需要其他网卡时修改 `gateway.listen_bind`。CLI 与桌面端从 `control.token` 读取凭据；`OCTRA_CONTROL_TOKEN` 可覆盖。

环境变量：`OCTRA_HOME`、`OCTRA_CONTROL_BIND`、`OCTRA_GATEWAY_BIND`、`OCTRA_LOG_LEVEL`、`OCTRA_LOG_FORMAT`（`pretty` / `json`）、`OCTRA_CONTROL_TOKEN`、`OCTRA_CONTROL_URL`（CLI）。

## 连接 Coding Agent

Claude Code、Codex CLI、OpenCode 可以把 Octra 作为 LLM 端点。先创建一个
`virtual_model_id` 与 Agent 请求模型名一致的 LLM route，再为 Agent 创建
virtual key：

```bash
pnpm cli keys create --name claude-code --consumer <consumer-id> \
  --permissions '{"allowed_models":["octra-default"]}'
```

把返回的 `octra-...` key 写入 Agent 配置。`control.token`（8081 端口）是
管理面凭据，绝不能交给 Agent。控制面准备、Claude Code / Codex / OpenCode
模板、限流响应头和轮转流程见
[examples/agents/README.md](./examples/agents/README.md)。

### 在不改动真实配置的情况下测试

启动 Daemon 前配置沙箱：

```toml
[agents]
sandbox_home = "/tmp/octra-agent-sandbox"
sandbox_path = "/tmp/octra-agent-bin"
```

`sandbox_home` 会替代真实用户 Home 用于探测、读取和写入；`sandbox_path`
用于查找 `claude`、`codex`、`opencode`，放几个假的可执行文件即可，省略时使用
真实 `PATH`。控制面通过 `GET /api/v1/agents/mode` 返回当前模式，Agents 页面会
显示沙箱横幅；预览操作始终只读。

## 当前已实现

- 独立 Daemon，两个监听端口，统一优雅关闭。
- 认证控制面：健康检查/配置/重载，以及 services、routes、upstreams、targets、consumers、consumer groups、credentials、plugins/configs、LLM providers/models/routing groups/routes/virtual keys/accounts、MCP servers/tools/ACLs/keys 的 CRUD；请求/审计日志、事件与任务可查询。LLM Consumer Policy 会被执行，但还没有控制面 CRUD。
- 常量时间 Bearer Token 认证，Token 随机生成并落盘。
- SQLite + SQLx 迁移，WAL / foreign_keys / busy_timeout；Runtime Snapshot 让路由与鉴权热路径不直接查库（虚拟密钥限额仍会读取持久化用量）。
- API Gateway：Host/Path/Method/Header 路由、最长前缀、轮询/加权/一致性哈希负载均衡、主动/被动健康检查、路径剥离、超时、重试、Key/Basic/JWT/ACL 认证与 Consumer Group、流式代理、访问日志与 JSON/Prometheus 指标。
- LLM Gateway：Canonical IR、OpenAI Chat/Responses、Anthropic、Gemini 请求/响应/SSE 转换、虚拟模型、优先级/权重路由与回退、`order`/`rotate`/`smart`/`usage`/`quota`/`pace`/`intent`/`semantic` 策略、静态或 OAuth Provider Account、Provider query/header 配置、配额/冷却/节流、虚拟密钥哈希存储、Consumer Policy、用量/成本 Rollup、`count_tokens`、可选的 PII 脱敏。
- MCP Gateway：Streamable HTTP 与 stdio 上游、工具发现与聚合、`<server>__<tool>` 命名、`/mcp` JSON-RPC 会话、按 Consumer 的 Tool ACL、API Key、调用审计。
- 插件：Native 与 WASM（API / LLM）运行时、插件自有 SQLite 存储、配置 Schema、按实例的指标与延迟分位数、持久化事件与定时任务，以及共享的限流/预算计数器。
- AI 原生 LLM 控制面（Phase 15，仅 AI-native）：`ai_*` schema、`octra-ai-config` 编译器、`AiStore`、`/api/v1/ai/*` CRUD、validate/preview/apply/revisions/rollback、revision snapshot、事务化 replace、target 级模型元数据（context、capabilities、pricing）、所有 policy 类型的内置执行（rate limit、budget guard、PII、model map、param override、system prompt、request audit、think tags、PromptGuard、PromptCompress、SemanticCache）、基于 AI 原生 description 的 `semantic` target routing（可选 OpenAI-compatible embedding，失败回退本地词法）、Provider `config_json.query` 参数与 protocol presets（Ollama、vLLM、LM Studio、Groq、OpenRouter、DeepSeek、Together、Fireworks、Claude、Google、OpenAI-compatible）、streaming cache 写入与 SSE 回放、AI-native account OAuth 状态与桌面控制、request audit `cache_status` 与 cache/routing 日志、支持 inline/stdin/file JSON body 的完整 CRUD CLI，以及 providers/accounts/models/targets/policies/attachments 全部使用 typed forms 的桌面端 AI Control 页面。legacy 模型表、`/admin/llm/*` 模型端点、`legacy`/`shadow`/`on` 模式和 import-legacy 路径已移除，不考虑向后兼容。
- Agent 集成：探测 Claude Code、Codex、OpenCode，预览并把 Octra Provider 写入其原生配置，支持原子写入与备份恢复。
- CLI：`status`、API Gateway `services`/`routes`/`upstreams`/`targets` CRUD、MCP `servers`/`tools`/`keys`/`acls` CRUD、`reload`、`doctor`、`keys`、`usage`，以及 `ai` status/actions 与 providers/accounts/models/targets/policies/attachments 的完整 CRUD。
- Tauri 2 + React 桌面端，使用统一设计 Token，所有页面直接读取本地控制面；AI Control 使用 typed forms，API Gateway 页面使用 JSON editor CRUD。
- 双 Workspace、CI（Rust fmt/clippy/test；前端 typecheck/test/build）。

## 尚未实现（按路线图推进）

- 桌面端对 MCP 实体的完整 CRUD 与 consumer policy CRUD；API Gateway 的 services/routes/upstreams/targets 已支持桌面 JSON editor 与 CLI CRUD，MCP 的 servers/tools/keys/ACLs 已支持 CLI CRUD。LLM 实体已经通过 AI Control 和 `octra ai` 完整管理。
- 事件总线的投递、重试/退避、定时任务与无订阅者跳过路径的集成测试；存储层保留策略已有测试。
- 更多协议适配器与更多 Agent 适配器/Agent Bridge。
- 有明确产品价值的参考模型补齐：magpie 的 `free`/`drain`/`expiring` 等策略，以及 Kong MCP 的 Consumer Group/默认/CEL ACL 或转换模式。
- 完善 AI 原生 LLM 控制面（ADR 0018）：OpenAI/Anthropic/OpenAI Responses/Gemini 之外的新 wire protocol adapter，以及桌面 webview 中更完善的 OAuth callback 处理。Phase 15 的控制 API、rollback、含 PromptGuard/PromptCompress/SemanticCache 的完整 policy 执行、带可选 embedding 的 `semantic` target routing、Provider query 参数与 protocol presets、streaming cache 写入/SSE 回放、AI-native account OAuth 端点与桌面控制、cache/routing 可观测性、完整 CRUD CLI 与全部 AI 实体的桌面 typed forms 已经存在。legacy 模型表、端点、模式和 import 路径已移除；`ai_*` 是唯一的模型控制面。SemanticCache/Routing 默认使用精确 key 加本地词法相似度。
- OS Keychain Secrets 集成与 OAuth Token 加密。
- 桌面端 Daemon 生命周期管理（启动、停止、健康监督）。
- 跨平台打包、签名与公证。

## 安全边界

- SQLite 中的网关凭据只保存 Secret 引用与 API Key 哈希，不保存明文 Provider Secret。
- Agent 配置备份保存原始文件的完整内容以保证可恢复；这些文件可能包含凭据，因此数据库及其 WAL 边车文件以 `0600` 权限创建，并且每个 Agent 只保留最近 20 份备份。控制面不会返回备份内容。
- 已应用的 Agent 配置按设计包含虚拟密钥明文；控制面之后不会再返回该密钥。
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
