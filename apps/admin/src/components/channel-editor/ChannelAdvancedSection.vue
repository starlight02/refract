<script setup lang="ts">
/**
 * 渠道高级段：参数覆盖、请求头、空回复重试、代理、测试模型、备注。
 */
import ExtraHeadersEditor from '@/components/ExtraHeadersEditor.vue'
import ParamOverrideEditor from '@/components/ParamOverrideEditor.vue'
import * as m from '@/paraglide/messages'
import type { HeaderRow } from '@/utils/extra-headers'
import type { OverrideDraft } from '@/utils/param-override'
import type { Channel } from '@refract/contracts'

const form = defineModel<Channel>({ required: true })
const showAdvanced = defineModel<boolean>('showAdvanced', { required: true })
const overrideDraft = defineModel<OverrideDraft>('overrideDraft', { required: true })
const headerRows = defineModel<HeaderRow[]>('headerRows', { required: true })

function normalizeEmptyRetryOverride(key: 'window_secs' | 'max_retries') {
  const value = form.value.empty_response_retry[key]
  if (value === null || (value as unknown) === '' || Number.isNaN(value)) {
    form.value.empty_response_retry[key] = null
  }
}
</script>

<template>
  <section class="glass glass-specular p-5">
    <button
      type="button"
      class="flex w-full items-center justify-between text-left"
      :aria-expanded="showAdvanced"
      @click="showAdvanced = !showAdvanced"
    >
      <span>
        <span class="text-sm font-semibold text-ink-soft uppercase">{{ m.ch_adv_title() }}</span>
        <span class="ml-2 text-xs text-ink-faint">
          {{ m.ch_adv_desc() }}
        </span>
      </span>
      <span class="text-xs text-ink-faint">{{
        showAdvanced ? m.common_collapse() : m.common_expand()
      }}</span>
    </button>

    <div v-if="showAdvanced" class="mt-4 flex flex-col gap-4">
      <ParamOverrideEditor v-model="overrideDraft" />

      <ExtraHeadersEditor v-model="headerRows" />

      <div>
        <span class="text-xs font-medium text-ink-soft">{{ m.ch_adv_empty_retry_title() }}</span>
        <p class="mt-1 text-[0.7rem] text-ink-faint">
          {{ m.ch_adv_empty_retry_desc() }}
        </p>
        <div class="mt-2 grid grid-cols-1 gap-4 sm:grid-cols-2">
          <label class="flex flex-col gap-1.5">
            <span class="text-xs text-ink-soft">{{ m.ch_adv_empty_retry_window() }}</span>
            <input
              v-model.number="form.empty_response_retry.window_secs"
              type="number"
              min="0"
              max="3600"
              step="1"
              :placeholder="m.ch_adv_empty_retry_inherit()"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
              @change="normalizeEmptyRetryOverride('window_secs')"
            />
          </label>
          <label class="flex flex-col gap-1.5">
            <span class="text-xs text-ink-soft">{{ m.ch_adv_empty_retry_max() }}</span>
            <input
              v-model.number="form.empty_response_retry.max_retries"
              type="number"
              min="0"
              max="100"
              step="1"
              :placeholder="m.ch_adv_empty_retry_inherit()"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
              @change="normalizeEmptyRetryOverride('max_retries')"
            />
          </label>
        </div>
      </div>

      <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">
            {{ m.ch_adv_proxy()
            }}<span class="font-normal text-ink-faint">{{ m.ch_adv_proxy_hint() }}</span>
          </span>
          <input
            v-model="form.proxy"
            type="text"
            placeholder="socks5://127.0.0.1:1080"
            class="glass-field px-3 py-2 font-mono text-sm outline-none"
          />
        </label>

        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">
            {{ m.ch_adv_test_model()
            }}<span class="font-normal text-ink-faint">{{ m.ch_adv_test_model_hint() }}</span>
          </span>
          <input
            v-model="form.test_model"
            type="text"
            :placeholder="m.ch_adv_test_model_placeholder()"
            class="glass-field px-3 py-2 font-mono text-sm outline-none"
          />
        </label>
      </div>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">
          {{ m.ch_adv_note()
          }}<span class="font-normal text-ink-faint">{{ m.ch_adv_note_hint() }}</span>
        </span>
        <input
          v-model="form.note"
          type="text"
          :placeholder="m.ch_adv_note_placeholder()"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
      </label>
    </div>
  </section>
</template>
