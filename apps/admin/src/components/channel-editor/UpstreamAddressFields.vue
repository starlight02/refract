<script setup lang="ts">
/**
 * 渠道默认地址与端点覆盖共用的非官方 / 完整 / 三段地址字段。
 *
 * 两处文案、控件和 placeholder 本来就不一样，用 variant 分叉而不是强行统一。
 */
import { computed } from 'vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import { PROTO_DEFAULTS } from '@/utils/channel-form'
import type { Protocol, UpstreamAddress } from '@refract/contracts'

const props = defineProps<{
  variant: 'channel' | 'endpoint'
  protocol?: Protocol
}>()

const address = defineModel<UpstreamAddress>({ required: true })
const protoDefaults = computed(() =>
  props.protocol ? PROTO_DEFAULTS[props.protocol] : PROTO_DEFAULTS.chat,
)
</script>

<template>
  <div v-if="variant === 'channel'" class="flex flex-col gap-3">
    <label class="flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="address.unofficial" label="非官方地址" />
      <span class="text-sm">
        <span class="font-medium">非官方地址</span>
        <span class="ml-2 text-xs text-ink-faint"> 关闭时一律使用协议官方地址 </span>
      </span>
    </label>

    <label v-if="address.unofficial" class="flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="address.full_address" label="完整地址" />
      <span class="text-sm">
        <span class="font-medium">完整地址</span>
        <span class="ml-2 text-xs text-ink-faint">直接指定最终 URL，不拼接不校验</span>
      </span>
    </label>

    <template v-if="address.unofficial">
      <input
        v-if="address.full_address"
        v-model="address.base_url"
        type="text"
        placeholder="https://proxy.example.com/openai/v1/chat/completions"
        class="glass-field px-3 py-2 font-mono text-sm outline-none"
      />
      <div v-else class="grid grid-cols-1 gap-3 sm:grid-cols-3">
        <input
          v-model="address.base_url"
          type="text"
          placeholder="https://api.example.com"
          class="glass-field px-3 py-2 font-mono text-sm outline-none sm:col-span-1"
        />
        <input
          v-model="address.version_prefix"
          type="text"
          placeholder="/v1（留空用协议默认）"
          class="glass-field px-3 py-2 font-mono text-sm outline-none"
        />
        <input
          v-model="address.path"
          type="text"
          placeholder="/chat/completions（留空用协议默认）"
          class="glass-field px-3 py-2 font-mono text-sm outline-none"
        />
      </div>
    </template>

    <p v-else class="rounded-lg bg-ink/5 px-3 py-2 text-xs text-ink-faint">
      使用各协议的官方地址。要接中转站请打开「非官方地址」。
    </p>
  </div>

  <template v-else>
    <label class="flex cursor-pointer items-center gap-2 text-xs text-ink-soft">
      <input v-model="address.full_address" type="checkbox" class="accent-[var(--color-accent)]" />
      完整地址
    </label>

    <input
      v-if="address.full_address"
      v-model="address.base_url"
      type="text"
      placeholder="https://proxy.example.com/full/path"
      class="glass-field px-3 py-1.5 font-mono text-xs outline-none"
    />
    <div v-else class="grid grid-cols-1 gap-2 sm:grid-cols-3">
      <input
        v-model="address.base_url"
        type="text"
        :placeholder="protoDefaults.base"
        class="glass-field px-3 py-1.5 font-mono text-xs outline-none"
      />
      <input
        v-model="address.version_prefix"
        type="text"
        :placeholder="protoDefaults.prefix"
        class="glass-field px-3 py-1.5 font-mono text-xs outline-none"
      />
      <input
        v-model="address.path"
        type="text"
        :placeholder="protoDefaults.path"
        class="glass-field px-3 py-1.5 font-mono text-xs outline-none"
      />
    </div>
  </template>
</template>
