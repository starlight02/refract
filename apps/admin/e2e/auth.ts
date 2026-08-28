import { readFile } from 'node:fs/promises'
import { join } from 'node:path'

import { expect, type Page } from '@playwright/test'

const issuedTokenPath = join(process.cwd(), 'e2e/.issued-admin-token')

/** 读取 `serve.sh` 从数据目录 `.admin_token` 抄出来的签发明文。 */
export async function readIssuedAdminToken(): Promise<string> {
  let lastError: unknown
  for (let attempt = 0; attempt < 50; attempt += 1) {
    try {
      const token = (await readFile(issuedTokenPath, 'utf8')).trim()
      if (token.startsWith('adm_')) {
        return token
      }
      lastError = new Error(`issued token has unexpected shape: ${token.slice(0, 8)}`)
    } catch (error) {
      lastError = error
    }
    await new Promise((resolve) => setTimeout(resolve, 100))
  }
  throw new Error(`issued admin token not found at ${issuedTokenPath}: ${String(lastError)}`)
}

/** 用服务端签发的管理令牌换 Session Cookie。 */
export async function loginAsAdmin(page: Page): Promise<void> {
  await page.context().addCookies([
    {
      name: 'PARAGLIDE_LOCALE',
      value: 'zh-Hans',
      domain: '127.0.0.1',
      path: '/',
    },
  ])
  const token = await readIssuedAdminToken()
  const response = await page.request.post('/api/auth/login', {
    data: { token },
  })
  expect(
    response.ok(),
    `e2e login failed: ${response.status()} ${await response.text()}`,
  ).toBeTruthy()
}

/** 用邮箱密码换 Session Cookie。 */
export async function loginAsUser(page: Page, email: string, password: string): Promise<void> {
  await page.context().addCookies([
    {
      name: 'PARAGLIDE_LOCALE',
      value: 'zh-Hans',
      domain: '127.0.0.1',
      path: '/',
    },
  ])
  const response = await page.request.post('/api/auth/login', {
    data: { email, password },
  })
  expect(
    response.ok(),
    `e2e user login failed: ${response.status()} ${await response.text()}`,
  ).toBeTruthy()
}
