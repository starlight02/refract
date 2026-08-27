<script setup lang="ts">
/**
 * 通知与自愈：webhook、签名密钥、自动禁用渠道的重测间隔。
 */
import { ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import { useAction } from '@/composables/useAction'
import { settings } from '@/api/client'
import type { NotifySettings } from '@refract/contracts'

const notify = defineModel<NotifySettings>({ required: true })
const webhookSecretConfigured = defineModel<boolean>('secretConfigured', { required: true })

defineProps<{
  loadError?: string | null
  secretLoadError?: string | null
  valid: boolean
  dirty: boolean
}>()

const emit = defineEmits<{
  retry: []
  retrySecret: []
}>()

const testNotify = useAction('发送失败')
const webhookSecretDraft = ref('')
const showWebhookSecret = ref(false)
const saveWebhookSecret = useAction('保存失败', { toast: true })

async function sendTestNotification() {
  await testNotify.run(
    () => settings.testNotify(),
    () => '已发送 —— 去通知渠道确认收到',
  )
}

async function applyWebhookSecret() {
  if (saveWebhookSecret.busy) return
  const secret = webhookSecretDraft.value.trim()
  await saveWebhookSecret.run(
    () => settings.setWebhookSecret(secret || null),
    (res) => {
      webhookSecretConfigured.value = res.configured
      webhookSecretDraft.value = ''
      return secret ? '签名密钥已保存，webhook 请求将携带签名头。' : '签名密钥已清除。'
    },
  )
}
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <div>
      <SettingsSectionError :message="loadError" @retry="emit('retry')" />
      <h2 class="text-sm font-semibold text-ink-soft uppercase">通知与自愈</h2>
      <p class="mt-1 text-xs text-ink-faint">
        熔断、恢复、自动禁用事件推送到 webhook（通用 JSON，可对接 Server酱 / 飞书 / Telegram
        网桥）。连续 3 次凭据错误的渠道会被自动停用， 并按设定间隔重测，通过即自动恢复。
      </p>
    </div>

    <label class="flex flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">Webhook 地址</span>
      <div class="flex items-center gap-2">
        <input
          v-model="notify.webhook_url"
          type="url"
          placeholder="https://example.com/hook（留空关闭通知）"
          aria-label="告警 webhook 地址"
          class="glass-field w-full max-w-xl px-3 py-2 font-mono text-sm outline-none"
        />
        <button
          type="button"
          class="glass-button-ghost shrink-0 px-3 py-2 text-sm"
          :disabled="testNotify.busy || dirty || !notify.webhook_url"
          :title="dirty ? '先保存设置再测试' : '发送一条测试通知'"
          @click="sendTestNotification"
        >
          <AppIcon v-if="testNotify.busy" name="spinner" class="animate-spin mr-1" :size="13" />
          {{ testNotify.busy ? '发送中…' : '发送测试' }}
        </button>
      </div>
      <span v-if="testNotify.notice?.text" class="text-xs text-ink-soft">
        {{ testNotify.notice?.text }}
      </span>
    </label>

    <SettingsSectionError :message="secretLoadError" @retry="emit('retrySecret')" />
    <!-- Webhook 签名密钥 -->
    <div class="flex max-w-xl flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">
        Webhook 签名密钥
        <span v-if="webhookSecretConfigured" class="ml-2 font-normal text-success">
          已配置（留空保存则清除）
        </span>
        <span v-else class="ml-2 font-normal text-ink-faint">未配置</span>
      </span>
      <div class="relative">
        <input
          v-model="webhookSecretDraft"
          :type="showWebhookSecret ? 'text' : 'password'"
          :placeholder="
            webhookSecretConfigured ? '新签名密钥；留空保存即清除' : '签名密钥（留空不签名）'
          "
          autocomplete="new-password"
          aria-label="Webhook 签名密钥"
          class="glass-field w-full px-3 py-2 pr-16 font-mono text-sm outline-none"
        />
        <button
          type="button"
          class="absolute top-1/2 right-2 -translate-y-1/2 rounded-md px-2 py-1 text-xs text-ink-faint hover:text-ink"
          :aria-label="showWebhookSecret ? '隐藏签名密钥' : '显示签名密钥'"
          :aria-pressed="showWebhookSecret"
          @click="showWebhookSecret = !showWebhookSecret"
        >
          {{ showWebhookSecret ? '隐藏' : '显示' }}
        </button>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <button
          type="button"
          class="glass-button-ghost px-3 py-1.5 text-xs disabled:opacity-50"
          :disabled="
            saveWebhookSecret.busy || (!webhookSecretConfigured && !webhookSecretDraft.trim())
          "
          @click="applyWebhookSecret"
        >
          <AppIcon
            v-if="saveWebhookSecret.busy"
            name="spinner"
            class="animate-spin mr-1"
            :size="13"
          />
          {{ saveWebhookSecret.busy ? '保存中…' : '保存签名密钥' }}
        </button>
      </div>
      <p class="text-xs text-ink-faint">
        配置后 webhook 请求携带 X-Refract-Signature 头（HMAC-SHA256 签名），接收端可验证来源。
      </p>
    </div>

    <label class="flex max-w-sm flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">自动禁用渠道的重测间隔</span>
      <div class="flex items-center gap-2">
        <input
          v-model.number="notify.retest_minutes"
          type="number"
          min="0"
          max="1440"
          step="1"
          inputmode="numeric"
          aria-label="重测间隔分钟数"
          class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
        />
        <span class="text-sm text-ink-faint">分钟，0 关闭自愈</span>
      </div>
    </label>

    <p v-if="!valid" class="text-xs text-danger" role="alert">
      webhook 需以 http(s):// 开头；重测间隔 0–1440 分钟。
    </p>
  </section>
</template>
