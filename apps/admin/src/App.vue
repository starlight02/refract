<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { RouterLink, RouterView } from 'vue-router'
import {
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from 'reka-ui'
import {
  AUTH_REQUIRED_EVENT,
  BACKEND_DOWN_EVENT,
  BACKEND_RESTORED_EVENT,
  setAdminToken,
} from '@/api/client'
import AppIcon from '@/components/AppIcon.vue'
import GlassToastContainer from '@/components/GlassToastContainer.vue'
import { initLiquidGlass } from '@/utils/liquidGlass'

/** 深色模式偏好的存储键。 */
const THEME_KEY = 'refract.theme'

/**
 * 深色模式。真值来源是 <html> 上的 `.dark` 类名（设计系统用类名切换而非
 * media query），这里只做镜像，避免出现「ref 说亮、DOM 是暗」的错位。
 */
const isDark = ref(false)

function applyTheme(dark: boolean) {
  isDark.value = dark
  document.documentElement.classList.toggle('dark', dark)
  localStorage.setItem(THEME_KEY, dark ? 'dark' : 'light')
}

onMounted(() => {
  // 挂载物理液态玻璃滤镜
  initLiquidGlass()
  // 首次访问没有存储值时跟随系统，之后用户的显式选择优先。
  const saved = localStorage.getItem(THEME_KEY)
  const dark =
    saved === 'dark' ||
    (saved === null && window.matchMedia('(prefers-color-scheme: dark)').matches)
  applyTheme(dark)
})

/**
 * 管理令牌失效弹窗。
 *
 * 任何一个管理 API 撞见 401/403 都会派发 AUTH_REQUIRED_EVENT。弹窗是唯一
 * 的恢复入口：填入令牌 → 存进 localStorage → 重新加载，让所有页面用新令牌
 * 重新拉数据。不尝试「原地重试当前请求」—— 各页面都有自己的加载逻辑，
 * 整页重载最简单也最不容易留下半更新的状态。
 */
const tokenDialogOpen = ref(false)
const tokenDraft = ref('')
const showToken = ref(false)
const reloading = ref(false)

function onAuthRequired() {
  if (!tokenDialogOpen.value) {
    tokenDraft.value = ''
    tokenDialogOpen.value = true
  }
}

function saveTokenAndReload() {
  const token = tokenDraft.value.trim()
  if (!token || reloading.value) return
  reloading.value = true
  setAdminToken(token)
  window.location.reload()
}

onMounted(() => window.addEventListener(AUTH_REQUIRED_EVENT, onAuthRequired))
onBeforeUnmount(() => window.removeEventListener(AUTH_REQUIRED_EVENT, onAuthRequired))

/**
 * 后端不可达横幅。
 *
 * dev 下 cargo watch 重编译（几秒到几分钟）、生产下服务重启，期间所有
 * API 都会失败。客户端层已经对 GET 静默重试；这里补上体感 —— 显示一条
 * 「后端启动中」横幅并轮询 `/health/live`，恢复后自动消失，用户不需要
 * 猜「是我配置错了还是它还没起来」。
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
    try {
      const response = await fetch('/health/live')
      if (response.ok) {
        onBackendRestored()
      }
    } catch {
      // 还没起来，下一轮再探。
    }
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

/** 桌面侧栏主导航。 */
const navItems = [
  { name: 'dashboard', label: '仪表盘', icon: 'gauge' },
  { name: 'channels', label: '渠道', icon: 'channels' },
  { name: 'models', label: '模型', icon: 'boxes' },
  { name: 'playground', label: '调试台', icon: 'chat' },
  { name: 'logs', label: '请求日志', icon: 'logs' },
  { name: 'keys', label: 'API 密钥', icon: 'key' },
] as const
/** 移动端底栏只放五个高频入口 —— 模型与调试台是桌面场景。 */
const mobileNavItems = [
  { name: 'dashboard', label: '仪表盘', icon: 'gauge' },
  { name: 'channels', label: '渠道', icon: 'channels' },
  { name: 'logs', label: '请求日志', icon: 'logs' },
  { name: 'keys', label: 'API 密钥', icon: 'key' },
  { name: 'settings', label: '设置', icon: 'settings' },
] as const
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
        <span>后端启动中（编译或重启），就绪后自动恢复</span>
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
      <button
        type="button"
        class="grid size-10 place-items-center rounded-[var(--radius-control)] text-ink-soft hover:bg-ink/8 hover:text-ink"
        :aria-label="isDark ? '切换到浅色模式' : '切换到深色模式'"
        :aria-pressed="isDark"
        @click="applyTheme(!isDark)"
      >
        <AppIcon :name="isDark ? 'moon' : 'sun'" :size="18" />
      </button>
    </header>

    <!-- 侧栏：固定宽度、满高、独立滚动。
         p-3 的外距让玻璃的阴影有地方溢出，不然会被视口边缘裁掉。 -->
    <aside
      class="fixed top-0 left-0 z-20 hidden h-screen w-[240px] shrink-0 p-3 md:block xl:w-[264px]"
    >
      <nav
        class="glass-thick glass-specular flex h-full flex-col overflow-y-auto px-4 py-5"
        aria-label="主导航"
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
            <span class="text-[0.7rem] text-ink-faint">LLM 网关</span>
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
            设置
          </RouterLink>

          <button
            type="button"
            class="shape-nav flex w-full cursor-pointer items-center gap-3 border-0 bg-transparent px-3 py-2.5 text-left text-[0.875rem] font-medium text-ink-soft transition-all duration-150 hover:bg-black/5 hover:text-ink dark:hover:bg-white/10"
            @click="applyTheme(!isDark)"
          >
            <span class="grid w-4 place-items-center" aria-hidden="true">
              <AppIcon :name="isDark ? 'moon' : 'sun'" />
            </span>
            {{ isDark ? '深色' : '浅色' }}
          </button>

          <p class="tabular m-0 px-3 pt-2 text-[0.7rem] text-ink-faint">v{{ version }}</p>
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

    <nav class="mobile-tabbar fixed inset-x-3 bottom-3 z-30 md:hidden" aria-label="移动端主导航">
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
          <DialogTitle class="text-lg font-semibold">管理端身份验证</DialogTitle>
          <DialogDescription class="mt-1 text-xs text-ink-faint">
            服务端已开启管理鉴权。首次启动可在数据目录下的
            <code class="font-mono text-ink-soft">.admin_token</code>
            隐藏文件查看初始凭据（10分钟有效）。
          </DialogDescription>

          <form class="mt-5 flex flex-col gap-4" @submit.prevent="saveTokenAndReload">
            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft">管理员账号</label>
              <input
                type="text"
                value="admin@localhost"
                readonly
                disabled
                class="glass-field w-full cursor-not-allowed bg-ink/5 px-3 py-2 text-sm text-ink-faint outline-none"
              />
            </div>

            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft"
                >管理令牌 (Admin Token)</label
              >
              <div class="relative">
                <input
                  v-model="tokenDraft"
                  :type="showToken ? 'text' : 'password'"
                  placeholder="adm_... 或自定义管理令牌"
                  autocomplete="current-password"
                  autofocus
                  aria-label="管理令牌"
                  class="glass-field w-full px-3 py-2 pr-16 font-mono text-sm outline-none"
                />
                <button
                  type="button"
                  class="absolute top-1/2 right-2 -translate-y-1/2 rounded-md px-2 py-1 text-xs text-ink-faint hover:text-ink"
                  :aria-label="showToken ? '隐藏管理令牌' : '显示管理令牌'"
                  :aria-pressed="showToken"
                  @click="showToken = !showToken"
                >
                  {{ showToken ? '隐藏' : '显示' }}
                </button>
              </div>
            </div>
            <div class="flex items-center gap-3">
              <button
                type="submit"
                class="glass-button-primary px-4 py-2.5 text-sm font-medium disabled:opacity-50"
                :disabled="!tokenDraft.trim() || reloading"
              >
                <AppIcon v-if="reloading" name="spinner" class="animate-spin mr-1" :size="14" />
                {{ reloading ? '正在重新加载…' : '保存并重新加载' }}
              </button>
              <button
                type="button"
                class="rounded-lg px-3 py-2.5 text-sm text-ink-soft hover:bg-ink/8 hover:text-ink"
                @click="tokenDialogOpen = false"
              >
                取消
              </button>
            </div>

            <div class="mt-2 rounded-lg border border-ink/8 bg-ink/5 p-3 text-xs text-ink-faint">
              <div class="font-medium text-ink-soft mb-1 flex items-center gap-1.5">
                <AppIcon name="info" :size="13" />
                <span>超时或丢失令牌？如何重新初始化：</span>
              </div>
              <p class="leading-relaxed">
                在服务器或 Docker 容器中执行：<br />
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
