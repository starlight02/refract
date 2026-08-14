import { fileURLToPath, URL } from 'node:url'

import { defineConfig } from 'vite-plus'
import vue from '@vitejs/plugin-vue'
import tailwindcss from '@tailwindcss/vite'

// Vue 3.6 Vapor Mode：编译期去掉虚拟 DOM，直接生成命令式的 DOM 更新代码。
// 对这个项目的意义很实在 —— 日志页会渲染上千行、渠道列表会频繁增删，
// Vapor 让这些列表的更新不用先构造再 diff 一棵虚拟树。
export default defineConfig({
  fmt: {
    semi: false,
    singleQuote: true,
  },
  lint: {
    plugins: ['eslint', 'typescript', 'unicorn', 'oxc', 'vue', 'vitest'],
    categories: {
      correctness: 'error',
    },
    env: {
      browser: true,
      builtin: true,
    },
    ignorePatterns: ['**/dist/**', '**/dist-ssr/**', '**/coverage/**'],
    rules: {
      'no-array-constructor': 'error',
      'typescript/ban-ts-comment': 'error',
      'typescript/no-empty-object-type': 'error',
      'typescript/no-explicit-any': 'error',
      'typescript/no-namespace': 'error',
      'typescript/no-require-imports': 'error',
      'typescript/no-unnecessary-type-constraint': 'error',
      'typescript/no-unsafe-function-type': 'error',
      'vite-plus/prefer-vite-plus-imports': 'error',
    },
    overrides: [
      {
        files: ['**/*.ts', '**/*.tsx', '**/*.mts', '**/*.cts', '**/*.vue'],
        rules: {
          'constructor-super': 'off',
          'getter-return': 'off',
          'no-class-assign': 'off',
          'no-const-assign': 'off',
          'no-dupe-class-members': 'off',
          'no-dupe-keys': 'off',
          'no-func-assign': 'off',
          'no-import-assign': 'off',
          'no-new-native-nonconstructor': 'off',
          'no-obj-calls': 'off',
          'no-redeclare': 'off',
          'no-setter-return': 'off',
          'no-this-before-super': 'off',
          'no-undef': 'off',
          'no-unreachable': 'off',
          'no-unsafe-negation': 'off',
          'no-var': 'error',
          'no-with': 'off',
          'prefer-const': 'error',
          'prefer-rest-params': 'error',
          'prefer-spread': 'error',
        },
      },
      {
        files: ['e2e/**/*.{test,spec}.{js,ts,jsx,tsx}'],
        rules: {
          'no-empty-pattern': 'off',
          'playwright/consistent-spacing-between-blocks': 'warn',
          'playwright/expect-expect': 'warn',
          'playwright/max-nested-describe': 'warn',
          'playwright/missing-playwright-await': 'error',
          'playwright/no-conditional-expect': 'warn',
          'playwright/no-conditional-in-test': 'warn',
          'playwright/no-duplicate-hooks': 'warn',
          'playwright/no-duplicate-slow': 'warn',
          'playwright/no-element-handle': 'warn',
          'playwright/no-eval': 'warn',
          'playwright/no-focused-test': 'error',
          'playwright/no-force-option': 'warn',
          'playwright/no-nested-step': 'warn',
          'playwright/no-networkidle': 'error',
          'playwright/no-page-pause': 'warn',
          'playwright/no-skipped-test': 'warn',
          'playwright/no-standalone-expect': 'error',
          'playwright/no-unnecessary-assertions': 'error',
          'playwright/no-unsafe-references': 'error',
          'playwright/no-unused-locators': 'error',
          'playwright/no-useless-await': 'warn',
          'playwright/no-useless-not': 'warn',
          'playwright/no-wait-for-navigation': 'error',
          'playwright/no-wait-for-selector': 'warn',
          'playwright/no-wait-for-timeout': 'warn',
          'playwright/prefer-hooks-in-order': 'warn',
          'playwright/prefer-hooks-on-top': 'warn',
          'playwright/prefer-locator': 'warn',
          'playwright/prefer-to-have-count': 'warn',
          'playwright/prefer-to-have-length': 'warn',
          'playwright/prefer-web-first-assertions': 'error',
          'playwright/valid-describe-callback': 'error',
          'playwright/valid-expect': 'error',
          'playwright/valid-expect-in-promise': 'error',
          'playwright/valid-test-tags': 'error',
          'playwright/valid-title': 'error',
        },
        jsPlugins: ['eslint-plugin-playwright'],
      },
      {
        files: ['src/**/__tests__/*'],
        rules: {
          'vitest/expect-expect': 'error',
          'vitest/no-commented-out-tests': 'error',
          'vitest/no-conditional-expect': 'error',
          'vitest/no-disabled-tests': 'warn',
          'vitest/no-focused-tests': 'error',
          'vitest/no-identical-title': 'error',
          'vitest/no-import-node-test': 'error',
          'vitest/no-interpolation-in-snapshots': 'error',
          'vitest/no-mocks-import': 'error',
          'vitest/no-standalone-expect': 'error',
          'vitest/no-unneeded-async-expect-function': 'error',
          'vitest/prefer-called-exactly-once-with': 'error',
          'vitest/require-local-test-context-for-concurrent-snapshots': 'error',
          'vitest/valid-describe-callback': 'error',
          'vitest/valid-expect': 'error',
          'vitest/valid-expect-in-promise': 'error',
          'vitest/valid-title': 'error',
        },
      },
    ],
    options: {
      typeAware: true,
      typeCheck: true,
    },
    jsPlugins: [
      {
        name: 'vite-plus',
        specifier: 'vite-plus/oxlint-plugin',
      },
    ],
  },
  plugins: [
    vue({
      // 全项目启用 Vapor：单文件组件不必逐个加 `<script setup vapor>`。
      features: { vapor: true },
    }),
    tailwindcss(),
  ],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  build: {
    // 后端用 rust-embed 内嵌 dist，产物越小启动镜像越小。
    target: 'esnext',
    cssCodeSplit: true,
    // 压缩尺寸报表要对每个 chunk 再跑一遍 gzip，只为了打印日志；
    // 真实的体积门禁看 dist 本身，关掉换构建速度。
    reportCompressedSize: false,
    rollupOptions: {
      output: {
        // 把变动频率差异大的依赖拆开：vue 运行时几乎不变，业务代码天天变。
        // 分开后用户不必为一次改动重下整个 bundle。
        //
        // Vite 8 走 Rolldown：分包用 `codeSplitting.groups`（正则匹配模块
        // 路径）。Rollup 时代的 `manualChunks` 对象形式在 Rolldown 下会直接
        // 报 "manualChunks is not a function"，`advancedChunks` 则已被废弃。
        codeSplitting: {
          groups: [
            { name: 'vue', test: /node_modules[\\/](?:@?vue|vue-router|pinia)/ },
            { name: 'reka', test: /node_modules[\\/]reka-ui/ },
          ],
        },
      },
    },
  },
  server: {
    proxy: {
      // 开发时把 API 打到本地后端，避免 CORS 配置污染生产代码。
      '/api': backendProxy(),
      '/v1': backendProxy(),
      '/v1beta': backendProxy(),
      '/health': backendProxy(),
      '/metrics': backendProxy(),
    },
  },
})

/**
 * 指向本地后端的代理条目。
 *
 * `pnpm dev` 里 cargo watch 编译期间（首次启动或每次改 Rust 代码）后端
 * 必然不可达，http-proxy 默认回一个裸 502 —— 页面看起来像坏了。这里把
 * 代理错误换成结构化的 503：前端客户端据此识别「后端编译中」，对 GET
 * 自动重试，App 壳显示恢复横幅。生产形态（单二进制内嵌前端）没有这层
 * 代理，不受影响。
 */
function backendProxy() {
  return {
    target: 'http://127.0.0.1:3939',
    changeOrigin: true,
    configure(proxy: { on(event: 'error', cb: (...args: unknown[]) => void): void }) {
      proxy.on('error', (...args) => {
        const res = args[2] as {
          headersSent?: boolean
          writeHead?: (status: number, headers: Record<string, string>) => void
          end?: (chunk: string) => void
        }
        // SSE/WebSocket 升级失败时第三个参数可能是裸 socket，防御式判断。
        if (typeof res?.writeHead === 'function' && !res.headersSent) {
          res.writeHead(503, { 'content-type': 'application/json' })
        }
        res?.end?.(
          JSON.stringify({
            code: 'backend_unavailable',
            message: '后端不可达 —— 正在编译或尚未启动，就绪后会自动恢复',
          }),
        )
      })
    },
  }
}
