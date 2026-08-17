# Refract

**协议优先的 LLM 聚合 API 网关。** 请求以一种协议入射，经网关折射后以另一种协议出射 —— 四个主流 LLM 协议互为入口与出口，上游渠道按协议而非厂商建模。

[English](./README.md) · 简体中文

## 为什么不是 new-api

new-api 的渠道类型是一个扁平枚举：OpenAI、Anthropic、Gemini、各家云厂商、各种中转站……每接一个新上游就加一个枚举值和一个适配器。渠道类型等于厂商，同一个中转站想同时走两种协议就得建两个渠道，协议转换逻辑散落在转发层的分支里。

Refract 的模型是：**渠道类型 = 协议**。只有五种：

| 类型 | 说明 |
|---|---|
| `chat` | OpenAI Chat Completions（`/v1/chat/completions`） |
| `responses` | OpenAI Responses API（`/v1/responses`） |
| `messages` | Anthropic Messages（`/v1/messages`） |
| `gemini` | Google Gemini（`/v1beta/models/{model}:generateContent`） |
| `aggregate` | 聚合渠道：一个渠道内挂载 1~4 个协议端点 |

聚合渠道能表达 new-api 表达不了的东西：*「我的中转站同时提供 OpenAI 和 Anthropic 两种协议，Anthropic 那条线走另一个域名和另一把 key，claude 系模型应该优先走原生 Anthropic 端点。」*

## 核心特性

- **协议转换**：四协议经统一 IR 互转（中枢辐射，非点对点）。原生请求只读取 `model`/`stream` 路由字段，完整 IR 仅在转换时构造；无别名和参数覆盖时，请求、响应与 SSE 原始字节直通。每个端点单独声明接受哪些协议的转换，未授权的转换直接报错。
- **灵活的地址构造**：每个渠道/端点可开关「非官方地址」（自定义 base URL + 版本前缀 + 路径三段拼接）与「完整地址」（最终 URL 原样使用，不拼接不校验）。
- **端点级配置**：聚合渠道的每个协议端点有独立的地址、密钥、模型集（支持 `别名=上游名` 映射）、协议转换策略和优先顺序。
- **原生优先路由**：全局开关。关闭时路由语义与 new-api 一致（纯优先级）；打开时原生协议端点始终压过转换端点。
- **熔断与健康度**：连续失败的端点按指数退避熔断，尊重上游的 `Retry-After` 头；重启后状态保留，可在界面手动解除。失败阈值与冷却窗口可在设置页运行时调整（阈值 0 关闭熔断）。
- **请求日志**：入站/上游协议、是否转换、命中渠道、重试次数、首字延迟、token 用量全部落库，支持按模型与按密钥聚合（含平均首字/总耗时与生成速率）；请求与响应正文快照默认记录（64KB 截断、流式存聚合文本、可在设置页关闭），日志页可查看完整请求。默认保留 30 天，可调整为 1–3650 天。每个响应都带 `x-refract-request-id` 头，与日志记录一一对应。
- **HTTP 语义透传**：原生成功响应保留上游状态码与端到端响应头，过滤 hop-by-hop headers，流式响应不会泄漏错误的 `Content-Length`。客户端请求头白名单（`anthropic-beta`、`anthropic-version`、`openai-beta`、`x-title`、`http-referer`）在原生调用时透传 —— 转换调用绝不透传。
- **密钥级软配额与速率限制**：每把网关密钥可设 token 总配额（超额在鉴权时被拒）与 RPM/TPM 每分钟限速（超限回 429 并带 `Retry-After`）。
- **模型计价与费用统计**：设置页维护「每百万 token」价表（精确名或前缀通配），请求成本按落库当时的价表固化进日志；仪表盘、按模型、按密钥统计均含费用。
- **调试台**：管理界面内置 Playground，经网关完整管线（路由、转码、熔断、日志）流式对话，不需要先创建网关密钥。
- **无人值守自愈**：连续终态鉴权失败可自动禁用渠道，定时重测在恢复后重新启用；Webhook 对熔断、恢复与自动禁用事件去重告警。
- **异常 200 恢复**：空回复在 3 秒内结束时默认最多重试 5 次，渠道可单独覆盖；还可将纯文本、HTML、未知 JSON/SSE 等非协议标准的 200 转换为明确的 500 错误。
- **渠道账本**：刷新中转站余额，在仪表盘按渠道比较请求量、延迟与费用，并查看小时/天粒度时序。
- **运维探针与指标**：公开的 `/health/live`、`/health/ready` 与 Prometheus 文本格式的 `/metrics`。
- **配置与数据库备份**：一个 JSON 文件导出/导入全部渠道、网关密钥与设置，跨实例迁移后原密钥继续可用；支持合并与替换。设置页还能查看数据库体积并在线生成 SQLite 备份。
- **单二进制部署**：前端编译产物内嵌进二进制，部署只需拷贝一个文件。
- **为单用户设计，不为单用户焊死**：所有业务表预留 `owner_id`，鉴权是 trait，将来加多用户不需要动业务逻辑。

## 快速开始

构建需要 Rust（2024 edition 工具链）与 Node.js（pnpm）：

```sh
# 1. 构建前端（产物在 web/dist，会被内嵌进二进制）
cd web && vp install && vp build && cd ..

# 2. 构建并运行网关
cargo run --release -p refract-server
```

打开 http://127.0.0.1:3939 即可进入管理界面。配置一个渠道后，把客户端的 API 地址指向网关即可：

```sh
curl http://127.0.0.1:3939/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "your-model", "messages": [{"role": "user", "content": "你好"}]}'
```

同一个模型也可以用其他协议的客户端访问（前提是对应端点开启了协议转换）：

```sh
# Anthropic Messages 形状 → 网关 → Chat 上游
curl http://127.0.0.1:3939/v1/messages \
  -H "Content-Type: application/json" \
  -d '{"model": "your-model", "max_tokens": 256, "messages": [{"role": "user", "content": "你好"}]}'
```

也可以用 Docker Compose 启动生产形态：

```sh
cp .env.example .env
# 编辑 .env，替换 REFRACT_ADMIN_TOKEN
docker compose up -d --build
curl --fail http://127.0.0.1:3939/health/ready
```

Compose 默认仅把端口映射到宿主机回环地址，容器内以非 root、只读根文件系统运行，数据库存放在命名卷。完整的备份、升级和恢复步骤见 [`docs/OPERATIONS.md`](./docs/OPERATIONS.md)。

## 网关端点

| 端点 | 协议 |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions |
| `POST /v1/completions` | 旧版 OpenAI Completions / FIM 透传 |
| `POST /v1/responses` | OpenAI Responses |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1beta/models/{model}:generateContent` | Gemini（`...:streamGenerateContent` 为流式） |
| `POST /v1beta/models/{model}:embedContent` · `:batchEmbedContents` | Gemini 原生嵌入（透传） |
| `POST /v1/embeddings` | OpenAI Embeddings（透传） |
| `POST /v1/images/generations` · `/v1/images/edits` | OpenAI Images（透传；edits 为 multipart） |
| `POST /v1/audio/speech` · `/v1/audio/transcriptions` · `/v1/audio/translations` | OpenAI Audio（透传；转写/翻译为 multipart） |
| `POST /v1/moderations` | OpenAI Moderations（透传） |
| `POST /v1/rerank` | Cohere/Jina 形状的重排序（透传） |
| `POST /v1/messages/count_tokens` | Anthropic token 计数（透传到 messages 端点） |
| `POST /v1beta/models/{model}:countTokens` | Gemini token 计数（透传到 gemini 端点） |
| `GET /v1/models` · `/v1/models/{id}` · `/v1beta/models` | OpenAI 与 Gemini 模型发现（由启用渠道派生） |
| `GET /v1/realtime?model=...`（WebSocket） | OpenAI Realtime 原生 WebSocket 桥接 |
| `GET /metrics` | Prometheus 指标（与健康探针一样不鉴权） |

流式与非流式都支持。网关端点带宽松的 CORS 头，浏览器里运行的客户端可以直接跨源调用。管理界面走 `/api/...`，与网关端点使用不同的鉴权体系，并且**不发** CORS 头 —— 管理面只允许同源访问。

嵌入、图像、音频等直通端点没有跨协议转换语义（Anthropic 没有图像 API，Gemini 嵌入形状完全不同），因此只路由到**同协议**的原生端点：把对应模型加进该协议端点的模型列表即可参与路由。别名（multipart 表单里的 `model` 字段同样会被改写）、重试与熔断的行为与对话流量完全一致；`model` 是路由依据，所有直通请求都必须携带。

Realtime 同样只走原生 Chat 端点：先按健康度选路由并应用模型别名、密钥限速和全局并发控制，再原样桥接文本、二进制、ping/pong 与关闭帧，不解析事件。服务端使用 `Authorization: Bearer <网关密钥>`；浏览器可使用与 OpenAI 兼容的 `realtime, openai-insecure-api-key.<网关密钥>` 子协议（也保留 `?key=`）。若渠道使用完整 Chat 地址，地址必须以 `/chat/completions` 或 `/realtime` 结尾，网关不会猜测未知路径。

## 配置

运行目录下的 `refract.toml`（模板见 [`refract.toml.example`](./refract.toml.example)），环境变量 `REFRACT_*` 优先级更高：

| 键 | 默认值 | 说明 |
|---|---|---|
| `listen` | `127.0.0.1:3939` | 监听地址。默认只听本机 |
| `database` | `refract.db` | SQLite 文件路径 |
| `require_auth` | `false` | 调用网关端点是否必须携带网关 API 密钥 |
| `admin_token` | 无 | 启动时设置/轮换管理令牌；推荐通过 `REFRACT_ADMIN_TOKEN` 注入 |
| `upstream_timeout_secs` | `300` | 上游请求整体超时（非流式） |
| `stream_idle_timeout_secs` | `120` | 流式：等待响应头的上限，以及两帧之间的最大间隔 |
| `shutdown_grace_secs` | `30` | 优雅关闭窗口，超时后强制断开存量连接 |
| `proxy` | 无 | 出站代理（http/socks5） |

**安全提示**：网关持有全部上游密钥。服务会拒绝在非回环地址启动，除非管理令牌已经配置且 `require_auth=true`。`REFRACT_ADMIN_TOKEN` 每次启动都会声明式地设置该令牌；不要把明文提交进仓库。

## 开发

一条命令同时拉起前后端，两边都有热重载（需要 [`cargo-watch`](https://github.com/watchexec/cargo-watch)：`cargo install cargo-watch`）：

```sh
pnpm install   # 仅首次
pnpm dev
```

- 后端 `127.0.0.1:3939` —— 改 Rust/SQL 自动重编译重启。
- 前端 `localhost:5173` —— Vite HMR，`/api`、`/v1`、`/v1beta`、`/health`、`/metrics` 已代理到后端。浏览器开这个地址。

提交门禁由 [lefthook](https://lefthook.dev) 承担（`pnpm install` 时自动装进 `.git/hooks`）：每次 `git commit` 会依次跑隐私检查（真实邮箱 / API 密钥 / 本机路径，见 `scripts/privacy-check.sh`）、前后端自动格式化（修复直接回填本次提交）与质量门禁（`vp check` + `clippy -D warnings`）。误报时在该行加 `privacy-allow` 注释标记豁免。

完整回归（钩子不含测试，发版前手动跑）：

```sh
cargo test --workspace --all-targets --all-features --locked
cd web
vp test                       # 前端单元测试
vp build
vp run test:e2e               # E2E：对真实后端二进制跑完整流程
```

架构细节见 [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md)，实现依据见 [`docs/research/FOUNDATIONS.md`](./docs/research/FOUNDATIONS.md)。

## 许可证

[AGPL-3.0-only](./LICENSE)。你可以自由使用和修改它；如果你修改后把它作为网络服务提供给他人，必须以同样的许可证公开你的修改。
