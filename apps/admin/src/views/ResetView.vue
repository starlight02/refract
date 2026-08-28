<script setup lang="ts">
import { reactive, ref } from 'vue'
import { RouterLink } from 'vue-router'
import { useAction } from '@/composables/useAction'
import * as m from '@/paraglide/messages'
import { auth } from '@/api/client'

const email = ref('')
const code = ref('')
const password = ref('')
const step = ref<'request' | 'confirm'>('request')
const action = reactive(useAction(m.auth_login_failed(), { toast: true }))

async function requestCode() {
  await action.run(
    () => auth.requestPasswordReset(email.value.trim()),
    () => {
      step.value = 'confirm'
      return m.auth_reset_sent()
    },
  )
}

async function confirm() {
  await action.run(
    () =>
      auth.confirmPasswordReset({
        email: email.value.trim(),
        code: code.value.trim(),
        new_password: password.value,
      }),
    () => m.auth_reset_done(),
  )
}
</script>

<template>
  <div class="mx-auto mt-16 max-w-md">
    <div class="glass-thick glass-specular p-6">
      <h1 class="text-xl font-semibold">{{ m.reset_title() }}</h1>
      <p class="mt-1 text-sm text-ink-faint">{{ m.reset_subtitle() }}</p>

      <form
        class="mt-5 flex flex-col gap-3"
        @submit.prevent="step === 'request' ? requestCode() : confirm()"
      >
        <input
          v-model="email"
          type="email"
          required
          :placeholder="m.auth_email()"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
        <template v-if="step === 'confirm'">
          <input
            v-model="code"
            type="text"
            inputmode="numeric"
            maxlength="6"
            :placeholder="m.auth_code_placeholder()"
            class="glass-field px-3 py-2 font-mono text-sm outline-none"
          />
          <input
            v-model="password"
            type="password"
            required
            minlength="10"
            :placeholder="m.auth_new_password()"
            class="glass-field px-3 py-2 text-sm outline-none"
          />
        </template>
        <p v-if="action.error" class="text-xs text-danger">{{ action.error }}</p>
        <button
          type="submit"
          class="glass-button-primary px-4 py-2.5 text-sm"
          :disabled="action.busy"
        >
          {{ step === 'request' ? m.auth_reset_send() : m.auth_reset_confirm() }}
        </button>
      </form>

      <p class="mt-4 text-sm text-ink-faint">
        <RouterLink to="/" class="text-ink-soft no-underline hover:text-ink">{{
          m.reset_go_login()
        }}</RouterLink>
      </p>
    </div>
  </div>
</template>
