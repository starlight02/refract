<script setup lang="ts">
/**
 * 渠道高级段：参数覆盖、请求头、空回复重试、代理、测试模型、备注。
 */
import ExtraHeadersEditor from '@/components/ExtraHeadersEditor.vue'
import ParamOverrideEditor from '@/components/ParamOverrideEditor.vue'
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
        <span class="text-sm font-semibold text-ink-soft uppercase">高级</span>
        <span class="ml-2 text-xs text-ink-faint">
          参数覆盖、自定义请求头、空回复重试、代理、测试模型、备注
        </span>
      </span>
      <span class="text-xs text-ink-faint">{{ showAdvanced ? '收起' : '展开' }}</span>
    </button>

    <div v-if="showAdvanced" class="mt-4 flex flex-col gap-4">
      <ParamOverrideEditor v-model="overrideDraft" />

      <ExtraHeadersEditor v-model="headerRows" />

      <div>
        <span class="text-xs font-medium text-ink-soft">上游 200 空回复重试</span>
        <p class="mt-1 text-[0.7rem] text-ink-faint">
          留空继承全局设置；填写 0 可为本渠道关闭对应限制。耗时按“完成时刻 − 首字节时刻”计算。
        </p>
        <div class="mt-2 grid grid-cols-1 gap-4 sm:grid-cols-2">
          <label class="flex flex-col gap-1.5">
            <span class="text-xs text-ink-soft">判定窗口（秒）</span>
            <input
              v-model.number="form.empty_response_retry.window_secs"
              type="number"
              min="0"
              max="3600"
              step="1"
              placeholder="留空继承全局"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
              @change="normalizeEmptyRetryOverride('window_secs')"
            />
          </label>
          <label class="flex flex-col gap-1.5">
            <span class="text-xs text-ink-soft">最大重试次数</span>
            <input
              v-model.number="form.empty_response_retry.max_retries"
              type="number"
              min="0"
              max="100"
              step="1"
              placeholder="留空继承全局"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
              @change="normalizeEmptyRetryOverride('max_retries')"
            />
          </label>
        </div>
      </div>

      <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">
            出站代理<span class="font-normal text-ink-faint">，http/socks5</span>
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
            测试模型<span class="font-normal text-ink-faint">，连通性测试与定时重测用</span>
          </span>
          <input
            v-model="form.test_model"
            type="text"
            placeholder="留空用端点第一个模型"
            class="glass-field px-3 py-2 font-mono text-sm outline-none"
          />
        </label>
      </div>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">
          备注<span class="font-normal text-ink-faint">，仅自己可见</span>
        </span>
        <input
          v-model="form.note"
          type="text"
          placeholder="主力站，月底记得续费"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
      </label>
    </div>
  </section>
</template>
