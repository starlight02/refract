<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useAction } from '@/composables/useAction'
import * as m from '@/paraglide/messages'
import { auth, me } from '@/api/client'
import { applyLogin, clearSession, session } from '@/stores/session'
import type { User } from '@refract/contracts'

const router = useRouter()
const profile = ref<User | null>(null)
const displayName = ref('')
const oldPassword = ref('')
const newPassword = ref('')
const saveName = reactive(useAction(m.profile_save_failed(), { toast: true }))
const savePassword = reactive(useAction(m.profile_save_failed(), { toast: true }))

onMounted(async () => {
  const loaded = await me.profile()
  profile.value = loaded
  displayName.value = loaded.display_name
})

async function submitName() {
  await saveName.run(
    () => me.updateProfile(displayName.value.trim()),
    (user) => {
      profile.value = user
      if (session.user) {
        applyLogin({
          authenticated: true,
          user: { ...session.user, display_name: user.display_name },
        })
      }
      return m.profile_saved()
    },
  )
}

async function submitPassword() {
  await savePassword.run(
    () => me.changePassword(oldPassword.value, newPassword.value),
    () => {
      oldPassword.value = ''
      newPassword.value = ''
      return m.profile_password_changed()
    },
  )
}

async function logout() {
  await auth.logout()
  clearSession()
  router.push('/')
  window.location.reload()
}
</script>

<template>
  <div class="mx-auto max-w-xl">
    <header class="mb-6">
      <h1 class="text-2xl font-semibold">{{ m.profile_title() }}</h1>
      <p class="mt-1 text-sm text-ink-faint">{{ m.profile_subtitle() }}</p>
    </header>

    <section class="glass glass-specular mb-4 p-5">
      <form class="flex flex-col gap-3" @submit.prevent="submitName">
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">{{ m.profile_email() }}</span>
          <input
            :value="profile?.email ?? session.user?.email"
            type="email"
            disabled
            class="glass-field cursor-not-allowed bg-ink/5 px-3 py-2 text-sm text-ink-faint outline-none"
          />
        </label>
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">{{ m.profile_display_name() }}</span>
          <input
            v-model="displayName"
            type="text"
            class="glass-field px-3 py-2 text-sm outline-none"
          />
        </label>
        <p v-if="saveName.error" class="text-xs text-danger">{{ saveName.error }}</p>
        <button
          type="submit"
          class="glass-button-primary self-start px-4 py-2 text-sm"
          :disabled="saveName.busy"
        >
          {{ m.profile_save_name() }}
        </button>
      </form>
    </section>

    <section class="glass glass-specular mb-4 p-5">
      <form class="flex flex-col gap-3" @submit.prevent="submitPassword">
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">{{ m.profile_old_password() }}</span>
          <input
            v-model="oldPassword"
            type="password"
            required
            class="glass-field px-3 py-2 text-sm outline-none"
          />
        </label>
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">{{ m.profile_new_password() }}</span>
          <input
            v-model="newPassword"
            type="password"
            required
            minlength="10"
            class="glass-field px-3 py-2 text-sm outline-none"
          />
          <span class="text-[0.7rem] text-ink-faint">{{ m.auth_password_hint() }}</span>
        </label>
        <p v-if="savePassword.error" class="text-xs text-danger">{{ savePassword.error }}</p>
        <button
          type="submit"
          class="glass-button-primary self-start px-4 py-2 text-sm"
          :disabled="savePassword.busy"
        >
          {{ m.profile_change_password() }}
        </button>
      </form>
    </section>

    <button type="button" class="glass-button-ghost px-4 py-2 text-sm" @click="logout">
      {{ m.profile_logout() }}
    </button>
  </div>
</template>
