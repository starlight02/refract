import type { ProtocolId } from './protocols'

export type Locale = 'zh' | 'en'

export const copy = {
  zh: {
    metaTitle: 'Refract — 协议优先的 LLM 网关',
    metaDescription:
      '协议优先的 LLM 聚合 API 网关。请求以一种协议入射，经网关折射后以另一种协议出射。',
    nav: {
      why: '对照',
      protocols: '协议',
      features: '能力',
      architecture: '架构',
      github: 'GitHub',
      signIn: '登录',
      signOut: '退出',
      menu: '菜单',
      close: '关闭',
    },
    hero: {
      kicker: '协议优先  ·  本地优先',
      titleA: '入射一种协议',
      titleB: '出射另一种。',
      lede: 'Refract 是协议优先的 LLM 聚合 API 网关。四个主流协议互为入口与出口，上游渠道按协议建模，而不是按厂商。',
      cta: '阅读架构',
      secondary: 'GitHub',
      chips: ['AGPL-3.0', '单二进制', 'SQLite', 'Rust'],
    },
    prism: {
      inbound: '入站协议',
      outbound: '出站协议',
      native: '原生直通 · 字节不变',
      transcoded: '经 IR 折射',
      ir: 'IR',
      irFull: '统一中间表示',
      passthrough: '直通',
      transcode: '折射',
      pickIn: '选择入站',
      pickOut: '选择出站',
    },
    kinds: {
      kicker: '01  —  模型',
      title: '渠道类型就是协议。一共五种。',
      lede: 'new-api 把渠道做成厂商枚举：每接入一个上游就多一个类型、多一套适配。Refract 只认协议。',
      items: {
        chat: { name: 'Chat', meaning: 'OpenAI Chat Completions' },
        responses: { name: 'Responses', meaning: 'OpenAI Responses API' },
        messages: { name: 'Messages', meaning: 'Anthropic Messages' },
        gemini: { name: 'Gemini', meaning: 'Google Gemini generateContent' },
        aggregate: {
          name: 'Aggregate',
          meaning: '一个渠道挂载 1–4 个协议端点',
        },
      },
      quote:
        '我的中转同时提供 OpenAI 与 Anthropic；Anthropic 走另一域名、另一把密钥；claude 模型永远先打原生端点。',
      quoteLead: '聚合渠道能表达 new-api 做不到的事：',
    },
    why: {
      kicker: '02  —  对照',
      title: '不要再为每个厂商写适配器。',
      lede: '渠道类型等于厂商时，双协议中转必须拆成两条渠道，协议翻译散落在 if/else 里。',
      leftTitle: 'new-api',
      rightTitle: 'Refract',
      left: [
        '渠道类型 = 厂商',
        '每个上游一个枚举、一套适配',
        '协议翻译散落在中继层的分支里',
        '双协议中转必须建两个渠道',
        '点对点转换，边数随协议平方增长',
      ],
      right: [
        '渠道类型 = 协议',
        '五种 kind，就是全部目录',
        '四协议经统一 IR 互转',
        '一个聚合渠道，多个协议端点',
        '中枢辐射：8 个适配器，不是 12 条边',
      ],
    },
    features: {
      kicker: '03  —  能力',
      title: '网关该有的，都在二进制里。',
      lede: '路由、熔断、日志、配额、计价、备份。管理界面内嵌，部署只需拷贝一个文件。',
      items: [
        {
          n: '01',
          title: '协议转换',
          body: '四协议经统一 IR 互转（中枢辐射，非点对点）。原生请求只读取 model / stream 路由字段；无别名和参数覆盖时，请求、响应与 SSE 原始字节直通。',
        },
        {
          n: '02',
          title: '原生优先路由',
          body: '全局开关。关闭时与 new-api 一致（纯优先级）；打开时原生协议端点始终压过转换端点。每个端点单独声明接受哪些入站协议。',
        },
        {
          n: '03',
          title: '熔断与健康度',
          body: '连续失败按指数退避熔断，尊重上游 Retry-After。状态跨重启保留，阈值与冷却窗口可在设置页运行时调整。',
        },
        {
          n: '04',
          title: '请求日志',
          body: '入站/上游协议、是否转换、命中渠道、重试、TTFB、token 全部落库。按模型、按密钥聚合；正文快照默认记录。每个响应带 x-refract-request-id。',
        },
        {
          n: '05',
          title: 'HTTP 语义透传',
          body: '原生成功保留上游状态码与端到端响应头。客户端头白名单（anthropic-beta、openai-beta 等）只在原生调用时转发——转换调用绝不透传。',
        },
        {
          n: '06',
          title: '配额、限速与计价',
          body: '每把网关密钥可设 token 总配额与 RPM/TPM。设置页维护每百万 token 价表，成本按落库当时的价表固化进日志。',
        },
        {
          n: '07',
          title: '无人值守自愈',
          body: '终态鉴权失败可自动禁用渠道，定时重测在恢复后重新启用。Webhook 对熔断、恢复与自动禁用事件去重告警。',
        },
        {
          n: '08',
          title: '异常 200 恢复',
          body: '空回复在短时间内结束时默认重试；还可将纯文本、HTML、未知 JSON/SSE 等非协议标准的 200 转为明确的 500。',
        },
      ],
    },
    architecture: {
      kicker: '04  —  架构',
      title: '所有协议只与 IR 对话。',
      lede: '中枢辐射，而不是点对点。每个协议只负责编解码自己的形状，互转成本固定为四进四出。',
      inbound: '入站解码',
      outbound: '出站编码',
      nativeNote: '同协议：不构造完整 IR，字节直通。',
      transcodeNote: '跨协议：解码 → IR → 编码，端点显式授权。',
      spokes: [
        { id: 'chat' as const, label: 'Chat Completions' },
        { id: 'responses' as const, label: 'Responses' },
        { id: 'messages' as const, label: 'Messages' },
        { id: 'gemini' as const, label: 'Gemini' },
      ],
    },
    endpoints: {
      kicker: '05  —  端点',
      title: '同一模型，四种说法。',
      lede: '配置渠道之后，把客户端指向网关。入站形状由你选，出站形状由渠道决定。',
      copy: '复制',
      copied: '已复制',
      via: '经网关折射为',
    },
    footer: {
      mark: 'Refract',
      line: '请求以一种协议入射，经网关折射后以另一种协议出射。',
      license: 'AGPL-3.0',
      source: '源码',
      architecture: '架构',
      operations: '运维',
      copyright: 'starlight02',
    },
    login: {
      kicker: '控制台',
      title: '登录 Refract',
      lede: '使用你的账户继续。登录后返回首页。',
      disabled: '登录已关闭。',
      back: '返回首页',
      continueWith: '使用',
    },
    error: {
      title: '出了点问题',
      fallback: '发生了意外错误。请尝试刷新页面。',
    },
  },
  en: {
    metaTitle: 'Refract — Protocol-first LLM gateway',
    metaDescription:
      'A protocol-first LLM aggregation API gateway. A request enters as one protocol and leaves as another.',
    nav: {
      why: 'Contrast',
      protocols: 'Protocols',
      features: 'Capabilities',
      architecture: 'Architecture',
      github: 'GitHub',
      signIn: 'Sign in',
      signOut: 'Sign out',
      menu: 'Menu',
      close: 'Close',
    },
    hero: {
      kicker: 'Protocol-first  ·  Local-first',
      titleA: 'Enter as one protocol.',
      titleB: 'Leave as another.',
      lede: 'Refract is a protocol-first LLM aggregation API gateway. Four mainstream protocols work interchangeably as both entry and exit. Upstream channels are modeled by protocol, not by vendor.',
      cta: 'Read the architecture',
      secondary: 'GitHub',
      chips: ['AGPL-3.0', 'Single binary', 'SQLite', 'Rust'],
    },
    prism: {
      inbound: 'Inbound',
      outbound: 'Outbound',
      native: 'Native passthrough · bytes unchanged',
      transcoded: 'Refracted through IR',
      ir: 'IR',
      irFull: 'Unified intermediate representation',
      passthrough: 'passthrough',
      transcode: 'transcode',
      pickIn: 'Choose inbound',
      pickOut: 'Choose outbound',
    },
    kinds: {
      kicker: '01  —  Model',
      title: 'Channel type is protocol. There are five.',
      lede: 'new-api models channel types as a vendor enum: each upstream adds a type and an adapter. Refract only models protocols.',
      items: {
        chat: { name: 'Chat', meaning: 'OpenAI Chat Completions' },
        responses: { name: 'Responses', meaning: 'OpenAI Responses API' },
        messages: { name: 'Messages', meaning: 'Anthropic Messages' },
        gemini: { name: 'Gemini', meaning: 'Google Gemini generateContent' },
        aggregate: {
          name: 'Aggregate',
          meaning: 'One channel, one to four protocol endpoints',
        },
      },
      quote:
        'My relay speaks both OpenAI and Anthropic; the Anthropic line uses a different domain and key; claude models should always hit the native endpoint first.',
      quoteLead: 'Aggregate channels can say things new-api cannot:',
    },
    why: {
      kicker: '02  —  Contrast',
      title: 'Stop adapting vendors. Adapt protocols.',
      lede: 'When channel type equals vendor, a dual-protocol relay splits into two channels, and translation logic scatters across if/else branches.',
      leftTitle: 'new-api',
      rightTitle: 'Refract',
      left: [
        'Channel type = vendor',
        'One enum value and adapter per upstream',
        'Translation scattered through the relay layer',
        'A dual-protocol relay needs two channels',
        'Pairwise conversion, edges grow with the square',
      ],
      right: [
        'Channel type = protocol',
        'Five kinds. That is the whole catalog.',
        'Four protocols convert through one IR',
        'One aggregate channel, many endpoints',
        'Hub-and-spoke: eight adapters, not twelve edges',
      ],
    },
    features: {
      kicker: '03  —  Capabilities',
      title: 'The gateway is the binary.',
      lede: 'Routing, breakers, logs, quotas, pricing, backups. Admin UI is embedded. Deploying is copying one file.',
      items: [
        {
          n: '01',
          title: 'Protocol transcoding',
          body: 'All four protocols convert through a unified IR (hub-and-spoke, not pairwise). Native requests only decode model / stream routing fields. With no alias or override, request, response, and SSE bytes pass through unchanged.',
        },
        {
          n: '02',
          title: 'Native-first routing',
          body: 'A global switch. Off: routing matches new-api (pure priority). On: native protocol endpoints always outrank transcoded ones. Each endpoint declares the inbound protocols it accepts.',
        },
        {
          n: '03',
          title: 'Circuit breaking',
          body: 'Consecutive failures suspend an endpoint with exponential backoff. Upstream Retry-After is honored. State survives restarts; thresholds are live-configurable, including disable.',
        },
        {
          n: '04',
          title: 'Request logs',
          body: 'Inbound and upstream protocol, transcoding, chosen channel, retries, TTFB, and tokens are persisted. Per-model and per-key aggregation; body snapshots on by default. Every response carries x-refract-request-id.',
        },
        {
          n: '05',
          title: 'HTTP semantic passthrough',
          body: 'Native successes keep upstream status and end-to-end headers. A client-header whitelist (anthropic-beta, openai-beta, and kin) is forwarded on native calls — never on transcoded ones.',
        },
        {
          n: '06',
          title: 'Quotas, rate limits, spend',
          body: 'Each gateway key can carry a token quota plus RPM/TPM. A per-million-token price table in Settings freezes cost into the log row at write time.',
        },
        {
          n: '07',
          title: 'Unattended recovery',
          body: 'Terminal authentication failures can auto-disable a channel; periodic retests bring it back. Deduplicated webhooks report suspension, recovery, and auto-disable.',
        },
        {
          n: '08',
          title: 'Abnormal-200 handling',
          body: 'Empty 200s that finish too fast are retried. Plain text, HTML, and unknown JSON/SSE can be lifted into explicit HTTP 500s instead of silent success.',
        },
      ],
    },
    architecture: {
      kicker: '04  —  Architecture',
      title: 'Every protocol speaks only to the IR.',
      lede: 'Hub-and-spoke, not pairwise. Each protocol encodes and decodes its own shape. Cross-protocol cost is fixed: four in, four out.',
      inbound: 'Decode inbound',
      outbound: 'Encode outbound',
      nativeNote: 'Same protocol: skip the full IR. Bytes pass through.',
      transcodeNote: 'Cross protocol: decode → IR → encode, with explicit endpoint consent.',
      spokes: [
        { id: 'chat' as const, label: 'Chat Completions' },
        { id: 'responses' as const, label: 'Responses' },
        { id: 'messages' as const, label: 'Messages' },
        { id: 'gemini' as const, label: 'Gemini' },
      ],
    },
    endpoints: {
      kicker: '05  —  Endpoints',
      title: 'One model, four shapes.',
      lede: 'Configure a channel, then point clients at the gateway. You choose the inbound shape; the channel chooses the outbound.',
      copy: 'Copy',
      copied: 'Copied',
      via: 'refracted by the gateway into',
    },
    footer: {
      mark: 'Refract',
      line: 'A request enters as one protocol and leaves as another.',
      license: 'AGPL-3.0',
      source: 'Source',
      architecture: 'Architecture',
      operations: 'Operations',
      copyright: 'starlight02',
    },
    login: {
      kicker: 'Console',
      title: 'Sign in to Refract',
      lede: 'Continue with your account. You will return to the homepage.',
      disabled: 'Sign-in is disabled.',
      back: 'Back to homepage',
      continueWith: 'Continue with',
    },
    error: {
      title: 'Something went wrong',
      fallback: 'An unexpected error occurred. Try reloading the page.',
    },
  },
} as const

export type Copy = (typeof copy)[Locale]

export const CURL: Record<ProtocolId, string> = {
  chat: `curl http://127.0.0.1:3939/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "your-model",
    "messages": [
      {"role": "user", "content": "hello"}
    ]
  }'`,
  responses: `curl http://127.0.0.1:3939/v1/responses \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "your-model",
    "input": "hello"
  }'`,
  messages: `curl http://127.0.0.1:3939/v1/messages \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "your-model",
    "max_tokens": 256,
    "messages": [
      {"role": "user", "content": "hello"}
    ]
  }'`,
  gemini: `curl http://127.0.0.1:3939/v1beta/models/your-model:generateContent \\
  -H "Content-Type: application/json" \\
  -d '{
    "contents": [
      {"role": "user", "parts": [{"text": "hello"}]}
    ]
  }'`,
}
