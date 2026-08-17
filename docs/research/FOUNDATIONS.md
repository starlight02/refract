# Refract 技术研究基线

> 调研日期：2026-08-11。本文记录实现依据和已经落地的工程决策；产品架构见 [`../ARCHITECTURE.md`](../ARCHITECTURE.md)。

## 1. 结论

Refract 不把四种协议伪装成同一种 JSON，而是在边界使用独立 codec，在内部使用统一 IR：

```text
caller wire format → protocol codec → UnifiedRequest/UnifiedResponse
                                     ↓
                            router + endpoint policy
                                     ↓
upstream wire format ← protocol codec ← unified IR
```

这样既能实现显式授权的跨协议转换，也能保留协议私有字段。不能无损映射的字段进入带协议前缀的扩展区；未授权转换在发出上游请求前失败。

聚合渠道不是“第五种 LLM wire protocol”，而是四种原生端点的容器。路由候选的最小粒度是 `(channel, endpoint protocol, model)`，因此端点专属 URL、密钥、模型、转换白名单和优先级都能独立生效。

## 2. 四种协议

| 协议 | Refract 入站 | 官方上游形态 | 关键差异 |
| --- | --- | --- | --- |
| OpenAI Chat Completions | `POST /v1/chat/completions` | `POST /chat/completions`（base URL 已含 `/v1`） | `messages[]`；流式返回 Chat chunk SSE；OpenAI 风格 Bearer 鉴权 |
| OpenAI Responses | `POST /v1/responses` | `POST /responses`（base URL 已含 `/v1`） | `input` 可以是文本或 item 数组；工具调用、工具结果、推理是顶层 item；支持 `previous_response_id` 等有状态字段 |
| Anthropic Messages | `POST /v1/messages` | `POST /v1/messages` | `system` 在顶层；`max_tokens` 必填；`x-api-key` 和 `anthropic-version` 请求头；流式 SSE 有命名事件 |
| Google Gemini | `POST /v1beta/models/{model}:generateContent` / `:streamGenerateContent` | 同形路径 | 模型和动作在 URL，不在请求体；`contents[]`、`systemInstruction`；流式使用 `?alt=sse`，以连接结束而非 `[DONE]` 结束 |

实现约束：

1. Gemini codec 不向请求体写 `model`；网关从路径提取模型，并在生成上游 URL 时替换 `{model}` 与 `{action}`。
2. Responses 的状态字段不能凭空转换成其他协议语义；保存在 `responses.*` 扩展区，只在目标仍为 Responses 时还原。
3. Anthropic 的连续同角色消息必须在编码时合并；从其他协议转换时缺失的 `max_tokens` 使用网关明确的兼容默认值。
4. Chat、Responses、Messages、Gemini 的 SSE 分别解析成统一流事件，再编码成调用方协议；不能用文本替换转码。
5. 未识别但合法的顶层字段保存在 `<protocol>.<field>` 扩展区，避免供应商新增参数时被网关静默吞掉。

官方依据：

- [OpenAI — Create chat completion](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)
- [OpenAI — Create a model response](https://developers.openai.com/api/reference/resources/responses/methods/create)
- [Anthropic — Create a Message](https://platform.claude.com/docs/en/api/messages)
- [Google — models.generateContent](https://ai.google.dev/api/generate-content)

## 3. 地址与鉴权模型

官方渠道使用协议默认地址。非官方渠道分两种严格模式：

- **拼接模式**：`base_url + version_prefix + path`，规范化重复 `/`，并校验最终路径是否符合所选协议。
- **完整地址模式**：把配置值当作最终 URL，不再拼接，也不做协议路径形状校验；Gemini 仍可通过 `{model}` / `{action}` 占位符表达动态路径。

聚合端点的地址和密钥为空时继承渠道默认值，非空时覆盖。密钥只在响应中返回掩码，不从管理 API 回显明文。

## 4. 路由基线与 new-api 差异

[new-api](https://github.com/QuantumNous/new-api) 提供了渠道、模型、失败重试、权重路由、协议转换和统计等成熟网关基线，也同时包含用户、计费、充值、权限组等商用功能。Refract 面向个人部署，明确不引入后者。

Refract 的新增路由规则：

1. 先按请求模型寻找所有原生或获准转换的端点。
2. 聚合渠道内，模型重复时按端点显式优先级选原生端点。
3. 全局“原生优先”开启时，原生协议候选先于任何转换候选；关闭时按渠道优先级、权重和健康状态执行普通路由。
4. 只对安全、可重试的上游失败尝试下一个候选；客户端错误和已经开始输出的流不能盲目重试。
5. 熔断状态按 `(channel, endpoint protocol)` 隔离，避免聚合渠道一个坏端点拖垮其他协议。

## 5. 前端技术基线

### Vue Vapor

Vapor 已并入 Vue core 的 vapor 分支；旧 [`vuejs/core-vapor`](https://github.com/vuejs/core-vapor) 仓库只保留历史说明。项目固定到 Vue `3.6.0-rc.3`，通过 `@vitejs/plugin-vue` 的 `features.vapor: true` 全局启用无 Virtual DOM 编译。插件 `6.0.8` 的类型定义明确标注该选项需要 Vue 3.6+。所有产品 SFC 均使用 `<script setup>`，符合插件可强制 Vapor 编译的范围。

### Vite+

项目已按 [Vite+ 迁移规范](https://viteplus.dev/guide/migrate) 收敛工具链：

- `vp dev / build / test / check / fmt / lint` 作为统一入口；
- Vite 由 Vite+ catalog/override 解析，Vitest 从 `vite-plus/test` 导入；
- 包管理器使用 pnpm 11+；
- 验收链为 `vp check`、Vue 类型构建、`vp test`、`vp build`。

依赖安装和包管理遵循 [Vite+ install 指南](https://viteplus.dev/guide/install)。

### Tailwind CSS 4

按 [Tailwind Vite 官方方案](https://tailwindcss.com/docs/installation/using-vite) 使用 `@tailwindcss/vite`，CSS 入口只需 `@import "tailwindcss"`。当前项目解析 `tailwindcss 4.3.3`。

### Reka UI

[Reka UI](https://reka-ui.com/docs/overview/getting-started) 提供无样式、可控/非可控、符合 WAI-ARIA 的 primitives。项目用其 Dialog、Switch 等交互原语，外观由 Refract 的玻璃设计 token 控制；不手写第二套焦点管理、Escape 关闭和 modal layering。当前依赖为 `reka-ui 2.10.x`。

### Liquid Glass Vue

[`@wxperia/liquid-glass-vue`](https://github.com/WXperia/liquid-glass-vue) 提供位移、折射、色差、弹性和鼠标跟踪。项目使用 `1.0.9`：

- 品牌标识使用真实 `LiquidGlass` 组件；
- 大面积表格和表单使用低成本 CSS glass surface，避免每个区域都运行位移滤镜；
- Safari/Firefox 对位移效果只有部分支持，因此 CSS glass 是必要的可用性降级，而不是另一套视觉语言；
- `prefers-reduced-motion` 下关闭非必要位移和过渡，`prefers-reduced-transparency` 与 `prefers-contrast` 提供实色/高对比降级。

## 6. 已落地版本与工程边界

后端工作区使用 Rust 2024 edition、最低 Rust `1.94`，HTTP 服务为 `warp 0.4.3`，异步运行时 `tokio 1.53`，上游客户端 `reqwest 0.13`，存储为 `sqlx 0.9` + SQLite。前端使用 Vue 3.6 RC Vapor、Vite+ `0.2.8`、Vite 8、Tailwind 4、Reka UI 2 和 Liquid Glass Vue 1。

依赖版本以锁文件为可复现事实；“最新”指实施时从官方 registry 解析到的当前版本，而不是无锁的浮动依赖。发布升级必须重新跑后端全工作区测试、Vite+ 检查/测试/构建以及浏览器视觉验收。
