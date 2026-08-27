<script setup lang="ts">
/**
 * 渠道编辑器「基础」段：名称、类型、优先级、权重、超时、标签、启用。
 */
import GlassSwitch from '@/components/GlassSwitch.vue'
import type { Channel, ChannelKind } from '@refract/contracts'

const KIND_OPTIONS: { value: ChannelKind; label: string; hint: string }[] = [
  { value: 'chat', label: 'Chat', hint: 'OpenAI Chat Completions' },
  { value: 'responses', label: 'Responses', hint: 'OpenAI Responses API' },
  { value: 'messages', label: 'Messages', hint: 'Anthropic Messages' },
  { value: 'gemini', label: 'Gemini', hint: 'Google Gemini' },
  { value: 'aggregate', label: '聚合', hint: '一个渠道挂多个协议端点' },
]

const form = defineModel<Channel>({ required: true })
const tagsText = defineModel<string>('tagsText', { required: true })
</script>

<template>
  <section class="glass glass-specular p-5">
    <h2 class="mb-4 text-sm font-semibold text-ink-soft uppercase">基础</h2>

    <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
      <label class="flex flex-col gap-1.5">
        <input
          v-model="form.name"
          type="text"
          placeholder="例如：中转站-主力"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">类型</span>
        <select v-model="form.kind" class="glass-field px-3 py-2 text-sm outline-none">
          <option v-for="k in KIND_OPTIONS" :key="k.value" :value="k.value">
            {{ k.label }} — {{ k.hint }}
          </option>
        </select>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">
          优先级
          <span class="font-normal text-ink-faint">越大越优先</span>
        </span>
        <input
          v-model.number="form.priority"
          type="number"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">
          权重
          <span class="font-normal text-ink-faint">同优先级内的加权随机</span>
        </span>
        <input
          v-model.number="form.weight"
          type="number"
          min="0"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">
          超时（秒）
          <span class="font-normal text-ink-faint">0 用全局默认</span>
        </span>
        <input
          v-model.number="form.timeout_secs"
          type="number"
          min="0"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft"
          >标签<span class="font-normal text-ink-faint">，逗号分隔</span></span
        >
        <input
          v-model="tagsText"
          type="text"
          placeholder="生产, 便宜"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
      </label>
    </div>

    <label class="mt-4 flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="form.enabled" label="启用渠道" tone="success" />
      <span class="text-sm">启用</span>
    </label>
  </section>
</template>
