<script setup lang="ts">
/**
 * 渠道默认密钥池与钥匙策略。
 */
import { computed, ref } from 'vue'
import { poolCredentials } from '@/utils/channel-form'
import type { Channel, KeyStrategy } from '@refract/contracts'

const KEY_STRATEGY_OPTIONS: { value: KeyStrategy; label: string; hint: string }[] = [
  { value: 'round_robin', label: '轮询', hint: '每次请求依次换用池中的钥匙' },
  { value: 'sticky', label: '黏性', hint: '同一调用方固定用一把钥匙，出错才换' },
  { value: 'random', label: '随机', hint: '每次请求随机选一把钥匙' },
]

const form = defineModel<Channel>({ required: true })
const credentialsText = defineModel<string>('credentialsText', { required: true })

const showCredential = ref(false)
const poolLineCount = computed(() => poolCredentials(credentialsText.value).length)
</script>

<template>
  <section class="glass glass-specular p-5">
    <div class="mb-1 flex items-center justify-between">
      <h2 class="text-sm font-semibold text-ink-soft uppercase">默认密钥</h2>
      <button
        type="button"
        class="rounded px-2 py-1 text-xs text-ink-faint hover:text-ink"
        :aria-pressed="showCredential"
        @click="showCredential = !showCredential"
      >
        {{ showCredential ? '隐藏' : '显示' }}
      </button>
    </div>
    <p class="mb-4 text-xs text-ink-faint">
      端点未单独配置密钥时继承这里。每行一把，支持多把钥匙轮换。
    </p>

    <!-- 密钥池：一行一把，保存时原样回传掩码行由后端还原 -->
    <div class="mt-4">
      <textarea
        id="credentials-pool"
        v-model="credentialsText"
        rows="4"
        spellcheck="false"
        autocomplete="new-password"
        placeholder="sk-...&#10;sk-...&#10;sk-..."
        aria-label="上游钥匙池，每行一把"
        class="glass-field w-full resize-y px-3 py-2 font-mono text-sm outline-none"
        :class="showCredential ? undefined : '[webkit-text-security:disc]'"
      ></textarea>
      <p class="mt-1 text-xs text-ink-faint">
        {{ poolLineCount }} 把钥匙参与轮换；留空则不可用（端点也未配置时）。
      </p>
    </div>

    <!-- 钥匙池策略 -->
    <div class="mt-4 flex flex-col gap-1.5">
      <span class="text-xs font-medium text-ink-soft">钥匙池策略</span>
      <div class="flex flex-wrap gap-2">
        <label
          v-for="option in KEY_STRATEGY_OPTIONS"
          :key="option.value"
          class="glass-field flex cursor-pointer items-center gap-2 px-3 py-2 text-sm"
        >
          <input
            v-model="form.key_strategy"
            type="radio"
            name="key-strategy"
            :value="option.value"
            class="accent-accent"
          />
          <span>{{ option.label }}</span>
          <span class="text-xs text-ink-faint">{{ option.hint }}</span>
        </label>
      </div>
    </div>
  </section>
</template>
