<script setup lang="ts">
import { reactive, ref } from 'vue'
import { RouterLink } from 'vue-router'
import { useAction } from '@/composables/useAction'
import * as m from '@/paraglide/messages'
import { auth } from '@/api/client'

const email = ref('')
const password = ref('')
const displayName = ref('')
const code = ref('')
const step = ref<'form' | 'verify'>('form')
const action = reactive(useAction(m.auth_login_failed(), { toast: true }))

async function submit() {
  await action.run(
    () =>
      auth.register({
        email: email.value.trim(),
        password: password.value,
        display_name: displayName.value.trim() || undefined,
      }),
    () => {
      step.value = 'verify'
      return m.auth_register_sent()
    },
  )
}

async function verify() {
  await action.run(
    () => auth.verifyEmail({ email: email.value.trim(), code: code.value.trim() }),
    () => m.auth_verify_done(),
  )
}
</script>

<template>
  <div class="mx-auto mt-16 max-w-md">
    <div class="glass-thick glass-specular p-6">
      <h1 class="text-xl font-semibold">{{ m.register_title() }}</h1>
      <p class="mt-1 text-sm text-ink-faint">{{ m.register_subtitle() }}</p>

      <form v-if="step === 'form'" class="mt-5 flex flex-col gap-3" @submit.prevent="submit">
        <input
          v-model="email"
          type="email"
          required
          :placeholder="m.auth_email()"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
        <input
          v-model="displayName"
          type="text"
          :placeholder="m.auth_display_name()"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
        <input
          v-model="password"
          type="password"
          required
          minlength="10"
          :placeholder="m.auth_password()"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
        <p class="text-[0.7rem] text-ink-faint">{{ m.auth_password_hint() }}</p>
        <p v-if="action.error" class="text-xs text-danger">{{ action.error }}</p>
        <button
          type="submit"
          class="glass-button-primary px-4 py-2.5 text-sm"
          :disabled="action.busy"
        >
          {{ action.busy ? m.auth_registering() : m.auth_register_btn() }}
        </button>
      </form>

      <form v-else class="mt-5 flex flex-col gap-3" @submit.prevent="verify">
        <input
          v-model="code"
          type="text"
          inputmode="numeric"
          maxlength="6"
          :placeholder="m.auth_code_placeholder()"
          class="glass-field px-3 py-2 font-mono text-sm outline-none"
        />
        <p v-if="action.error" class="text-xs text-danger">{{ action.error }}</p>
        <button
          type="submit"
          class="glass-button-primary px-4 py-2.5 text-sm"
          :disabled="action.busy"
        >
          {{ m.auth_verify_btn() }}
        </button>
      </form>

      <p class="mt-4 text-sm text-ink-faint">
        <RouterLink to="/" class="text-ink-soft no-underline hover:text-ink">{{
          m.register_go_login()
        }}</RouterLink>
      </p>
    </div>
  </div>
</template>
