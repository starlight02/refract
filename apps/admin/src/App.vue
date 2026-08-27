<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, toRef } from 'vue'
import { RouterLink, RouterView } from 'vue-router'
import {
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from 'reka-ui'
import AppIcon from '@/components/AppIcon.vue'
import GlassToastContainer from '@/components/GlassToastContainer.vue'
import { useAction } from '@/composables/useAction'
import * as m from '@/paraglide/messages'
import { getLocale, setLocale } from '@/paraglide/runtime'
import { authGate } from '@/router'
import { orElse } from '@/utils/effect'
import { initLiquidGlass } from '@/utils/liquidGlass'
import { AUTH_REQUIRED_EVENT, BACKEND_DOWN_EVENT, BACKEND_RESTORED_EVENT, auth } from '@/api/client'
import { CRYPTO_PLAINTEXT_EVENT } from '@/api/crypto'

/** 深色模式偏好的存储键。 */
const THEME_KEY = 'refract.theme'

/**
 * 深色模式。真值来源是 <html> 上的 `.dark` 类名（设计系统用类名切换而非
 * media query），这里只做镜像，避免出现「ref 说亮、DOM 是暗」的错位。
 */
const isDark = ref(false)
const currentLocale = computed(() => getLocale())

function toggleLanguage() {
  setLocale(getLocale() === 'zh-Hans' ? 'en' : 'zh-Hans')
}

function applyTheme(dark: boolean) {
  isDark.value = dark
  document.documentElement.classList.toggle('dark', dark)
  localStorage.setItem(THEME_KEY, dark ? 'dark' : 'light')
}

onMounted(async () => {
  // 挂载物理液态玻璃滤镜
  initLiquidGlass()
  // 首次访问没有存储值时跟随系统，之后用户的显式选择优先。
  const saved = localStorage.getItem(THEME_KEY)
  const dark =
    saved === 'dark' ||
    (saved === null && window.matchMedia('(prefers-color-scheme: dark)').matches)
  applyTheme(dark)

  const sess = await orElse(() => auth.session())
  if (authGate.needsLogin || (sess?.configured && !sess.authenticated)) {
    tokenDialogOpen.value = true
  }
})

/**
 * 管理令牌失效弹窗。
 *
 * 任何一个管理 API 撞见 401/403 都会派发 AUTH_REQUIRED_EVENT。弹窗是唯一
 * 的恢复入口：填入令牌 → 换发 Session Cookie → 重新加载，让所有页面用新会话
 * 重新拉数据。不尝试「原地重试当前请求」—— 各页面都有自己的加载逻辑，
 * 整页重载最简单也最不容易留下半更新的状态。
 */
const tokenDialogOpen = ref(false)
const tokenDraft = ref('')
const showToken = ref(false)
const login = useAction(m.auth_login_failed())
const tokenError = toRef(login, 'error')
const tokenBusy = toRef(login, 'busy')

function onAuthRequired() {
  if (!tokenDialogOpen.value) {
    tokenDraft.value = ''
    login.clear()
    tokenDialogOpen.value = true
  }
}

async function saveTokenAndReload() {
  const token = tokenDraft.value.trim()
  if (!token || tokenBusy.value) return
  await login.run(
    () => auth.login(token),
    () => {
      tokenDraft.value = ''
      tokenDialogOpen.value = false
      window.location.reload()
    },
  )
}

onMounted(() => window.addEventListener(AUTH_REQUIRED_EVENT, onAuthRequired))
onBeforeUnmount(() => window.removeEventListener(AUTH_REQUIRED_EVENT, onAuthRequired))

/**
 * 后端连不上时的横幅。只在 fetch 网络失败或 dev 代理 503 时出现，
 * 业务 4xx/5xx 走页面自己的错误文案，不再谎称「正在编译」。
 */
const backendDown = ref(false)
let healthTimer: ReturnType<typeof setInterval> | null = null

function onBackendRestored() {
  if (backendDown.value) {
    backendDown.value = false
    if (healthTimer) clearInterval(healthTimer)
    healthTimer = null
  }
}

function onBackendDown() {
  if (backendDown.value) return
  backendDown.value = true
  healthTimer = setInterval(async () => {
    const live = await fetch('/health/live').then(
      (response) => response.ok,
      () => false,
    )
    if (live) onBackendRestored()
  }, 1_500)
}

onMounted(() => {
  window.addEventListener(BACKEND_DOWN_EVENT, onBackendDown)
  window.addEventListener(BACKEND_RESTORED_EVENT, onBackendRestored)
})
onBeforeUnmount(() => {
  window.removeEventListener(BACKEND_DOWN_EVENT, onBackendDown)
  window.removeEventListener(BACKEND_RESTORED_EVENT, onBackendRestored)
  if (healthTimer) clearInterval(healthTimer)
})

const cryptoPlaintext = ref(false)

function onCryptoPlaintext() {
  cryptoPlaintext.value = true
}

onMounted(() => window.addEventListener(CRYPTO_PLAINTEXT_EVENT, onCryptoPlaintext))
onBeforeUnmount(() => window.removeEventListener(CRYPTO_PLAINTEXT_EVENT, onCryptoPlaintext))

/** 桌面侧栏主导航。 */
const navItems = computed(() => [
  { name: 'dashboard', label: m.nav_dashboard(), icon: 'gauge' as const },
  { name: 'channels', label: m.nav_channels(), icon: 'channels' as const },
  { name: 'models', label: m.nav_models(), icon: 'boxes' as const },
  { name: 'playground', label: m.nav_playground(), icon: 'chat' as const },
  { name: 'logs', label: m.nav_logs(), icon: 'logs' as const },
  { name: 'keys', label: m.nav_keys(), icon: 'key' as const },
])
/** 移动端底栏只放五个高频入口 —— 模型与调试台是桌面场景。 */
const mobileNavItems = computed(() => [
  { name: 'dashboard', label: m.nav_dashboard(), icon: 'gauge' as const },
  { name: 'channels', label: m.nav_channels(), icon: 'channels' as const },
  { name: 'logs', label: m.nav_logs(), icon: 'logs' as const },
  { name: 'keys', label: m.nav_keys(), icon: 'key' as const },
  { name: 'settings', label: m.nav_settings(), icon: 'settings' as const },
])
// 版本号由 vite.config.ts 在构建期从 apps/admin/package.json 注入，与后端版本同步演进。
const version = __APP_VERSION__
</script>

<template>
  <!-- 根容器不能带不透明背景：正常流盒的背景绘制在负 z-index 元素之上，
       会把 .canvas-aurora 完全盖住。画布底色由 body 提供。 -->
  <div class="flex min-h-screen text-ink">
    <!-- 极光色斑背景：给所有玻璃层一个可折射的对象，见 main.css 的说明。 -->
    <div class="canvas-aurora" aria-hidden="true"></div>

    <!-- 后端不可达横幅：编译/重启窗口的全局提示，恢复后自动消失。 -->
    <Transition
      enter-active-class="transition-opacity duration-300"
      enter-from-class="opacity-0"
      leave-active-class="transition-opacity duration-300"
      leave-to-class="opacity-0"
    >
      <div
        v-if="backendDown"
        class="glass fixed top-20 left-1/2 z-40 flex max-w-[calc(100%-2rem)] -translate-x-1/2 items-center gap-2.5 border-warning/30 bg-warning/12 px-4 py-2.5 text-sm font-medium text-ink shadow-[0_12px_32px_-14px_oklch(0%_0_0/0.28)] md:top-3"
        role="status"
      >
        <AppIcon name="spinner" class="animate-spin text-warning shrink-0" :size="15" />
        <span>{{ m.app_backend_reconnecting() }}</span>
      </div>
    </Transition>
    <Transition
      enter-active-class="transition-opacity duration-300"
      enter-from-class="opacity-0"
      leave-active-class="transition-opacity duration-300"
      leave-to-class="opacity-0"
    >
      <div
        v-if="cryptoPlaintext"
        class="glass fixed top-20 left-1/2 z-40 flex max-w-[calc(100%-2rem)] -translate-x-1/2 items-center gap-2.5 border-warning/30 bg-warning/12 px-4 py-2.5 text-sm font-medium text-ink shadow-[0_12px_32px_-14px_oklch(0%_0_0/0.28)] md:top-3"
        role="status"
      >
        <span>{{ m.app_crypto_plaintext_warning() }}</span>
        <button
          type="button"
          class="shrink-0 text-ink-soft hover:text-ink cursor-pointer"
          :aria-label="m.app_close_warning()"
          @click="cryptoPlaintext = false"
        >
          {{ m.common_close() }}
        </button>
      </div>
    </Transition>
    <header
      class="glass-thick mobile-topbar fixed inset-x-3 top-3 z-30 flex h-14 items-center justify-between px-3 md:hidden"
    >
      <RouterLink :to="{ name: 'dashboard' }" class="flex items-center gap-2 no-underline">
        <img
          src="/favicon.svg"
          alt=""
          class="size-8 shrink-0"
          aria-hidden="true"
          draggable="false"
        />
        <span class="text-sm font-semibold text-ink">Refract</span>
      </RouterLink>
      <div class="flex items-center gap-1">
        <button
          type="button"
          class="grid size-10 place-items-center rounded-[var(--radius-control)] text-ink-soft hover:bg-ink/8 hover:text-ink cursor-pointer"
          :aria-label="m.app_language()"
          @click="toggleLanguage"
        >
          <span class="text-xs font-semibold uppercase">{{
            currentLocale === 'zh-Hans' ? 'EN' : '中'
          }}</span>
        </button>
        <button
          type="button"
          class="grid size-10 place-items-center rounded-[var(--radius-control)] text-ink-soft hover:bg-ink/8 hover:text-ink cursor-pointer"
          :aria-label="isDark ? m.app_switch_to_light() : m.app_switch_to_dark()"
          :aria-pressed="isDark"
          @click="applyTheme(!isDark)"
        >
          <AppIcon :name="isDark ? 'moon' : 'sun'" :size="18" />
        </button>
      </div>
    </header>

    <!-- 侧栏：固定宽度、满高、独立滚动。
         p-3 的外距让玻璃的阴影有地方溢出，不然会被视口边缘裁掉。 -->
    <aside
      class="fixed top-0 left-0 z-20 hidden h-screen w-[240px] shrink-0 p-3 md:block xl:w-[264px]"
    >
      <nav
        class="glass-thick glass-specular flex h-full flex-col overflow-y-auto px-4 py-5"
        :aria-label="m.nav_main_nav()"
      >
        <!-- macOS 窗口控制红黄绿按纽 -->
        <div class="mb-5 flex items-center gap-2 px-2" aria-hidden="true">
          <span
            class="size-3 rounded-full bg-[#ff5f56] border border-[#e0443e]/50 shadow-xs"
          ></span>
          <span
            class="size-3 rounded-full bg-[#ffbd2e] border border-[#dea123]/50 shadow-xs"
          ></span>
          <span
            class="size-3 rounded-full bg-[#27c93f] border border-[#1aab29]/50 shadow-xs"
          ></span>
        </div>

        <!-- 标识 -->
        <RouterLink
          :to="{ name: 'dashboard' }"
          class="mb-6 flex items-center gap-3 px-2 no-underline"
        >
          <img
            src="/favicon.svg"
            alt=""
            class="size-9 shrink-0"
            aria-hidden="true"
            draggable="false"
          />
          <span class="flex flex-col leading-tight">
            <span class="text-[0.95rem] font-semibold text-ink">Refract</span>
            <span class="text-[0.7rem] text-ink-faint">{{ m.app_gateway_tag() }}</span>
          </span>
        </RouterLink>

        <!-- 主导航项 -->
        <ul class="flex list-none flex-col gap-1 p-0">
          <li v-for="item in navItems" :key="item.name">
            <RouterLink
              :to="{ name: item.name }"
              class="shape-nav group flex items-center gap-3 px-3 py-2.5 text-[0.875rem] font-medium text-ink-soft no-underline transition-all duration-150 hover:bg-black/5 hover:text-ink dark:hover:bg-white/10"
              active-class="glass-tab-active"
            >
              <span class="grid w-4 place-items-center" aria-hidden="true">
                <AppIcon :name="item.icon" />
              </span>
              {{ item.label }}
            </RouterLink>
          </li>
        </ul>

        <!-- mt-auto 把以下内容顶到侧栏底部 -->
        <div class="mt-auto flex flex-col gap-1 pt-5">
          <RouterLink
            to="/settings"
            class="shape-nav flex items-center gap-3 px-3 py-2.5 text-[0.875rem] font-medium text-ink-soft no-underline transition-all duration-150 hover:bg-black/5 hover:text-ink dark:hover:bg-white/10"
            active-class="glass-tab-active"
          >
            <span class="grid w-4 place-items-center" aria-hidden="true">
              <AppIcon name="settings" />
            </span>
            {{ m.nav_settings() }}
          </RouterLink>

          <button
            type="button"
            class="shape-nav flex w-full cursor-pointer items-center gap-3 border-0 bg-transparent px-3 py-2.5 text-left text-[0.875rem] font-medium text-ink-soft transition-all duration-150 hover:bg-black/5 hover:text-ink dark:hover:bg-white/10"
            @click="toggleLanguage"
          >
            <span class="grid w-4 place-items-center" aria-hidden="true">
              <AppIcon name="languages" />
            </span>
            {{ currentLocale === 'zh-Hans' ? 'English' : '简体中文' }}
          </button>

          <button
            type="button"
            class="shape-nav flex w-full cursor-pointer items-center gap-3 border-0 bg-transparent px-3 py-2.5 text-left text-[0.875rem] font-medium text-ink-soft transition-all duration-150 hover:bg-black/5 hover:text-ink dark:hover:bg-white/10"
            @click="applyTheme(!isDark)"
          >
            <span class="grid w-4 place-items-center" aria-hidden="true">
              <AppIcon :name="isDark ? 'moon' : 'sun'" />
            </span>
            {{ isDark ? m.app_dark_mode() : m.app_light_mode() }}
          </button>

          <a
            href="https://github.com/starlight02/refract"
            target="_blank"
            rel="noreferrer"
            title="GitHub"
            class="tabular m-0 flex items-center gap-1.5 px-3 pt-2 text-[0.7rem] text-ink-faint no-underline transition-colors hover:text-ink-soft"
          >
            <svg class="size-3 shrink-0" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
              <path
                d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12"
              />
            </svg>
            v{{ version }}
          </a>
        </div>
      </nav>
    </aside>

    <!-- 主内容区。ml 让出侧栏宽度，min-w-0 防止宽表格把布局撑破。 -->
    <main
      class="min-h-screen min-w-0 flex-1 px-4 pt-24 pb-28 sm:px-5 md:ml-[240px] md:p-5 xl:ml-[264px] xl:p-6"
      :class="backendDown ? 'max-md:pt-40' : ''"
    >
      <RouterView />
    </main>

    <nav
      class="mobile-tabbar fixed inset-x-3 bottom-3 z-30 md:hidden"
      :aria-label="m.nav_mobile_nav()"
    >
      <RouterLink
        v-for="item in mobileNavItems"
        :key="item.name"
        :to="{ name: item.name }"
        class="mobile-tab-item"
        active-class="mobile-tab-item-active"
      >
        <span class="mobile-tab-icon" aria-hidden="true">
          <AppIcon :name="item.icon" :size="18" />
        </span>
        <span class="mobile-tab-label">{{ item.label }}</span>
      </RouterLink>
    </nav>

    <!-- 管理令牌失效弹窗：401/403 时的全局恢复入口 -->
    <DialogRoot v-model:open="tokenDialogOpen">
      <DialogPortal>
        <DialogOverlay
          class="fixed inset-0 z-50 bg-ink/25 backdrop-blur-sm data-[state=closed]:opacity-0 data-[state=open]:opacity-100"
        />
        <DialogContent
          class="glass-thick glass-specular fixed top-1/2 left-1/2 z-50 w-[calc(100%-2rem)] max-w-md -translate-x-1/2 -translate-y-1/2 p-6 outline-none"
        >
          <DialogTitle class="text-lg font-semibold">{{ m.auth_title() }}</DialogTitle>
          <DialogDescription class="mt-1 text-xs text-ink-faint">
            {{ m.auth_desc_p1() }}
            <code class="font-mono text-ink-soft">.admin_token</code>
            {{ m.auth_desc_p2() }}
          </DialogDescription>

          <form class="mt-5 flex flex-col gap-4" @submit.prevent="saveTokenAndReload">
            <div v-if="tokenError" class="rounded-lg bg-danger/10 p-3 text-xs text-danger">
              {{ tokenError }}
            </div>
            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                m.auth_admin_account()
              }}</label>
              <input
                type="text"
                value="admin@localhost"
                readonly
                disabled
                class="glass-field w-full cursor-not-allowed bg-ink/5 px-3 py-2 text-sm text-ink-faint outline-none"
              />
            </div>

            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                m.auth_admin_token()
              }}</label>
              <div class="relative">
                <input
                  v-model="tokenDraft"
                  :type="showToken ? 'text' : 'password'"
                  :placeholder="m.auth_token_placeholder()"
                  autocomplete="current-password"
                  autofocus
                  :aria-label="m.auth_token_aria()"
                  class="glass-field w-full px-3 py-2 pr-16 font-mono text-sm outline-none"
                />
                <button
                  type="button"
                  class="absolute top-1/2 right-2 -translate-y-1/2 rounded-md px-2 py-1 text-xs text-ink-faint hover:text-ink"
                  :aria-label="showToken ? m.auth_hide_token() : m.auth_show_token()"
                  :aria-pressed="showToken"
                  @click="showToken = !showToken"
                >
                  {{ showToken ? m.common_hide() : m.common_show() }}
                </button>
              </div>
            </div>
            <div class="flex items-center gap-3">
              <button
                type="submit"
                class="glass-button-primary px-4 py-2.5 text-sm font-medium disabled:opacity-50"
                :disabled="!tokenDraft.trim() || tokenBusy"
              >
                <AppIcon v-if="tokenBusy" name="spinner" class="animate-spin mr-1" :size="14" />
                {{ tokenBusy ? m.auth_logging_in() : m.auth_login_btn() }}
              </button>
              <button
                type="button"
                class="rounded-lg px-3 py-2.5 text-sm text-ink-soft hover:bg-ink/8 hover:text-ink cursor-pointer"
                @click="tokenDialogOpen = false"
              >
                {{ m.common_cancel() }}
              </button>
            </div>

            <div class="mt-2 rounded-lg border border-ink/8 bg-ink/5 p-3 text-xs text-ink-faint">
              <div class="font-medium text-ink-soft mb-1 flex items-center gap-1.5">
                <AppIcon name="info" :size="13" />
                <span>{{ m.auth_reset_guide_title() }}</span>
              </div>
              <p class="leading-relaxed">
                {{ m.auth_reset_guide_body() }}<br />
                <code class="font-mono text-ink-soft select-all">refract-server --reset-admin</code>
              </p>
            </div>
          </form>
        </DialogContent>
      </DialogPortal>
    </DialogRoot>

    <!-- 全局轻量通知容器 -->
    <GlassToastContainer />
  </div>
</template>
