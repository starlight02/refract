# Refract — 架构设计

> LLM 聚合型 API 网关。个人自用，工程质量按生产级要求。

## 0. 命名

**Refract**（折射）。请求以一种协议入射，经网关折射后以另一种协议出射 —— 这正是本项目的核心：协议转换 + 路由。

## 1. 与 new-api 的根本分歧

new-api 的渠道类型是一个**扁平枚举**（OpenAI / Anthropic / Gemini / 各家云厂商 / 各种中转站……几十种），每加一个上游就加一个枚举值 + 一个 adaptor。这导致：

- 渠道类型 = 厂商，而不是协议。同一厂商换协议要新建渠道。
- 一个渠道只能有一个 baseURL、一个 key、一种协议。
- 协议转换逻辑散落在 relay 层的 if/else 里，与路由、计费耦合。

Refract 的模型：**渠道类型 = 协议**，厂商无关。

```
ChannelKind = Chat | Responses | Messages | Gemini | Aggregate
```

前四种是 **单协议渠道**，`Aggregate` 是 **聚合渠道**：一个渠道内挂载 1~4 个协议端点，每个端点有独立的 URL / key / 模型集 / 协议转换策略，并按显式优先级排序。

这个模型能表达 new-api 表达不了的东西：_"我的中转站同时提供 OpenAI 和 Anthropic 两种协议，且 Anthropic 那条线走另一个域名和另一把 key，claude 系模型应该优先走原生 Anthropic 端点。"_

## 2. 领域模型

### 2.1 协议 Protocol

```rust
enum Protocol { Chat, Responses, Messages, Gemini }
```

四个值同时扮演三种角色：

1. **入口协议** — 客户端打进来用的协议（由 HTTP 路径决定）。
2. **渠道原生协议** — 上游端点真正说的协议。
3. **转换目标** — 协议转换开关里勾选的协议。

### 2.2 地址构造 UpstreamAddress

需求 2 的三段式拼接，是一个独立的值对象，与渠道类型正交：

```rust
struct UpstreamAddress {
    unofficial: bool,        // 非官方开关，默认 false
    base_url: Option<String>,
    version_prefix: Option<String>,  // 如 "/v1"
    path: Option<String>,            // 如 "/chat/completions"
    full_address: bool,      // 完整地址开关
}
```

解析规则（`resolve(protocol) -> Url`）：

| unofficial | full_address | 行为                                                                                             |
| ---------- | ------------ | ------------------------------------------------------------------------------------------------ |
| false      | —            | 用协议默认官方地址：base + 协议默认 prefix + 协议默认 path                                       |
| true       | false        | `base_url + version_prefix + path`，三段任一为空则回落到协议默认值；拼接后**校验**路径与协议匹配 |
| true       | true         | 直接用 `base_url` 原样作为完整 URL，**不拼接、不校验**                                           |

`full_address = true` 时仍会替换显式的 `{model}` 与 `{action}` 占位符；未写占位符的地址完全由用户负责，网关不会猜测或改写路径。

### 2.3 协议转换 TranscodePolicy

```rust
struct TranscodePolicy {
    enabled: bool,               // 协议转换开关，默认 false
    accepted: EnumSet<Protocol>, // 勾选的可转换协议
}
```

判定 `can_serve(inbound, native)`：

- `inbound == native` → 永远允许（原生直通）
- `!enabled` → 拒绝
- `enabled && accepted.contains(inbound)` → 允许（需要转换）
- 否则 → 拒绝，返回明确错误（需求 4："直接报错返回"）

### 2.4 渠道 Channel

```rust
struct Channel {
    id, name, enabled, priority, weight, tags,
    kind: ChannelKind,
    credential: Credential,         // 默认 key
    address: UpstreamAddress,       // 默认地址
    endpoints: Vec<ChannelEndpoint>,// 单协议渠道恰好 1 个；聚合渠道 1..=4 个
    model_mapping, param_override, timeout, proxy, ...
}

struct ChannelEndpoint {
    protocol: Protocol,
    order: u16,                     // 需求 5：端点优先顺序
    address: UpstreamAddress,       // 可为空 → 继承渠道默认
    credential: Option<Credential>, // 可为空 → 继承渠道默认
    models: Vec<ModelEntry>,        // 需求 3：每端点独立模型集
    transcode: TranscodePolicy,     // 需求 4：每端点独立转换策略
}
```

`param_override` 是显式两段结构：`common` 合并进所有端点请求体，`protocols.<id>` 只在对应协议端点展开。值为 `null` 表示删除该字段。旧扁平对象（协议名键 + 对象值当分组）读入时仍会映射到新结构。

**关键设计：单协议渠道也用 `endpoints` 表达**（长度恒为 1）。这样路由层只认 `(Channel, ChannelEndpoint)` 二元组，不需要分支处理两种渠道形态 —— 聚合与非聚合的差异被压缩到构造期，运行时完全统一。

### 2.5 路由候选 RouteCandidate

路由的原子单位不是渠道，而是**端点**：

```rust
struct RouteCandidate<'a> {
    channel: &'a Channel,
    endpoint: &'a ChannelEndpoint,
    upstream_model: &'a str,   // 经 model_mapping 映射后的上游模型名
    native: bool,              // endpoint.protocol == inbound_protocol
}
```

## 3. 路由算法

输入：`inbound: Protocol`、`model: &str`、`native_first: bool`（需求 6 的全局开关）。

```
1. 收集候选：遍历启用渠道的所有端点，端点模型集含 model（或映射命中）
   且 endpoint.transcode.can_serve(inbound, endpoint.protocol) 通过。
2. 渠道内去重（需求 5）：同一渠道内若多个端点都能服务该模型，
   只保留 order 最小的那个（原生端点应被配置为 order 最小）。
   —— 精确规则：先按 (native desc, order asc) 排序取首个。
3. 全局排序分层：
   native_first = true  → 分层键 = (native desc, channel.priority desc)
   native_first = false → 分层键 = (channel.priority desc)      [new-api 语义]
4. 取最高层的所有候选，按 channel.weight 加权随机选一个。
5. 失败重试：从剩余候选中按同样规则再取，最多 `max_attempts` 次（含首次）；
   默认同一渠道只贡献一个候选，打开 `retry_same_channel` 后才允许尝试其其他端点。
```

`native_first` 的语义差异是精确的：关闭时**优先级完全由 priority 决定**，原生与否只在同优先级内作为次级排序键；打开时**原生性成为最高位排序键**，一个 priority=0 的原生端点会压过 priority=100 的转换端点。

熔断与路由的交互：处于熔断中的端点在执行阶段被降到候选序列**末尾**（而不是从规划里剔除）——健康端点全部失败时，熔断端点仍是最后的退路。熔断判定读进程内缓存（写穿到 SQLite），不在热路径上碰数据库。熔断参数（阈值/冷却窗口）存 settings 表，可在运行时经管理 API 热更新。

## 4. 协议转换架构

### 4.1 中枢 IR（Intermediate Representation）

不做 4×4=12 对点对点转换器（组合爆炸，且新增协议是 O(n²) 工作量）。采用**中枢辐射模型**：

```
Chat ─┐                    ┌─ Chat
Res  ─┤→ decode → [IR] → encode ├─ Res
Msg  ─┤                    ├─ Msg
Gem  ─┘                    └─ Gemini
```

每个协议实现两个 trait：

```rust
trait RequestCodec {
    fn decode(&self, raw: &RawJson) -> Result<UnifiedRequest>;
    fn encode(&self, ir: &UnifiedRequest) -> Result<RawJson>;
}
trait ResponseCodec {
    fn decode(&self, raw: &RawJson) -> Result<UnifiedResponse>;
    fn encode(&self, ir: &UnifiedResponse) -> Result<RawJson>;
}
trait StreamCodec {
    fn decode(&self) -> Box<dyn StreamDecoder>;  // SSE bytes → Vec<StreamEvent>
    fn encode(&self) -> Box<dyn StreamEncoder>;  // StreamEvent → SSE bytes
}
```

新增协议 = 实现 3 个 trait = O(n)。

**直通优化（critical）**：HTTP 入口只反序列化顶层 `model` 与 `stream` 路由字段。原生候选无模型别名和参数覆盖时，跳过完整 IR，请求/响应/SSE 字节原样透传；成功状态码与端到端响应头一并保留，hop-by-hop headers 与流式 `Content-Length` 被过滤。只有协议转换时才构造完整 IR，原生别名/参数覆盖只改写顶层 JSON 并保留未知字段。

### 4.2 统一 IR 设计要点

IR 必须是四种协议的**并集**而非交集，否则转换必然有损。无法映射的字段进 `extensions: Map<String, Value>`，由目标编码器决定丢弃还是尽力还原。已知例外：Anthropic `cache_control` 只在 messages→messages 用原文 extension 保真，跨协议会丢失 block 级缓存断点；转码路径不保证 `logprobs`（Responses 流式编码器会写空数组）。

```rust
struct UnifiedRequest {
    model: String,
    messages: Vec<Message>,        // 含 system 归一化
    system: Option<Vec<ContentPart>>, // Anthropic/Gemini 的独立 system
    tools: Vec<ToolDef>,
    tool_choice: ToolChoice,
    sampling: Sampling,            // temperature/top_p/top_k/penalties/stop/seed
    max_output_tokens: Option<u32>,
    stream: bool,
    reasoning: Option<ReasoningConfig>,
    response_format: Option<ResponseFormat>,
    metadata: Metadata,
    extensions: BTreeMap<String, Value>,
}

enum ContentPart {
    Text(String),
    Image { source: MediaSource, mime: Option<String> },
    Audio { source: MediaSource, mime: Option<String> },
    File  { source: MediaSource, mime: Option<String>, name: Option<String> },
    Thinking { text: String, signature: Option<String> },  // 保留 Anthropic signature
    RedactedThinking { data: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { id: String, content: Vec<ContentPart>, is_error: bool },
}

enum MediaSource { Url(String), Base64(String), FileId(String) }
```

`Thinking.signature` 必须保留：Anthropic 多轮工具调用时若丢失 signature，上游会拒绝请求。转到其他协议时它进 extensions，转回来时还原。

### 4.3 流式转换

流式是最难的部分。统一事件模型：

```rust
enum StreamEvent {
    Start { id, model, role },
    ContentStart { index, kind: PartKind },
    TextDelta { index, text },
    ThinkingDelta { index, text },
    ThinkingSignature { index, signature },
    ToolCallStart { index, id, name },
    ToolCallArgsDelta { index, json_fragment },
    ContentStop { index },
    Usage(Usage),
    Stop { reason: StopReason },
    Done,
    Error(ProtocolError),
}
```

各协议的编码器是**状态机**，因为：

- Anthropic 要求严格的 `message_start → content_block_start → deltas → content_block_stop → message_delta → message_stop` 序列，且 `content_block` 有 index。
- Responses API 要求 `response.created → response.output_item.added → response.content_part.added → deltas → ... → response.completed`，且每个事件带递增 `sequence_number`。
- OpenAI Chat 的 tool_calls delta 用 `index` 累积，首帧带 `id`/`name`，后续只带 `arguments` 片段。
- Gemini 每个 chunk 是完整的 `candidates[]` 结构。

所以编码器持有 `EncoderState`，负责补齐目标协议要求的仪式性事件。

### 4.4 直通端点（Passthrough）

嵌入、图像、音频、审核、重排序与 token 计数没有跨协议转换语义 —— Anthropic
没有图像 API，Gemini 的嵌入形状与 OpenAI 完全不同。为它们发明「统一 IR」
只会制造有损转换，所以这类端点走独立的直通管线：

- `Action::Passthrough(PassKind)` 声明端点种类；每个 `PassKind` 自带协议归属
  （`count_tokens` 挂 Messages、`:countTokens` 挂 Gemini、其余挂 Chat）、
  默认路径与地址校验后缀。
- 路由复用同一个 planner，但候选被过滤为**入口协议的原生端点** —— 直通
  永远不转码。
- 请求与响应字节原样往返。唯一的改写是模型别名与（JSON 体的）参数覆盖；
  multipart 表单（音频转写、图像编辑）按 RFC 7578 的结构只替换 `model`
  字段的值，boundary 与文件字节原封不动。
- 重试、熔断、健康记录、密钥治理（白名单/配额/限速）与日志和对话流量
  完全一致；`model` 字段是路由依据，因此对直通请求是必填的。

### 4.5 Realtime WebSocket

`GET /v1/realtime?model=...` 是独立的原生 Chat 桥接路径。HTTP 升级前完成网关
鉴权、模型白名单、RPM/全局并发准入、渠道权限、路由和熔断排序；升级后再用
渠道凭据连接上游，并把文本、二进制、ping/pong 与 close 帧双向转发。并发
permit 持有到整个会话结束，连接时长和异常状态写入统一请求日志。

Realtime 事件是有状态会话协议，不进入对话 IR，也不做跨协议转换。地址由同一
Chat 端点的 `{base}{prefix}` 推导为 `/realtime`；完整地址只接受明确的
`/chat/completions` 或 `/realtime` 后缀，模型别名通过 URL query builder 编码。
Realtime 入口只走 HTTP/1.1（明文）或 HTTPS 的 Upgrade。xitca 的 HTTP/2 栈支持
RFC 8441 extended CONNECT，但 WebSocket 桥接不在 h2/h3 上传输；h3 dispatcher
明确禁止 upgrade/expect 服务。

## 5. Crate 划分

```
refract-core      领域模型、错误、ID、时间。无 IO 依赖。进程配置由 refract-server 用 figment 加载。
refract-protocol  IR + 四协议 codec + 流式转码器。依赖 core。纯函数，易测。
refract-store     SQLite 持久化 + 具体仓储类型（ChannelRepo 等）。依赖 core。
refract-upstream  HTTP 客户端、地址解析、凭据注入、SSE 解析。依赖 core+protocol。
refract-router    候选收集、排序、加权随机、重试、熔断、健康度、执行器。依赖 core+protocol+store+upstream。
refract-api       xitca-web：网关端点 + 管理 REST。依赖全部。
refract-server    二进制：配置加载、装配、嵌入前端、优雅关闭。
```

依赖方向严格单向，无循环。`refract-protocol` 不依赖 store/upstream，可以脱离 IO 做纯单元测试 —— 这是协议转换正确性的保证。

## 6. 多用户可扩展性（需求 7）

个人自用不做用户系统，但**不能把"单用户"焊死在数据模型里**。做法：

- 所有业务表**预留 `owner_id INTEGER NOT NULL DEFAULT 1`** 列，当前恒为 1。
- 网关鉴权抽象为 `Authenticator`，产出 `Principal { owner_id, scopes, api_key }`。当前 `SingleUserAuthenticator` 校验网关 API key；管理令牌是独立的管理面凭据，避免推理密钥获得配置权限。未来可替换认证器而不改 Warp 路由与授权判断。
- 仓储层所有查询方法**都带 `owner_id` 参数**，当前传常量。
- 配额字段在 schema 中就位（`quota`, `used_quota`），鉴权时强制检查，成功请求按实际用量累计。`auth.admin_username` 会在 bootstrap 写入，当前会话响应仍硬编码 `admin@localhost`，该键尚未参与鉴权。

这样加多用户 = 加一张 users 表 + 换一个 Authenticator 实现 + 放开 owner_id 来源，不动业务逻辑。

## 6.1 安全与凭据治理

- **凭据静态加密**：渠道密钥支持在持久化层以 AES-256-GCM 加密落库（格式为 `refract.v1.` 前缀 + base64(随机 12 字节 nonce || 密文+tag)）。主密钥可通过 `REFRACT_MASTER_KEY` 环境变量或设置表配置；历史明文与无密钥模式完全兼容透传。
- **管理面防爆破**：内存维护客户端 IP 连续失败计数，连续 5 次失败自动锁定 60 秒（返回 403 与 `Retry-After`），成功即清零。
- **网关单 IP 速率限制**：在各协议入口前按客户端 IP 维持自然分钟窗口，支持独立的 RPM 上限约束。
- **单请求上游调用总预算**：`RoutingPolicy` 配置 `max_upstream_calls`，由执行器在所有重试、密钥轮换与空回复重发点统一计数，防止请求产生无界扇出。
- **数据库与备份权限收紧**：新创建的 SQLite 库文件与目录强制设为 `0600`/`0700` 权限；定时自动备份与手动备份产物统一受此约束。
- **Webhook HMAC 签名**：配置 `notify.webhook_secret` 时，所有告警推送请求附带 `X-Refract-Signature: sha256=<hex>` 标头。管理面 JSON 写操作走信封加密；Playground 为消费 SSE 走原始字节，不经该信封。

## 7. 前端

前端是同一 pnpm + Vite Plus workspace 中的两个独立应用，不共享框架运行时：

```text
apps/admin       Vue 管理后台；构建产物嵌入 refract-server
apps/homepage    React 产品首页；构建为独立静态站点
packages/contracts
                 管理 API 类型与协议元数据；两个应用共同依赖
```

边界原则：

- `admin` 是管理面客户端，与 `/api/*` 同源部署；它使用管理令牌，不承载公开站点会话。
- `homepage` 是公开展示面，独立发布到静态托管/CDN；它不进入网关二进制，也不直接依赖管理 API。
- `contracts` 只保存线上的 JSON 类型和稳定协议元数据，不放 UI 组件、状态管理或 HTTP transport。
- 两个应用可以使用不同 UI 框架；共享发生在协议和设计 token 层，不做跨 React/Vue 组件封装。
- 根目录只有一个 `pnpm-lock.yaml`，Vite Plus 通过 `vp run -r <task>` 执行 workspace 任务。

管理后台：

- Vue 3.6 RC + **Vapor Mode**（`@vitejs/plugin-vue` 全局启用）
- Vite Plus + Tailwind CSS v4.3（`@tailwindcss/vite`）
- reka-ui 2.10（无样式可访问性原语）
- 状态：Pinia 4，路由：vue-router 5

公开首页：

- React 19 + TanStack Router
- Vite Plus + Tailwind CSS v4.3
- 静态构建产物独立发布；Release 同时提供 homepage 压缩包

## 8. 测试策略

| 层       | 方式                                                                          |
| -------- | ----------------------------------------------------------------------------- |
| protocol | 纯单元测试 + `insta` 快照，四协议两两互转的黄金样例                           |
| router   | 表驱动测试，覆盖 native_first 开关、优先级分层、加权随机（固定 seed）         |
| store    | 内存 SQLite，仓储 CRUD + 迁移                                                 |
| upstream | `wiremock` 假上游，覆盖地址解析矩阵、SSE 解析、超时重试                       |
| api      | 进程内 `TestRequest`，端到端 handler 测试                                      |
| 前端     | Vite+ check/build、Vitest、真实 Rust 服务上的 Playwright E2E + 多视口视觉验收 |
