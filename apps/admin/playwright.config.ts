import { defineConfig, devices } from '@playwright/test'

/**
 * E2E 对着**真实的生产形态**跑：refract-server 单二进制（内嵌前端）+
 * 一次性 SQLite。不拆「前端 + mock 后端」—— 管理 API 的形状、鉴权、
 * 网关路由都是被测对象的一部分，mock 掉它们等于没测。
 *
 * 启动流程见 `e2e/serve.sh`：确保 dist 与二进制新鲜后在 4539 端口拉起。
 */
export default defineConfig({
  testDir: './e2e',
  timeout: 60 * 1000,
  expect: {
    timeout: 10_000,
  },
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  // 所有用例共享同一个服务端实例与数据库，必须串行。
  workers: 1,
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : 'list',
  use: {
    actionTimeout: 10_000,
    baseURL: 'http://127.0.0.1:4539',
    trace: 'on-first-retry',
    // 默认无头：E2E 大多在自动化里跑；想亲眼看浏览器时 HEADED=1。
    headless: process.env.HEADED ? false : true,
  },
  projects: [
    {
      name: 'chromium',
      use: {
        ...devices['Desktop Chrome'],
      },
    },
  ],
  webServer: {
    command: 'sh e2e/serve.sh',
    url: 'http://127.0.0.1:4539/',
    // 首次运行可能要编译前端与后端，给足时间；之后是秒级 no-op。
    timeout: 300 * 1000,
    reuseExistingServer: false,
  },
})
