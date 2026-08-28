<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, toRef } from 'vue'
import { RouterLink, RouterView, useRoute } from 'vue-router'
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
import { applyLogin, isRestricted, refreshSession, session } from '@/stores/session'
import { initLiquidGlass } from '@/utils/liquidGlass'
import { AUTH_REQUIRED_EVENT, BACKEND_DOWN_EVENT, BACKEND_RESTORED_EVENT, auth } from '@/api/client'
import { CRYPTO_PLAINTEXT_EVENT } from '@/api/crypto'
import type { IconName } from '@/components/AppIcon.vue'

const THEME_KEY = 'refract.theme'
const route = useRoute()
const isPublic = computed(() => Boolean(route.meta.public))

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
  initLiquidGlass()
  const saved = localStorage.getItem(THEME_KEY)
  const dark =
    saved === 'dark' ||
    (saved === null && window.matchMedia('(prefers-color-scheme: dark)').matches)
  applyTheme(dark)

  await refreshSession()
  if (authGate.needsLogin || (session.configured && !session.authenticated)) {
    tokenDialogOpen.value = true
  }
})

const tokenDialogOpen = ref(false)
const authTab = ref<'login' | 'register' | 'reset'>('login')
const emailDraft = ref('')
const passwordDraft = ref('')
const displayNameDraft = ref('')
const tokenDraft = ref('')
const showToken = ref(false)
const showPassword = ref(false)
const tokenSectionOpen = ref(false)
const resetStep = ref<'request' | 'confirm'>('request')
const resetCode = ref('')
const resetPassword = ref('')
const login = useAction(m.auth_login_failed())
const tokenError = toRef(login, 'error')
const tokenBusy = toRef(login, 'busy')

const verifyCode = ref('')
const verifyAction = useAction(m.auth_verify_failed())

function onTokenToggle(event: Event) {
  const el = event.target
  tokenSectionOpen.value = el instanceof HTMLDetailsElement && el.open
}
function onAuthRequired() {
  if (!tokenDialogOpen.value && !isPublic.value) {
    tokenDraft.value = ''
    emailDraft.value = ''
    passwordDraft.value = ''
    login.clear()
    authTab.value = 'login'
    tokenDialogOpen.value = true
  }
}

async function afterAuthSuccess() {
  tokenDraft.value = ''
  passwordDraft.value = ''
  tokenDialogOpen.value = false
  window.location.reload()
}

async function saveTokenAndReload() {
  const token = tokenDraft.value.trim()
  if (!token || tokenBusy.value) return
  await login.run(
    () => auth.login({ token }),
    (result) => {
      applyLogin(result)
      afterAuthSuccess()
    },
  )
}

async function loginWithPassword() {
  const email = emailDraft.value.trim()
  const password = passwordDraft.value
  if (!email || !password || tokenBusy.value) return
  await login.run(
    () => auth.login({ email, password }),
    (result) => {
      applyLogin(result)
      afterAuthSuccess()
    },
  )
}

async function submitRegister() {
  const email = emailDraft.value.trim()
  const password = passwordDraft.value
  if (!email || !password || tokenBusy.value) return
  await login.run(
    () =>
      auth.register({
        email,
        password,
        display_name: displayNameDraft.value.trim() || undefined,
      }),
    () => {
      authTab.value = 'login'
      return m.auth_register_sent()
    },
  )
}

async function submitResetRequest() {
  const email = emailDraft.value.trim()
  if (!email || tokenBusy.value) return
  await login.run(
    () => auth.requestPasswordReset(email),
    () => {
      resetStep.value = 'confirm'
      return m.auth_reset_sent()
    },
  )
}

async function submitResetConfirm() {
  const email = emailDraft.value.trim()
  if (!email || !resetCode.value.trim() || !resetPassword.value || tokenBusy.value) return
  await login.run(
    () =>
      auth.confirmPasswordReset({
        email,
        code: resetCode.value.trim(),
        new_password: resetPassword.value,
      }),
    () => {
      authTab.value = 'login'
      resetStep.value = 'request'
      return m.auth_reset_done()
    },
  )
}

async function submitVerify() {
  const email = session.user?.email ?? emailDraft.value.trim()
  if (!email || !verifyCode.value.trim()) return
  await verifyAction.run(
    async () => {
      const result = await auth.verifyEmail({ email, code: verifyCode.value.trim() })
      await refreshSession()
      verifyCode.value = ''
      return result
    },
    () => m.auth_verify_done(),
  )
}

async function resendVerify() {
  const email = session.user?.email ?? emailDraft.value.trim()
  if (!email) return
  await verifyAction.run(
    () => auth.resendVerification(email),
    () => m.auth_resend_sent(),
  )
}

onMounted(() => window.addEventListener(AUTH_REQUIRED_EVENT, onAuthRequired))
onBeforeUnmount(() => window.removeEventListener(AUTH_REQUIRED_EVENT, onAuthRequired))

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

interface NavItem {
  name: string
  label: string
  icon: IconName
}

const adminNavItems = computed<NavItem[]>(() => [
  { name: 'admin-dashboard', label: m.nav_dashboard(), icon: 'gauge' },
  { name: 'admin-channels', label: m.nav_channels(), icon: 'channels' },
  { name: 'admin-models', label: m.nav_models(), icon: 'boxes' },
  { name: 'admin-users', label: m.nav_users(), icon: 'users' },
  { name: 'admin-playground', label: m.nav_playground(), icon: 'chat' },
  { name: 'admin-logs', label: m.nav_logs(), icon: 'logs' },
])

const selfNavItems = computed<NavItem[]>(() => {
  if (session.user?.role === 'admin') {
    return [
      { name: 'dashboard', label: m.nav_my_usage(), icon: 'gauge' },
      { name: 'keys', label: m.nav_keys(), icon: 'key' },
      { name: 'channels', label: m.nav_my_channels(), icon: 'channels' },
      { name: 'logs', label: m.nav_my_logs(), icon: 'logs' },
      { name: 'wallet', label: m.nav_wallet(), icon: 'wallet' },
      { name: 'profile', label: m.nav_profile(), icon: 'user' },
    ]
  }
  return [
    { name: 'dashboard', label: m.nav_dashboard(), icon: 'gauge' },
    { name: 'channels', label: m.nav_channels(), icon: 'channels' },
    { name: 'keys', label: m.nav_keys(), icon: 'key' },
    { name: 'logs', label: m.nav_logs(), icon: 'logs' },
    { name: 'wallet', label: m.nav_wallet(), icon: 'wallet' },
    { name: 'profile', label: m.nav_profile(), icon: 'user' },
  ]
})

const navItems = computed(() =>
  session.user?.role === 'admin' ? adminNavItems.value : selfNavItems.value,
)

const mobileNavItems = computed<NavItem[]>(() => {
  if (session.user?.role === 'admin') {
    return [
      { name: 'admin-dashboard', label: m.nav_dashboard(), icon: 'gauge' },
      { name: 'admin-channels', label: m.nav_channels(), icon: 'channels' },
      { name: 'admin-logs', label: m.nav_logs(), icon: 'logs' },
      { name: 'keys', label: m.nav_keys(), icon: 'key' },
      { name: 'admin-settings', label: m.nav_settings(), icon: 'settings' },
    ]
  }
  return [
    { name: 'dashboard', label: m.nav_dashboard(), icon: 'gauge' },
    { name: 'channels', label: m.nav_channels(), icon: 'channels' },
    { name: 'logs', label: m.nav_logs(), icon: 'logs' },
    { name: 'keys', label: m.nav_keys(), icon: 'key' },
    { name: 'wallet', label: m.nav_wallet(), icon: 'wallet' },
  ]
})

const homeRoute = computed(() =>
  session.user?.role === 'admin' ? { name: 'admin-dashboard' } : { name: 'dashboard' },
)

const version = __APP_VERSION__
</script>

<template>
  <div class="flex min-h-screen text-ink">
    <div class="canvas-aurora" aria-hidden="true"></div>

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

    <Transition
      enter-active-class="transition-opacity duration-300"
      enter-from-class="opacity-0"
      leave-active-class="transition-opacity duration-300"
      leave-to-class="opacity-0"
    >
      <div
        v-if="isRestricted && session.authenticated && !isPublic"
        class="glass fixed top-20 left-1/2 z-40 flex max-w-[calc(100%-2rem)] -translate-x-1/2 flex-col gap-2 border-warning/30 bg-warning/12 px-4 py-2.5 text-sm font-medium text-ink shadow-[0_12px_32px_-14px_oklch(0%_0_0/0.28)] md:top-3"
        role="status"
      >
        <div class="flex items-center gap-2.5">
          <AppIcon name="warning" class="text-warning shrink-0" :size="15" />
          <span>{{ m.auth_verify_banner() }}</span>
        </div>
        <form class="flex flex-wrap items-center gap-2" @submit.prevent="submitVerify">
          <input
            v-model="verifyCode"
            type="text"
            inputmode="numeric"
            maxlength="6"
            :placeholder="m.auth_code_placeholder()"
            class="glass-field w-28 px-2 py-1.5 font-mono text-sm outline-none"
          />
          <button type="submit" class="glass-button-primary px-3 py-1.5 text-xs font-medium">
            {{ m.auth_verify_btn() }}
          </button>
          <button
            type="button"
            class="rounded-lg px-2 py-1.5 text-xs text-ink-soft hover:text-ink cursor-pointer"
            @click="resendVerify"
          >
            {{ m.auth_resend_btn() }}
          </button>
        </form>
        <p v-if="verifyAction.error" class="text-xs text-danger">{{ verifyAction.error }}</p>
      </div>
    </Transition>

    <template v-if="!isPublic">
      <header
        class="glass-thick mobile-topbar fixed inset-x-3 top-3 z-30 flex h-14 items-center justify-between px-3 md:hidden"
      >
        <RouterLink :to="homeRoute" class="flex items-center gap-2 no-underline">
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

      <aside
        class="fixed top-0 left-0 z-20 hidden h-screen w-[240px] shrink-0 p-3 md:block xl:w-[264px]"
      >
        <nav
          class="glass-thick glass-specular flex h-full flex-col overflow-y-auto px-4 py-5"
          :aria-label="m.nav_main_nav()"
        >
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

          <RouterLink :to="homeRoute" class="mb-6 flex items-center gap-3 px-2 no-underline">
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

          <template v-if="session.user?.role === 'admin'">
            <p
              class="mb-1 px-3 text-[0.65rem] font-semibold uppercase tracking-wide text-ink-faint"
            >
              {{ m.nav_group_admin() }}
            </p>
            <ul class="mb-4 flex list-none flex-col gap-1 p-0">
              <li v-for="item in adminNavItems" :key="item.name">
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
            <p
              class="mb-1 px-3 text-[0.65rem] font-semibold uppercase tracking-wide text-ink-faint"
            >
              {{ m.nav_group_self() }}
            </p>
            <ul class="flex list-none flex-col gap-1 p-0">
              <li v-for="item in selfNavItems" :key="item.name">
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
          </template>
          <ul v-else class="flex list-none flex-col gap-1 p-0">
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

          <div class="mt-auto flex flex-col gap-1 pt-5">
            <RouterLink
              v-if="session.user?.role === 'admin'"
              :to="{ name: 'admin-settings' }"
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
              <svg
                class="size-3 shrink-0"
                viewBox="0 0 24 24"
                fill="currentColor"
                aria-hidden="true"
              >
                <path
                  d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12"
                />
              </svg>
              v{{ version }}
            </a>
          </div>
        </nav>
      </aside>
    </template>

    <main
      class="min-h-screen min-w-0 flex-1 px-4 pt-24 pb-28 sm:px-5 md:p-5 xl:p-6"
      :class="[isPublic ? '' : 'md:ml-[240px] xl:ml-[264px]', backendDown ? 'max-md:pt-40' : '']"
    >
      <RouterView />
    </main>

    <nav
      v-if="!isPublic"
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

          <div class="mt-4 flex gap-1 rounded-lg bg-ink/6 p-1">
            <button
              v-for="tab in ['login', 'register', 'reset'] as const"
              :key="tab"
              type="button"
              class="flex-1 rounded-md px-2 py-1.5 text-xs font-medium cursor-pointer"
              :class="authTab === tab ? 'bg-surface text-ink shadow-xs' : 'text-ink-soft'"
              @click="authTab = tab"
            >
              {{
                tab === 'login'
                  ? m.auth_tab_login()
                  : tab === 'register'
                    ? m.auth_tab_register()
                    : m.auth_tab_reset()
              }}
            </button>
          </div>

          <form
            v-if="authTab === 'login'"
            class="mt-5 flex flex-col gap-4"
            @submit.prevent="
              tokenSectionOpen && tokenDraft.trim() ? saveTokenAndReload() : loginWithPassword()
            "
          >
            <div v-if="tokenError" class="rounded-lg bg-danger/10 p-3 text-xs text-danger">
              {{ tokenError }}
            </div>
            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                m.auth_email()
              }}</label>
              <input
                v-model="emailDraft"
                type="email"
                autocomplete="username"
                class="glass-field w-full px-3 py-2 text-sm outline-none"
              />
            </div>
            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                m.auth_password()
              }}</label>
              <div class="relative">
                <input
                  v-model="passwordDraft"
                  :type="showPassword ? 'text' : 'password'"
                  autocomplete="current-password"
                  class="glass-field w-full px-3 py-2 pr-16 text-sm outline-none"
                />
                <button
                  type="button"
                  class="absolute top-1/2 right-2 -translate-y-1/2 rounded-md px-2 py-1 text-xs text-ink-faint hover:text-ink"
                  @click="showPassword = !showPassword"
                >
                  {{ showPassword ? m.common_hide() : m.common_show() }}
                </button>
              </div>
            </div>
            <button
              type="submit"
              class="glass-button-primary px-4 py-2.5 text-sm font-medium disabled:opacity-50"
              :disabled="
                ((!emailDraft.trim() || !passwordDraft) && !tokenDraft.trim()) || tokenBusy
              "
            >
              <AppIcon v-if="tokenBusy" name="spinner" class="animate-spin mr-1" :size="14" />
              {{ tokenBusy ? m.auth_logging_in() : m.auth_login_btn() }}
            </button>

            <details class="rounded-lg border border-ink/8 bg-ink/5 p-3" @toggle="onTokenToggle">
              <summary class="cursor-pointer text-xs font-medium text-ink-soft">
                {{ m.auth_admin_token() }}
              </summary>
              <div class="relative mt-3">
                <input
                  v-model="tokenDraft"
                  :type="showToken ? 'text' : 'password'"
                  :placeholder="m.auth_token_placeholder()"
                  autocomplete="off"
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
              <p class="mt-2 text-xs text-ink-faint leading-relaxed">
                {{ m.auth_reset_guide_body() }}<br />
                <code class="font-mono text-ink-soft select-all">refract-server --reset-admin</code>
              </p>
            </details>
          </form>

          <form
            v-else-if="authTab === 'register'"
            class="mt-5 flex flex-col gap-4"
            @submit.prevent="submitRegister"
          >
            <div v-if="tokenError" class="rounded-lg bg-danger/10 p-3 text-xs text-danger">
              {{ tokenError }}
            </div>
            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                m.auth_email()
              }}</label>
              <input
                v-model="emailDraft"
                type="email"
                required
                class="glass-field w-full px-3 py-2 text-sm outline-none"
              />
            </div>
            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                m.auth_display_name()
              }}</label>
              <input
                v-model="displayNameDraft"
                type="text"
                class="glass-field w-full px-3 py-2 text-sm outline-none"
              />
            </div>
            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                m.auth_password()
              }}</label>
              <input
                v-model="passwordDraft"
                type="password"
                required
                minlength="10"
                class="glass-field w-full px-3 py-2 text-sm outline-none"
              />
              <p class="mt-1 text-[0.7rem] text-ink-faint">{{ m.auth_password_hint() }}</p>
            </div>
            <button
              type="submit"
              class="glass-button-primary px-4 py-2.5 text-sm font-medium disabled:opacity-50"
              :disabled="!emailDraft.trim() || !passwordDraft || tokenBusy"
            >
              {{ tokenBusy ? m.auth_registering() : m.auth_register_btn() }}
            </button>
          </form>

          <form
            v-else
            class="mt-5 flex flex-col gap-4"
            @submit.prevent="resetStep === 'request' ? submitResetRequest() : submitResetConfirm()"
          >
            <div v-if="tokenError" class="rounded-lg bg-danger/10 p-3 text-xs text-danger">
              {{ tokenError }}
            </div>
            <div>
              <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                m.auth_email()
              }}</label>
              <input
                v-model="emailDraft"
                type="email"
                required
                class="glass-field w-full px-3 py-2 text-sm outline-none"
              />
            </div>
            <template v-if="resetStep === 'confirm'">
              <div>
                <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                  m.auth_code()
                }}</label>
                <input
                  v-model="resetCode"
                  type="text"
                  inputmode="numeric"
                  maxlength="6"
                  class="glass-field w-full px-3 py-2 font-mono text-sm outline-none"
                />
              </div>
              <div>
                <label class="mb-1 block text-xs font-medium text-ink-soft">{{
                  m.auth_new_password()
                }}</label>
                <input
                  v-model="resetPassword"
                  type="password"
                  minlength="10"
                  class="glass-field w-full px-3 py-2 text-sm outline-none"
                />
              </div>
            </template>
            <button
              type="submit"
              class="glass-button-primary px-4 py-2.5 text-sm font-medium disabled:opacity-50"
              :disabled="tokenBusy"
            >
              {{ resetStep === 'request' ? m.auth_reset_send() : m.auth_reset_confirm() }}
            </button>
          </form>
        </DialogContent>
      </DialogPortal>
    </DialogRoot>

    <GlassToastContainer />
  </div>
</template>
