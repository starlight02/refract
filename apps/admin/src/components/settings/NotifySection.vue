<script setup lang="ts">
/**
 * 通知与自愈：webhook、签名密钥、自动禁用渠道的重测间隔。
 */
import { ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import * as m from '@/paraglide/messages'
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

const testNotify = useAction(m.settings_notify_test_failed())
const webhookSecretDraft = ref('')
const showWebhookSecret = ref(false)
const saveWebhookSecret = useAction(m.settings_save_failed(), { toast: true })

async function sendTestNotification() {
  await testNotify.run(
    () => settings.testNotify(),
    () => m.settings_notify_test_sent(),
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
      return secret ? m.settings_notify_secret_saved() : m.settings_notify_secret_cleared()
    },
  )
}
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <div>
      <SettingsSectionError :message="loadError" @retry="emit('retry')" />
      <h2 class="text-sm font-semibold text-ink-soft uppercase">{{ m.settings_notify_title() }}</h2>
      <p class="mt-1 text-xs text-ink-faint">
        {{ m.settings_notify_desc() }}
      </p>
    </div>

    <label class="flex flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">{{ m.settings_notify_webhook_url() }}</span>
      <div class="flex items-center gap-2">
        <input
          v-model="notify.webhook_url"
          type="url"
          :placeholder="m.settings_notify_webhook_placeholder()"
          :aria-label="m.settings_notify_webhook_aria()"
          class="glass-field w-full max-w-xl px-3 py-2 font-mono text-sm outline-none"
        />
        <button
          type="button"
          class="glass-button-ghost shrink-0 px-3 py-2 text-sm"
          :disabled="testNotify.busy || dirty || !notify.webhook_url"
          :title="dirty ? m.settings_notify_test_dirty_hint() : m.settings_notify_test_title()"
          @click="sendTestNotification"
        >
          <AppIcon v-if="testNotify.busy" name="spinner" class="animate-spin mr-1" :size="13" />
          {{ testNotify.busy ? m.common_sending() : m.settings_notify_send_test() }}
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
        {{ m.settings_notify_secret_title() }}
        <span v-if="webhookSecretConfigured" class="ml-2 font-normal text-success">
          {{ m.settings_notify_secret_configured() }}
        </span>
        <span v-else class="ml-2 font-normal text-ink-faint">{{
          m.settings_notify_secret_not_configured()
        }}</span>
      </span>
      <div class="relative">
        <input
          v-model="webhookSecretDraft"
          :type="showWebhookSecret ? 'text' : 'password'"
          :placeholder="
            webhookSecretConfigured
              ? m.settings_notify_secret_configured_ph()
              : m.settings_notify_secret_unconfigured_ph()
          "
          autocomplete="new-password"
          :aria-label="m.settings_notify_secret_aria()"
          class="glass-field w-full px-3 py-2 pr-16 font-mono text-sm outline-none"
        />
        <button
          type="button"
          class="absolute top-1/2 right-2 -translate-y-1/2 rounded-md px-2 py-1 text-xs text-ink-faint hover:text-ink"
          :aria-label="
            showWebhookSecret ? m.settings_notify_hide_secret() : m.settings_notify_show_secret()
          "
          :aria-pressed="showWebhookSecret"
          @click="showWebhookSecret = !showWebhookSecret"
        >
          {{ showWebhookSecret ? m.common_hide() : m.common_show() }}
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
          {{ saveWebhookSecret.busy ? m.common_saving() : m.settings_notify_save_secret() }}
        </button>
      </div>
      <p class="text-xs text-ink-faint">
        {{ m.settings_notify_secret_desc() }}
      </p>
    </div>

    <label class="flex max-w-sm flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">{{ m.settings_notify_retest_title() }}</span>
      <div class="flex items-center gap-2">
        <input
          v-model.number="notify.retest_minutes"
          type="number"
          min="0"
          max="1440"
          step="1"
          inputmode="numeric"
          :aria-label="m.settings_notify_retest_aria()"
          class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
        />
        <span class="text-sm text-ink-faint">{{ m.settings_notify_retest_hint() }}</span>
      </div>
    </label>

    <p v-if="!valid" class="text-xs text-danger" role="alert">
      {{ m.settings_notify_val_err() }}
    </p>
  </section>
</template>
