import http from 'node:http'
import net from 'node:net'

import { expect, test } from '@playwright/test'

/**
 * 端到端测试：对着真实的生产形态跑。
 *
 * - 服务端是 `refract-server` 单二进制（内嵌前端 + SQLite），由
 *   `e2e/serve.sh` 在 4539 端口拉起，数据库是一次性临时文件。
 * - 「上游」是本测试进程里的一个 node http 服务，形状照抄 OpenAI
 *   Chat Completions。网关必须真的把请求打到它、并把它的响应送回浏览器。
 *
 * 用例之间有状态依赖（先建渠道才能路由），因此整个文件串行执行。
 */
test.describe.configure({ mode: 'serial' })

/** 固定应答的上游。 */
const UPSTREAM_ANSWER = {
  id: 'chatcmpl-e2e',
  model: 'gpt-4o',
  choices: [
    {
      index: 0,
      message: { role: 'assistant', content: 'e2e-ok' },
      finish_reason: 'stop',
    },
  ],
  usage: { prompt_tokens: 5, completion_tokens: 2, total_tokens: 7 },
}

/** 记录上游收到的最后一个请求，供断言「网关到底发了什么」。 */
interface SeenRequest {
  path: string
  headers: http.IncomingHttpHeaders
  body: Record<string, unknown>
}

let upstream: http.Server
let upstreamUrl = ''
let lastSeen: SeenRequest | null = null

/** 找一个确定空闲的端口：先绑定再关闭，随即把端口让给「死上游」用。 */
function findDeadPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = net.createServer()
    probe.listen(0, '127.0.0.1', () => {
      const address = probe.address()
      if (typeof address !== 'object' || address === null) {
        reject(new Error('failed to probe a free port'))
        return
      }
      const { port } = address
      probe.close(() => resolve(port))
    })
  })
}

/** OpenAI Embeddings 形状的固定应答。 */
const UPSTREAM_EMBEDDINGS_ANSWER = {
  object: 'list',
  data: [{ object: 'embedding', index: 0, embedding: [0.1, 0.2, 0.3] }],
  model: 'gpt-4o',
  usage: { prompt_tokens: 3, total_tokens: 3 },
}

test.beforeAll(async () => {
  upstream = http.createServer((req, res) => {
    let raw = ''
    req.on('data', (chunk: Buffer) => {
      raw += chunk.toString()
    })
    req.on('end', () => {
      let body: Record<string, unknown> = {}
      try {
        body = raw ? JSON.parse(raw) : {}
      } catch {
        // 非 JSON 请求体保持空对象，断言会自然失败。
      }
      lastSeen = { path: req.url ?? '', headers: req.headers, body }
      if (raw.includes('nonstandard-200-e2e')) {
        res.writeHead(200, { 'content-type': 'text/plain' })
        res.end('upstream returned a maintenance page')
        return
      }
      // 流式请求回 SSE —— 调试台等流式客户端要吃增量帧。
      if (body.stream === true) {
        res.writeHead(200, { 'content-type': 'text/event-stream' })
        res.write(
          'data: {"id":"chatcmpl-e2e","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"e2e-"},"finish_reason":null}]}\n\n',
        )
        res.write(
          'data: {"id":"chatcmpl-e2e","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}\n\n',
        )
        res.write('data: [DONE]\n\n')
        res.end()
        return
      }
      res.writeHead(200, { 'content-type': 'application/json' })
      const answer = req.url?.endsWith('/embeddings') ? UPSTREAM_EMBEDDINGS_ANSWER : UPSTREAM_ANSWER
      res.end(JSON.stringify(answer))
    })
  })
  await new Promise<void>((resolve) => upstream.listen(0, '127.0.0.1', resolve))
  const address = upstream.address()
  if (typeof address !== 'object' || address === null) {
    throw new Error('fake upstream failed to start')
  }
  upstreamUrl = `http://127.0.0.1:${address.port}`
})

test.afterAll(async () => {
  await new Promise<void>((resolve, reject) =>
    upstream.close((error) => (error ? reject(error) : resolve())),
  )
})

/** 在页面上下文里对网关发请求（同源，免 CORS）。 */
async function gatewayFetch(
  page: import('@playwright/test').Page,
  base: string,
  path: string,
  body: Record<string, unknown>,
): Promise<{ status: number; body: Record<string, unknown> }> {
  // about:blank 是 opaque origin：从它发 fetch 会被 CORS 挡住。
  // 先把页面挪到网关上，同源请求就不需要任何 CORS 配合。
  if (new URL(page.url()).origin === 'null') {
    await page.goto('/')
  }
  return page.evaluate(
    async ([url, b]) => {
      const response = await fetch(url as string, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(b),
      })
      const text = await response.text()
      let parsed: Record<string, unknown> = {}
      try {
        parsed = text ? JSON.parse(text) : {}
      } catch {
        parsed = { raw: text }
      }
      return { status: response.status, body: parsed }
    },
    [`${base}${path}`, body] as const,
  )
}

test('仪表盘能打开并展示指标卡', async ({ page }) => {
  await page.goto('/')
  await expect(page.getByRole('heading', { name: '仪表盘' })).toBeVisible()
  await expect(page.getByText('请求总数')).toBeVisible()
  await expect(page.getByText('平均延迟')).toBeVisible()
})

test('通过编辑器创建渠道', async ({ page }) => {
  await page.goto('/channels')
  await page.getByRole('button', { name: '新建渠道' }).click()
  await expect(page.getByRole('heading', { name: '新建渠道' })).toBeVisible()

  // 必填项缺失时不允许保存。
  await expect(page.getByRole('button', { name: '创建渠道' })).toBeDisabled()

  await page.getByPlaceholder('例如：中转站-主力').fill('E2E 主渠道')
  await page.getByRole('switch', { name: '非官方地址' }).click()
  await page.getByPlaceholder('https://api.example.com').fill(upstreamUrl)
  // 钥匙池 textarea：统一密钥入口，一行一把。
  await page.getByRole('textbox', { name: '上游钥匙池，每行一把' }).fill('sk-e2e-main')

  const modelInput = page.getByRole('textbox', { name: 'Chat 模型输入' })
  await modelInput.fill('gpt-4o')
  await modelInput.press('Enter')
  await expect(page.locator('article')).toContainText('gpt-4o')

  await page.getByRole('button', { name: '创建渠道' }).click()

  // 保存成功回到列表页，新渠道立刻可见。
  await expect(page).toHaveURL(/\/channels$/)
  await expect(page.getByRole('heading', { name: 'E2E 主渠道' }).first()).toBeVisible()
})

test('网关把原生 chat 请求路由到上游并落日志', async ({ page, baseURL }) => {
  const result = await gatewayFetch(page, baseURL ?? '', '/v1/chat/completions', {
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'ping' }],
  })
  expect(result.status).toBe(200)
  const choices = result.body.choices as Array<{ message: { content: string } }>
  expect(choices[0]?.message.content).toBe('e2e-ok')

  // 上游实际收到的请求：路径、鉴权头都必须符合 OpenAI 约定。
  expect(lastSeen?.path).toBe('/v1/chat/completions')
  expect(lastSeen?.headers.authorization).toBe('Bearer sk-e2e-main')
  expect(lastSeen?.body.model).toBe('gpt-4o')

  // 日志落库是异步的：先轮询 API 等行出现，再打开日志页验证展示。
  await page.goto('/channels')
  await expect
    .poll(async () =>
      page.evaluate(async () => {
        const response = await fetch('/api/logs')
        const envelope = (await response.json()) as { data: unknown[] }
        return envelope.data.length
      }),
    )
    .toBeGreaterThan(0)

  await page.goto('/logs')
  await expect(page.locator('table')).toContainText('gpt-4o')
  await expect(page.locator('table')).toContainText('E2E 主渠道')
})

test('embeddings 请求透传到 chat 端点', async ({ page, baseURL }) => {
  const result = await gatewayFetch(page, baseURL ?? '', '/v1/embeddings', {
    model: 'gpt-4o',
    input: 'hello embeddings',
  })
  expect(result.status).toBe(200)
  const data = result.body.data as Array<{ embedding: number[] }>
  expect(data[0]?.embedding).toEqual([0.1, 0.2, 0.3])

  // 上游收到的路径必须是 /embeddings 而不是对话端点，鉴权头照常注入。
  expect(lastSeen?.path).toBe('/v1/embeddings')
  expect(lastSeen?.headers.authorization).toBe('Bearer sk-e2e-main')
  expect(lastSeen?.body.input).toBe('hello embeddings')
})

test('网关端点带 CORS 头，浏览器客户端可跨源直连', async ({ request, baseURL }) => {
  // preflight：任意源的 OPTIONS 必须放行并声明允许的头。
  const preflight = await request.fetch(`${baseURL}/v1/chat/completions`, {
    method: 'OPTIONS',
    headers: {
      origin: 'https://client.example',
      'access-control-request-method': 'POST',
      'access-control-request-headers': 'authorization, content-type',
    },
  })
  expect(preflight.status()).toBe(204)
  expect(preflight.headers()['access-control-allow-origin']).toBe('*')
  expect(preflight.headers()['access-control-allow-headers']).toContain('authorization')

  // 真实响应（包括错误响应）也必须带 CORS 头，浏览器才能读到 body。
  const response = await request.post(`${baseURL}/v1/chat/completions`, {
    headers: { origin: 'https://client.example' },
    data: { model: 'ghost-model', messages: [] },
  })
  expect(response.headers()['access-control-allow-origin']).toBe('*')
})

test('未授权的协议转换被明确拒绝', async ({ page, baseURL }) => {
  // 渠道没开协议转换：Anthropic 形状打过来必须报错，而不是硬转。
  const result = await gatewayFetch(page, baseURL ?? '', '/v1/messages', {
    model: 'gpt-4o',
    max_tokens: 16,
    messages: [{ role: 'user', content: 'hi' }],
  })
  expect(result.status).toBe(400)
  const envelope = result.body as { error?: { message?: string }; message?: string }
  const message = envelope.error?.message ?? envelope.message ?? ''
  expect(message).toContain('chat')
})

test('开启协议转换后 Messages 客户端能打到 Chat 上游', async ({ page, baseURL }) => {
  await page.goto('/channels')
  await page.getByRole('button', { name: '编辑' }).click()
  await expect(page.getByRole('heading', { name: '编辑渠道' })).toBeVisible()

  // 打开端点的协议转换开关，勾选 Messages。
  await page
    .locator('label')
    .filter({ hasText: '协议转换' })
    .locator('input[type="checkbox"]')
    .check()
  await page
    .locator('label')
    .filter({ has: page.locator('.proto-badge', { hasText: 'Messages' }) })
    .locator('input[type="checkbox"]')
    .check()
  await page.getByRole('button', { name: '保存修改' }).click()
  await expect(page).toHaveURL(/\/channels$/)

  const result = await gatewayFetch(page, baseURL ?? '', '/v1/messages', {
    model: 'gpt-4o',
    max_tokens: 32,
    messages: [{ role: 'user', content: 'hi' }],
  })
  expect(result.status).toBe(200)
  // 回给客户端的必须是 Anthropic 形状。
  expect(result.body.type).toBe('message')
  const content = result.body.content as Array<{ text?: string }>
  expect(content[0]?.text).toBe('e2e-ok')

  // 而上游收到的仍是 Chat 形状 —— 转换发生在网关内部。
  expect(lastSeen?.path).toBe('/v1/chat/completions')
  expect(Array.isArray(lastSeen?.body.messages)).toBe(true)
})

test('模型映射可原地编辑，保存后上游名被改写', async ({ page, baseURL }) => {
  await page.goto('/channels')
  await page.getByRole('button', { name: '编辑' }).click()
  await expect(page.getByRole('heading', { name: '编辑渠道' })).toBeVisible()

  // 未设映射前，上游收到与对外名相同的模型名。
  const before = await gatewayFetch(page, baseURL ?? '', '/v1/chat/completions', {
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'pre-mapping' }],
  })
  expect(before.status).toBe(200)
  expect(lastSeen?.body.model).toBe('gpt-4o')

  // 点模型 chip 进入原地编辑，填上游名，回车确认。
  await page.getByRole('button', { name: 'gpt-4o' }).click()
  const upstreamInput = page.getByRole('textbox', { name: 'gpt-4o 上游名' })
  // 点击即聚焦是编辑体验的一部分（模板 ref 在 v-for 里会变数组，
  // 这行断言守住「点开就落光标」的行为）。
  await expect(upstreamInput).toBeFocused()
  await upstreamInput.fill('e2e-alias')
  await upstreamInput.press('Enter')
  await expect(page.getByRole('button', { name: '→e2e-alias' })).toBeVisible()

  await page.getByRole('button', { name: '保存修改' }).click()
  await expect(page).toHaveURL(/\/channels$/)

  // 保存后：对外名不变，上游实际收到的是映射后的名字。
  const mapped = await gatewayFetch(page, baseURL ?? '', '/v1/chat/completions', {
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'mapped' }],
  })
  expect(mapped.status).toBe(200)
  expect(lastSeen?.body.model).toBe('e2e-alias')

  // 映射在重新打开编辑器后仍在；清空上游名则回到同名直通。
  await page.getByRole('button', { name: '编辑' }).click()
  await page.getByRole('button', { name: '→e2e-alias' }).click()
  const clearInput = page.getByRole('textbox', { name: 'gpt-4o 上游名' })
  await clearInput.fill('')
  await clearInput.press('Enter')
  await expect(page.getByRole('button', { name: '→e2e-alias' })).toBeHidden()

  await page.getByRole('button', { name: '保存修改' }).click()
  await expect(page).toHaveURL(/\/channels$/)

  const restored = await gatewayFetch(page, baseURL ?? '', '/v1/chat/completions', {
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'restored' }],
  })
  expect(restored.status).toBe(200)
  expect(lastSeen?.body.model).toBe('gpt-4o')
})

test('复制渠道产生禁用副本，批量操作可以删掉它', async ({ page }) => {
  await page.goto('/channels')
  await page.getByRole('button', { name: '复制' }).click()

  // 复制后直接进入副本的编辑页。
  await expect(page.getByRole('heading', { name: '编辑渠道' })).toBeVisible()
  await expect(page.getByPlaceholder('例如：中转站-主力')).toHaveValue('E2E 主渠道 副本')

  // 回列表：副本以禁用状态存在。
  await page.getByRole('button', { name: '返回' }).click()
  const copyCard = page.locator('article').filter({ hasText: 'E2E 主渠道 副本' })
  await expect(copyCard).toBeVisible()
  await expect(copyCard.getByRole('switch')).not.toBeChecked()

  // 批量模式：选中副本并删除。
  await page.getByRole('button', { name: '批量管理' }).click()
  await copyCard.click()
  const toolbar = page.getByRole('toolbar', { name: '批量操作' })
  await expect(toolbar).toContainText('已选 1 条')
  await toolbar.getByRole('button', { name: '删除', exact: true }).click()
  await toolbar.getByRole('button', { name: /确认删除 1 条/ }).click()

  await expect(page.locator('article').filter({ hasText: 'E2E 主渠道 副本' })).toHaveCount(0)
  // 原渠道毫发无损。
  await expect(page.getByRole('heading', { name: 'E2E 主渠道' })).toBeVisible()
})

test('编辑渠道不碰密钥时，掩码不会毁掉已保存的凭据', async ({ page, baseURL }) => {
  await page.goto('/channels')
  await page.getByRole('button', { name: '编辑' }).click()
  await expect(page.getByRole('heading', { name: '编辑渠道' })).toBeVisible()

  // 编辑器里显示的是脱敏占位符，不是明文。
  const credential = page.getByRole('textbox', { name: '上游钥匙池，每行一把' })
  await expect(credential).toHaveValue(/…|•/)

  // 只改标签，不碰密钥，保存。
  await page.getByPlaceholder('生产, 便宜').fill('掩码回写回归')
  await page.getByRole('button', { name: '保存修改' }).click()
  await expect(page).toHaveURL(/\/channels$/)

  // 保存后网关发出的请求必须仍带真实密钥 —— 掩码一旦入库这里就是 401。
  const result = await gatewayFetch(page, baseURL ?? '', '/v1/chat/completions', {
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'credential survives edit' }],
  })
  expect(result.status).toBe(200)
  expect(lastSeen?.headers.authorization).toBe('Bearer sk-e2e-main')
})

test('API 密钥明文只出现一次', async ({ page }) => {
  await page.goto('/keys')
  await page.getByRole('button', { name: '新建密钥' }).click()
  await page.getByPlaceholder('例如：本地开发').fill('E2E 客户端')
  await page.getByRole('button', { name: '创建' }).click()

  // 明文块在弹窗里；列表卡片上同时会出现前缀，必须限定作用域。
  const plaintextBlock = page.getByRole('dialog').locator('code')
  await expect(plaintextBlock).toBeVisible()
  const plaintext = ((await plaintextBlock.textContent()) ?? '').trim()
  expect(plaintext.length).toBeGreaterThan(10)

  await page.getByRole('button', { name: '我已保存，关闭' }).click()

  // 列表里只有前缀，完整明文在关闭弹窗后必须从页面消失。
  await expect(page.getByText('E2E 客户端')).toBeVisible()
  await expect(page.locator('body')).not.toContainText(plaintext)
})

test('路由策略修改后持久化', async ({ page }) => {
  await page.goto('/settings')
  const nativeFirst = page.getByRole('switch', { name: '原生优先' })
  await expect(nativeFirst).toBeChecked()

  await nativeFirst.click()
  await page.getByRole('button', { name: '保存设置' }).click()
  await expect(page.getByText('已保存')).toBeVisible()

  await page.reload()
  await expect(page.getByRole('switch', { name: '原生优先' })).not.toBeChecked()

  // 恢复默认，避免影响后续用例的语义。
  await page.getByRole('switch', { name: '原生优先' }).click()
  await page.getByRole('button', { name: '保存设置' }).click()
  await expect(page.getByText('已保存')).toBeVisible()
})

test('200 空回复策略使用默认值、可持久化并热更新严格模式', async ({ page, baseURL }) => {
  await page.goto('/settings')
  const window = page.getByRole('spinbutton', { name: '判定窗口（秒）' })
  const retries = page.getByRole('spinbutton', { name: '最大重试次数' }).last()
  const strict200 = page.getByRole('switch', { name: '非标准 200 转为 500' })
  await expect(window).toHaveValue('3')
  await expect(retries).toHaveValue('5')
  await expect(strict200).not.toBeChecked()

  await window.fill('4')
  await retries.fill('2')
  await strict200.click()
  await page.getByRole('button', { name: '保存设置' }).click()
  await expect(page.getByText('已保存')).toBeVisible()

  const invalid = await gatewayFetch(page, baseURL ?? '', '/v1/chat/completions', {
    model: 'gpt-4o',
    messages: [{ role: 'user', content: 'nonstandard-200-e2e' }],
  })
  expect(invalid.status).toBe(500)
  expect(
    String(invalid.body.error && (invalid.body.error as { message?: string }).message),
  ).toContain('does not match the configured `chat` protocol')

  await page.reload()
  await expect(page.getByRole('spinbutton', { name: '判定窗口（秒）' })).toHaveValue('4')
  await expect(page.getByRole('spinbutton', { name: '最大重试次数' }).last()).toHaveValue('2')
  await expect(page.getByRole('switch', { name: '非标准 200 转为 500' })).toBeChecked()

  await page.getByRole('spinbutton', { name: '判定窗口（秒）' }).fill('3')
  await page.getByRole('spinbutton', { name: '最大重试次数' }).last().fill('5')
  await page.getByRole('switch', { name: '非标准 200 转为 500' }).click()
  await page.getByRole('button', { name: '保存设置' }).click()
  await expect(page.getByText('已保存')).toBeVisible()
})

test('日志保留设置经过 API 持久化并守住输入范围', async ({ page }) => {
  await page.goto('/settings')
  const retention = page.getByRole('spinbutton', { name: '保留天数' })
  await expect(retention).toHaveValue('30')

  await retention.fill('45')
  await page.getByRole('button', { name: '保存设置' }).click()
  await expect(page.getByText('已保存')).toBeVisible()
  await page.reload()
  await expect(page.getByRole('spinbutton', { name: '保留天数' })).toHaveValue('45')

  await retention.fill('0')
  await expect(page.getByRole('alert')).toContainText('1–3650')
  await expect(page.getByRole('button', { name: '保存设置' })).toBeDisabled()

  await retention.fill('30')
  await page.getByRole('button', { name: '保存设置' }).click()
  await expect(page.getByText('已保存')).toBeVisible()
})

test('熔断参数可调、持久化并拒绝非法组合', async ({ page }) => {
  await page.goto('/settings')
  const threshold = page.getByRole('spinbutton', { name: '熔断失败阈值' })
  const base = page.getByRole('spinbutton', { name: '熔断起始冷却秒数' })
  const max = page.getByRole('spinbutton', { name: '熔断冷却上限秒数' })
  await expect(threshold).toHaveValue('5')

  // 上限小于起始值：客户端校验拦下，保存不可点。
  await base.fill('600')
  await max.fill('300')
  await expect(page.getByRole('alert')).toContainText('上限不能小于起始值')
  await expect(page.getByRole('button', { name: '保存设置' })).toBeDisabled()

  // 合法组合保存后经 API 往返仍在。
  await threshold.fill('3')
  await base.fill('10')
  await max.fill('120')
  await page.getByRole('button', { name: '保存设置' }).click()
  await expect(page.getByText('已保存')).toBeVisible()
  await page.reload()
  await expect(page.getByRole('spinbutton', { name: '熔断失败阈值' })).toHaveValue('3')

  // 恢复默认，别影响熔断行为相关的其他用例。
  await page.getByRole('spinbutton', { name: '熔断失败阈值' }).fill('5')
  await page.getByRole('spinbutton', { name: '熔断起始冷却秒数' }).fill('30')
  await page.getByRole('spinbutton', { name: '熔断冷却上限秒数' }).fill('900')
  await page.getByRole('button', { name: '保存设置' }).click()
  await expect(page.getByText('已保存')).toBeVisible()
})

test('390px 视口使用移动导航且核心内容可见', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/')

  await expect(page.getByRole('navigation', { name: '移动端主导航' })).toBeVisible()
  // exact: true —— 否则「主导航」会以子串匹配到「移动端主导航」。
  await expect(page.getByRole('navigation', { name: '主导航', exact: true })).toBeHidden()
  await expect(page.getByRole('heading', { name: '仪表盘' })).toBeVisible()
  await expect(page.getByRole('link', { name: /渠道/ })).toBeVisible()
})

test('管理令牌保护整个管理界面', async ({ page, context }) => {
  await page.goto('/settings')
  await page.getByPlaceholder('新令牌（启用或更换）').fill('e2e-admin-secret')
  await page.getByRole('button', { name: '启用或更换' }).click()
  await expect(page.getByText('令牌已生效，会话已更新。')).toBeVisible()

  // 模拟「换了浏览器」：清除 Session Cookie，管理界面必须被挡住。
  await context.clearCookies()
  await page.reload()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toContainText('管理端身份验证')

  // 错误令牌不能放行：填错令牌点击登录，弹窗显示错误且依然拦截。
  await dialog.getByPlaceholder('adm_... 或自定义管理令牌').fill('wrong-token')
  await dialog.getByRole('button', { name: '登录并进入系统' }).click()
  await expect(dialog.getByText('invalid admin token')).toBeVisible()

  // 正确令牌恢复界面：填入正确令牌点击登录，等待页面加载完成并确认弹窗消失。
  await dialog.getByPlaceholder('adm_... 或自定义管理令牌').fill('e2e-admin-secret')
  await Promise.all([
    page.waitForEvent('load'),
    dialog.getByRole('button', { name: '登录并进入系统' }).click(),
  ])
  await expect(page.getByRole('heading', { name: '设置' })).toBeVisible()
  await expect(page.getByRole('dialog')).toBeHidden()
  // 收尾：关闭管理鉴权，让剩余用例回到开放状态。
  await page.goto('/settings')
  await page.getByRole('button', { name: '关闭管理鉴权' }).click()
  await expect(page.getByText('管理鉴权已关闭。')).toBeVisible()
})

test('连续失败触发熔断，界面可见且可手动解除', async ({ page, baseURL }) => {
  const deadPort = await findDeadPort()

  // 用管理 API 建一个指向死端口的渠道（UI 流程已在创建用例里覆盖过）。
  await page.goto('/channels')
  const created = await page.evaluate(
    async ([port]) => {
      const response = await fetch('/api/channels', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          id: 0,
          owner_id: 1,
          name: 'E2E 熔断渠道',
          kind: 'chat',
          enabled: true,
          priority: 10,
          weight: 1,
          credential: 'sk-dead',
          address: {
            unofficial: true,
            full_address: false,
            base_url: `http://127.0.0.1:${port}`,
            version_prefix: null,
            path: null,
          },
          endpoints: [
            {
              protocol: 'chat',
              order: 0,
              enabled: true,
              address: {
                unofficial: false,
                full_address: false,
                base_url: null,
                version_prefix: null,
                path: null,
              },
              credential: null,
              models: [{ name: 'dead-model', upstream: null }],
              transcode: { enabled: false, accepted: [] },
            },
          ],
          tags: [],
          timeout_secs: 0,
          proxy: null,
          param_override: null,
          note: null,
        }),
      })
      return { status: response.status }
    },
    [deadPort] as const,
  )
  expect(created.status).toBe(200)

  // 默认熔断阈值是连续 5 次失败。多发几发确保越线。
  for (let i = 0; i < 6; i++) {
    const result = await gatewayFetch(page, baseURL ?? '', '/v1/chat/completions', {
      model: 'dead-model',
      messages: [{ role: 'user', content: 'anyone there?' }],
    })
    expect(result.status).toBeGreaterThanOrEqual(500)
  }

  // 等熔断状态落库。
  await expect
    .poll(async () =>
      page.evaluate(async () => {
        const response = await fetch('/api/health/channels')
        const envelope = (await response.json()) as {
          data: Array<{ suspended_until: string | null }>
        }
        return envelope.data.some(
          (h) => h.suspended_until && new Date(h.suspended_until).getTime() > Date.now(),
        )
      }),
    )
    .toBe(true)

  // 列表页必须能看到熔断并能手动解除。
  await page.reload()
  await expect(page.getByText('chat 端点熔断中')).toBeVisible()
  await expect(page.getByText('熔断 1 个端点')).toBeVisible()
  await page.getByRole('button', { name: '解除熔断' }).click()
  await expect(page.getByText('chat 端点熔断中')).toBeHidden()
})

test('仪表盘汇总了全部流量', async ({ page }) => {
  await page.goto('/')
  // 前面已经打了十几次请求：总数不可能是 0。
  await expect(page.locator('.tabular.text-3xl').first()).not.toHaveText(/^0$/)
  // 仪表盘现在有「按模型」与「按渠道」两张表，分别断言。
  await expect(page.getByLabel('按模型统计表').locator('table')).toContainText('gpt-4o')
  await expect(page.getByLabel('按渠道统计表').locator('table')).toContainText('E2E 主渠道')
})

test('设置页可导出备份，导回时同名渠道被跳过', async ({ page }) => {
  await page.goto('/settings')

  // 导出触发浏览器下载，内容是完整的备份文档。
  const downloadPromise = page.waitForEvent('download')
  await page.getByRole('button', { name: '导出备份' }).click()
  const download = await downloadPromise
  expect(download.suggestedFilename()).toMatch(/^refract-backup-.*\.json$/)
  const stream = await download.createReadStream()
  const chunks: Buffer[] = []
  for await (const chunk of stream) chunks.push(chunk as Buffer)
  const document_ = JSON.parse(Buffer.concat(chunks).toString()) as {
    version: number
    channels: Array<{ name: string; credentials: string[] }>
    keys: Array<{ key_hash: string }>
    settings: { log_retention_days: number }
  }
  expect(document_.version).toBe(1)
  expect(document_.channels.map((c) => c.name)).toContain('E2E 主渠道')
  // 备份必须可恢复：渠道凭据是明文（统一存钥匙池），密钥带哈希。
  expect(document_.channels.find((c) => c.name === 'E2E 主渠道')?.credentials).toEqual([
    'sk-e2e-main',
  ])

  // merge 导回：所有内容同名/同哈希，必须全部跳过而不产生重复。
  const before = await page.evaluate(async () => {
    const response = await fetch('/api/channels')
    return ((await response.json()) as { data: unknown[] }).data.length
  })
  const result = await page.evaluate(async (doc) => {
    const response = await fetch('/api/import', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ mode: 'merge', data: doc }),
    })
    return (await response.json()) as { data: { channels_imported: number } }
  }, document_)
  expect(result.data.channels_imported).toBe(0)
  const after = await page.evaluate(async () => {
    const response = await fetch('/api/channels')
    return ((await response.json()) as { data: unknown[] }).data.length
  })
  expect(after).toBe(before)
})

test('替换导入需要二次确认，取消则不动数据', async ({ page }) => {
  await page.goto('/settings')

  // 直接从 API 拿备份文档，UI 下载路径上一条已经验过。
  const document_ = await page.evaluate(async () => {
    const response = await fetch('/api/export')
    return ((await response.json()) as { data: unknown }).data
  })

  await page.getByRole('radio', { name: /替换/ }).check()

  const file = {
    name: 'refract-backup.json',
    mimeType: 'application/json',
    buffer: Buffer.from(JSON.stringify(document_)),
  }

  const before = await page.evaluate(async () => {
    const response = await fetch('/api/channels')
    return ((await response.json()) as { data: Array<{ id: number }> }).data.length
  })

  // 选完文件不立即导入，先弹确认条；取消后什么都不发生。
  await page.getByLabel('选择备份文件').setInputFiles(file)
  const dialog = page.getByRole('alertdialog', { name: '确认替换导入' })
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: '取消' }).click()
  await expect(dialog).toBeHidden()
  const afterCancel = await page.evaluate(async () => {
    const response = await fetch('/api/channels')
    return ((await response.json()) as { data: Array<{ id: number }> }).data.length
  })
  expect(afterCancel).toBe(before)

  // 再来一次，这回确认 —— 清空后导入同一份备份，渠道数量不变。
  await page.getByLabel('选择备份文件').setInputFiles(file)
  await dialog.getByRole('button', { name: '确认替换' }).click()
  await expect(page.getByRole('status')).toContainText('导入完成')
  const afterReplace = await page.evaluate(async () => {
    const response = await fetch('/api/channels')
    return ((await response.json()) as { data: Array<{ id: number }> }).data.length
  })
  expect(afterReplace).toBe(before)
})

test('调试台走完整网关管线并流式渲染回复', async ({ page }) => {
  await page.goto('/playground')
  await expect(page.getByRole('heading', { name: '调试台' })).toBeVisible()

  // 模型下拉由 /api/models 派生；显式选中目标模型（列表里可能还有
  // 其他测试留下的渠道）。
  await page.getByLabel('调试模型').selectOption('gpt-4o')

  await page.getByLabel('消息输入').fill('ping from playground')
  await page.getByRole('button', { name: '发送' }).click()

  // 假上游按 SSE 分两帧回 "e2e-" + "ok"，最终气泡应拼出完整文本。
  await expect(page.getByLabel('会话记录')).toContainText('e2e-ok')
  // 上游收到的请求确实是流式的（走了真实的网关流式管线）。
  expect(lastSeen?.body.stream).toBe(true)
})

test('日志详情弹窗展示完整请求与响应正文', async ({ page }) => {
  // serial 前序用例刚发过调试台流式请求：最新一条日志就是它。
  await page.goto('/logs')
  await page.locator('tbody tr').first().click()
  await page.getByRole('button', { name: '查看完整请求' }).click()

  const dialog = page.getByRole('dialog', { name: '完整请求' })
  await expect(dialog).toBeVisible()
  // 请求正文：调试台发出的用户消息。
  await expect(dialog).toContainText('ping from playground')
  // 响应正文：流式聚合出的文本。
  await expect(dialog).toContainText('e2e-ok')
  await dialog.getByRole('button', { name: '关闭' }).click()
  await expect(dialog).toBeHidden()
})

test('价表在设置页维护，模型页汇总渠道与价格', async ({ page }) => {
  await page.goto('/settings')
  await page.getByRole('button', { name: '添加规则' }).click()
  await page.getByLabel('价表第 1 行模式').fill('gpt-4o')
  await page.getByLabel('价表第 1 行输入单价').fill('2.5')
  await page.getByLabel('价表第 1 行输出单价').fill('10')
  await page.getByRole('button', { name: '保存设置' }).click()
  await expect(page.getByText('已保存')).toBeVisible()

  await page.goto('/models')
  const table = page.locator('table')
  await expect(table).toContainText('gpt-4o')
  await expect(table).toContainText('E2E 主渠道')
  await expect(table).toContainText('2.5')
  await expect(table).toContainText('10')
})

test('渠道编辑器支持批量粘贴并按多种分隔符自动切分模型标签', async ({ page }) => {
  await page.goto('/channels/new')
  const input = page.getByRole('textbox', { name: 'Chat 模型输入' })
  await input.focus()

  await page.evaluate(() => {
    const target = document.querySelector('input[aria-label="Chat 模型输入"]') as HTMLInputElement
    const dt = new DataTransfer()
    dt.setData('text/plain', 'gpt-4o, gpt-4o-mini; claude-3-5-sonnet\ndeepseek-chat')
    const pasteEvent = new ClipboardEvent('paste', {
      bubbles: true,
      cancelable: true,
      clipboardData: dt,
    })
    target.dispatchEvent(pasteEvent)
  })

  await expect(page.getByText('gpt-4o', { exact: true })).toBeVisible()
  await expect(page.getByText('gpt-4o-mini', { exact: true })).toBeVisible()
  await expect(page.getByText('claude-3-5-sonnet', { exact: true })).toBeVisible()
  await expect(page.getByText('deepseek-chat', { exact: true })).toBeVisible()
})

test('表单未保存修改在路由切换时触发确认拦截', async ({ page }) => {
  await page.goto('/channels/new')
  await page.getByPlaceholder('例如：中转站-主力').fill('草稿渠道')

  let dialogMessage = ''
  page.once('dialog', async (dialog) => {
    dialogMessage = dialog.message()
    await dialog.dismiss()
  })

  await page.getByRole('link', { name: '仪表盘' }).click()
  expect(dialogMessage).toContain('未保存')

  await expect(page.getByRole('heading', { name: '新建渠道' })).toBeVisible()
  await expect(page.getByPlaceholder('例如：中转站-主力')).toHaveValue('草稿渠道')
})

test('请求日志行支持键盘 Enter/Space 展开折叠与 ARIA 状态更新', async ({ page }) => {
  await page.goto('/logs')
  const row = page.locator('tbody tr[role="button"]').first()
  await expect(row).toBeVisible()
  await expect(row).toHaveAttribute('aria-expanded', 'false')

  await row.focus()
  await row.press('Enter')
  await expect(row).toHaveAttribute('aria-expanded', 'true')
  await expect(row).toHaveAttribute('aria-controls', /^log-detail-\d+$/)
  const detailRow = page.locator('tbody tr[id^="log-detail-"]').first()
  await expect(detailRow).toBeVisible()

  await row.press('Space')
  await expect(row).toHaveAttribute('aria-expanded', 'false')
  await expect(detailRow).toBeHidden()
})
