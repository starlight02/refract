<script setup lang="ts">
/**
 * 渠道编辑器「基础」段：名称、类型、优先级、权重、超时、标签、启用。
 */
import GlassSwitch from '@/components/GlassSwitch.vue'
import * as m from '@/paraglide/messages'
import type { Channel, ChannelKind } from '@refract/contracts'

const KIND_OPTIONS: { value: ChannelKind; label: () => string }[] = [
  { value: 'chat', label: m.ch_basics_type_chat },
  { value: 'responses', label: m.ch_basics_type_responses },
  { value: 'messages', label: m.ch_basics_type_messages },
  { value: 'gemini', label: m.ch_basics_type_gemini },
  { value: 'aggregate', label: m.ch_basics_type_aggregate },
]
const form = defineModel<Channel>({ required: true })
const tagsText = defineModel<string>('tagsText', { required: true })
defineProps<{ hideVisibility?: boolean }>()
</script>

<template>
  <section class="glass glass-specular p-5">
    <h2 class="mb-4 text-sm font-semibold text-ink-soft uppercase">{{ m.ch_basics_title() }}</h2>

    <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">{{ m.ch_basics_name() }}</span>
        <input
          v-model="form.name"
          type="text"
          :placeholder="m.ch_basics_name_placeholder()"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">{{ m.ch_basics_type() }}</span>
        <select v-model="form.kind" class="glass-field px-3 py-2 text-sm outline-none">
          <option v-for="k in KIND_OPTIONS" :key="k.value" :value="k.value">
            {{ k.label() }}
          </option>
        </select>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">
          {{ m.ch_basics_priority() }}
          <span class="font-normal text-ink-faint">{{ m.ch_basics_priority_hint() }}</span>
        </span>
        <input
          v-model.number="form.priority"
          type="number"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">
          {{ m.ch_basics_weight() }}
          <span class="font-normal text-ink-faint">{{ m.ch_basics_weight_hint() }}</span>
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
          {{ m.ch_basics_timeout() }}
          <span class="font-normal text-ink-faint">{{ m.ch_basics_timeout_hint() }}</span>
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
          >{{ m.ch_basics_tags()
          }}<span class="font-normal text-ink-faint">{{ m.ch_basics_tags_hint() }}</span></span
        >
        <input
          v-model="tagsText"
          type="text"
          :placeholder="m.ch_basics_tags_placeholder()"
          class="glass-field px-3 py-2 text-sm outline-none"
        />
      </label>
      <label v-if="!hideVisibility" class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">{{ m.ch_visibility() }}</span>
        <select v-model="form.visibility" class="glass-field px-3 py-2 text-sm outline-none">
          <option value="shared">{{ m.ch_visibility_shared() }}</option>
          <option value="private">{{ m.ch_visibility_private() }}</option>
        </select>
      </label>
    </div>

    <label class="mt-4 flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="form.enabled" :label="m.ch_basics_enable_switch()" tone="success" />
      <span class="text-sm">{{ m.common_enable() }}</span>
    </label>
  </section>
</template>
