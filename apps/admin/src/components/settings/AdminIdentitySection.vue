<script setup lang="ts">
/**
 * 管理身份：轮换/关闭管理令牌、退出会话。
 */
import { computed, onMounted, ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import { useAction } from '@/composables/useAction'
import { auth as authApi, settings } from '@/api/client'
import { orElse } from '@/utils/effect'

defineProps<{
  loadError?: string | null
}>()

const emit = defineEmits<{
  retry: []
}>()

const sessionActive = ref(true)
const tokenDraft = ref('')
const showTokenDraft = ref(false)
const applyAdminToken = useAction('设置失败', { toast: true })
const clearAdminToken = useAction('关闭失败', { toast: true })
const logoutSession = useAction('退出失败', { toast: true })
const tokenBusy = computed(() => applyAdminToken.busy || clearAdminToken.busy || logoutSession.busy)

onMounted(async () => {
  const s = await orElse(() => authApi.session())
  if (s) sessionActive.value = s.authenticated
})

/**
 * 启用或更换服务端令牌。
 *
 * 服务端在设置成功后会直接下发最新 Session Cookie，当前浏览器自动保持登录。
 */
async function applyToken() {
  const token = tokenDraft.value.trim()
  if (!token || tokenBusy.value) return
  await applyAdminToken.run(
    () => settings.setAdminToken(token),
    () => {
      sessionActive.value = true
      tokenDraft.value = ''
      return '令牌已生效，会话已更新。'
    },
  )
}

/** 关闭管理鉴权：服务端清除令牌哈希与 Cookie。 */
async function clearToken() {
  if (tokenBusy.value) return
  await clearAdminToken.run(
    () => settings.setAdminToken(null),
    () => {
      sessionActive.value = true
      return '管理鉴权已关闭。'
    },
  )
}

/** 登出当前控制台会话 */
async function logout() {
  if (tokenBusy.value) return
  await logoutSession.run(
    () => authApi.logout(),
    () => {
      window.location.reload()
    },
  )
}
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <div>
      <h2 class="text-sm font-semibold text-ink-soft uppercase">管理身份与令牌</h2>
      <p class="mt-1 text-xs text-ink-faint">
        默认管理员账号为
        <code class="font-mono text-ink-soft">admin@localhost</code>。启用后管理界面与 /api
        的所有请求都需要携带该令牌。服务端只保存哈希，令牌本身无法读回。首次凭据保存在数据目录的
        <code class="font-mono text-ink-soft">.admin_token</code> 文件中，10 分钟后自动删除；
        若令牌丢失或文件过期，必须在宿主机或容器内使用
        <code class="font-mono text-ink-soft">refract-server --reset-admin</code> 重启实例。
      </p>
    </div>
    <p class="text-xs text-ink-faint">
      会话通过安全 HttpOnly Cookie 维护，浏览器不持久化任何明文令牌。
    </p>
    <div class="relative">
      <input
        v-model="tokenDraft"
        :type="showTokenDraft ? 'text' : 'password'"
        placeholder="新令牌（启用或更换）"
        autocomplete="new-password"
        aria-label="新管理令牌"
        class="glass-field w-full px-3 py-2 pr-16 font-mono text-sm outline-none"
      />
      <button
        type="button"
        class="absolute top-1/2 right-2 -translate-y-1/2 rounded-md px-2 py-1 text-xs text-ink-faint hover:text-ink"
        :aria-label="showTokenDraft ? '隐藏管理令牌' : '显示管理令牌'"
        :aria-pressed="showTokenDraft"
        @click="showTokenDraft = !showTokenDraft"
      >
        {{ showTokenDraft ? '隐藏' : '显示' }}
      </button>
    </div>

    <div class="flex flex-wrap items-center gap-3">
      <button
        type="button"
        class="glass-button-primary px-4 py-2 text-sm font-medium disabled:opacity-50"
        :disabled="tokenBusy || !tokenDraft.trim()"
        @click="applyToken"
      >
        <AppIcon v-if="tokenBusy" name="spinner" class="animate-spin mr-1" :size="14" />
        {{ tokenBusy ? '处理中…' : '启用或更换' }}
      </button>
      <button
        type="button"
        class="glass-button-ghost glass-button-ghost-danger px-4 py-2 text-sm"
        :disabled="tokenBusy"
        @click="clearToken"
      >
        <AppIcon v-if="tokenBusy" name="spinner" class="animate-spin mr-1" :size="14" />
        {{ tokenBusy ? '关闭中…' : '关闭管理鉴权' }}
      </button>
      <button
        type="button"
        class="glass-button-ghost px-4 py-2 text-sm text-ink-soft hover:text-ink"
        :disabled="tokenBusy"
        @click="logout"
      >
        退出登录
      </button>
    </div>
  </section>
</template>
