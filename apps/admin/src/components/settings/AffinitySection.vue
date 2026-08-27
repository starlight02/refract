<script setup lang="ts">
/**
 * 渠道亲和性：规则草稿、预设、运行状态与清空绑定。
 */
import { ref, watch } from 'vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import AppIcon from '@/components/AppIcon.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import * as m from '@/paraglide/messages'
import { useAction } from '@/composables/useAction'
import { settings } from '@/api/client'
import { orElse } from '@/utils/effect'
import { AFFINITY_PRESETS, emptyAffinityRule, type AffinityRuleDraft } from '@/utils/affinity-draft'
import type { AffinitySettings, AffinityStatsResponse } from '@refract/contracts'

const affinity = defineModel<AffinitySettings>({ required: true })
const rules = defineModel<AffinityRuleDraft[]>('rules', { required: true })

defineProps<{
  loadError?: string | null
  valid: boolean
}>()

const emit = defineEmits<{
  retry: []
}>()

const affinityStats = ref<AffinityStatsResponse | null>(null)
const clearAffinity = useAction(m.settings_affinity_clear_failed(), { toast: true })

async function refreshAffinityStats() {
  affinityStats.value = await orElse(() => settings.affinityStats(), null)
}

watch(
  () => affinity.value,
  () => {
    void refreshAffinityStats()
  },
  { immediate: true },
)

function addAffinityRule() {
  rules.value.push(emptyAffinityRule(rules.value.length + 1))
}

function removeAffinityRule(index: number) {
  rules.value.splice(index, 1)
}

function addSourceRow(draft: AffinityRuleDraft) {
  draft.sources.push({ kind: 'api_key_id', value: '' })
}

function removeSourceRow(draft: AffinityRuleDraft, index: number) {
  draft.sources.splice(index, 1)
}

function applyAffinityPreset(preset: (typeof AFFINITY_PRESETS)[number]) {
  rules.value.push(preset.make())
}

/** 清空已建立的绑定缓存；不影响规则本身。 */
async function clearAffinityBindings() {
  if (clearAffinity.busy) return
  await clearAffinity.run(
    () => settings.clearAffinity(),
    (res) => {
      void refreshAffinityStats()
      return m.settings_affinity_cleared_msg({ count: res.cleared })
    },
  )
}
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <label class="flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="affinity.enabled" :label="m.settings_affinity_switch()" />
      <div>
        <span class="text-sm font-semibold text-ink-soft uppercase">{{
          m.settings_affinity_title()
        }}</span>
        <p class="mt-1 text-xs text-ink-faint">
          {{ m.settings_affinity_desc() }}
        </p>
      </div>
    </label>

    <template v-if="affinity.enabled">
      <!-- 预设 -->
      <div class="border-t border-ink/8 pt-4">
        <span class="mb-2 block text-sm font-medium text-ink-soft">{{
          m.settings_affinity_presets()
        }}</span>
        <div class="flex flex-wrap gap-2">
          <button
            v-for="preset in AFFINITY_PRESETS"
            :key="preset.label"
            type="button"
            class="glass-button-ghost px-3 py-2 text-sm"
            :title="preset.desc"
            @click="applyAffinityPreset(preset)"
          >
            <AppIcon name="sparkles" :size="14" />
            {{ preset.label }}
          </button>
        </div>
      </div>

      <!-- 规则列表 -->
      <div class="border-t border-ink/8 pt-4">
        <div class="mb-3 flex items-center justify-between">
          <span class="text-sm font-medium text-ink-soft">{{
            m.settings_affinity_rules_title()
          }}</span>
          <button
            type="button"
            class="glass-button-ghost px-3 py-2 text-sm"
            @click="addAffinityRule"
          >
            <AppIcon name="plus" :size="14" />
            {{ m.settings_affinity_add_rule() }}
          </button>
        </div>
        <p v-if="rules.length === 0" class="text-xs text-ink-faint">
          {{ m.settings_affinity_no_rules() }}
        </p>
        <div v-for="(draft, ri) in rules" :key="ri" class="mb-4 rounded-xl border border-ink/8 p-4">
          <div class="flex items-start gap-3">
            <label class="flex flex-1 flex-col gap-1.5">
              <span class="text-xs font-medium text-ink-soft">{{
                m.settings_affinity_rule_name()
              }}</span>
              <input
                v-model="draft.name"
                type="text"
                class="glass-field px-3 py-2 text-sm outline-none"
                :placeholder="m.settings_affinity_rule_name_placeholder()"
              />
            </label>
            <button
              type="button"
              class="glass-button-ghost glass-button-ghost-danger shrink-0 px-2 py-2"
              :aria-label="m.settings_affinity_del_rule_aria({ name: draft.name })"
              @click="removeAffinityRule(ri)"
            >
              <AppIcon name="trash" :size="13" />
            </button>
          </div>

          <div class="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-3">
            <label class="flex flex-col gap-1.5">
              <span class="text-xs text-ink-soft">{{ m.settings_affinity_model_regex() }}</span>
              <input
                v-model="draft.model_regex"
                type="text"
                class="glass-field px-3 py-2 font-mono text-sm outline-none"
                placeholder="^(gpt|claude)-.*"
              />
            </label>
            <label class="flex flex-col gap-1.5">
              <span class="text-xs text-ink-soft">{{ m.settings_affinity_path_regex() }}</span>
              <input
                v-model="draft.path_regex"
                type="text"
                class="glass-field px-3 py-2 font-mono text-sm outline-none"
                placeholder="/v1/chat/completions"
              />
            </label>
            <label class="flex flex-col gap-1.5">
              <span class="text-xs text-ink-soft">{{ m.settings_affinity_val_regex() }}</span>
              <input
                v-model="draft.value_regex"
                type="text"
                class="glass-field px-3 py-2 font-mono text-sm outline-none"
                placeholder="^user-(\d+)"
              />
            </label>
          </div>

          <!-- 来源列表 -->
          <div class="mt-3">
            <div class="mb-2 flex items-center justify-between">
              <span class="text-xs font-medium text-ink-soft">{{
                m.settings_affinity_sources_title()
              }}</span>
              <button
                type="button"
                class="glass-button-ghost px-2 py-1 text-xs"
                @click="addSourceRow(draft)"
              >
                <AppIcon name="plus" :size="12" />
                {{ m.settings_affinity_add_source() }}
              </button>
            </div>
            <div v-for="(row, si) in draft.sources" :key="si" class="mb-2 flex items-center gap-2">
              <select
                v-model="row.kind"
                class="glass-field w-40 px-2 py-2 text-sm outline-none"
                :aria-label="m.settings_affinity_source_kind_aria()"
              >
                <option value="api_key_id">{{ m.settings_affinity_src_api_key() }}</option>
                <option value="header">{{ m.settings_affinity_src_header() }}</option>
                <option value="body">{{ m.settings_affinity_src_body() }}</option>
              </select>
              <input
                v-if="row.kind !== 'api_key_id'"
                v-model="row.value"
                type="text"
                class="glass-field flex-1 px-3 py-2 font-mono text-sm outline-none"
                :placeholder="
                  row.kind === 'header'
                    ? m.settings_affinity_src_header_placeholder()
                    : m.settings_affinity_src_body_placeholder()
                "
              />
              <button
                v-if="draft.sources.length > 1"
                type="button"
                class="glass-button-ghost shrink-0 px-2 py-2"
                :aria-label="m.settings_affinity_del_source_aria({ index: si + 1 })"
                @click="removeSourceRow(draft, si)"
              >
                <AppIcon name="x" :size="13" />
              </button>
            </div>
          </div>

          <div class="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-3">
            <label class="flex flex-col gap-1.5">
              <span class="text-xs text-ink-soft">{{ m.settings_affinity_ttl_field() }}</span>
              <input
                v-model.number="draft.ttl_secs"
                type="number"
                min="1"
                max="604800"
                class="glass-field tabular px-3 py-2 text-sm outline-none"
                :placeholder="m.settings_affinity_ttl_default()"
              />
            </label>
            <label class="flex cursor-pointer items-center gap-2 self-end pb-2">
              <input
                v-model="draft.include_model"
                type="checkbox"
                class="accent-[var(--color-accent)]"
              />
              <span class="text-xs text-ink-soft">{{ m.settings_affinity_include_model() }}</span>
            </label>
            <label class="flex cursor-pointer items-center gap-2 self-end pb-2">
              <input
                v-model="draft.skip_retry_on_failure"
                type="checkbox"
                class="accent-[var(--color-accent)]"
              />
              <span class="text-xs text-ink-soft">{{ m.settings_affinity_skip_retry() }}</span>
            </label>
          </div>
        </div>
      </div>

      <!-- 全局参数 -->
      <div class="border-t border-ink/8 pt-4">
        <span class="mb-2 block text-sm font-medium text-ink-soft">{{
          m.settings_affinity_global_params()
        }}</span>
        <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <label class="flex flex-col gap-1.5">
            <span class="text-xs text-ink-soft">{{ m.settings_affinity_max_entries() }}</span>
            <input
              v-model.number="affinity.max_entries"
              type="number"
              min="1"
              class="glass-field tabular w-40 px-3 py-2 text-sm outline-none"
            />
          </label>
          <label class="flex flex-col gap-1.5">
            <span class="text-xs text-ink-soft">{{ m.settings_affinity_default_ttl() }}</span>
            <input
              v-model.number="affinity.default_ttl_secs"
              type="number"
              min="1"
              max="604800"
              class="glass-field tabular w-40 px-3 py-2 text-sm outline-none"
            />
          </label>
          <label class="flex cursor-pointer items-center gap-2">
            <input
              v-model="affinity.switch_on_success"
              type="checkbox"
              class="accent-[var(--color-accent)]"
            />
            <span class="text-xs text-ink-soft">{{ m.settings_affinity_switch_on_success() }}</span>
          </label>
          <label class="flex cursor-pointer items-center gap-2">
            <input
              v-model="affinity.keep_on_channel_disabled"
              type="checkbox"
              class="accent-[var(--color-accent)]"
            />
            <span class="text-xs text-ink-soft">{{ m.settings_affinity_keep_disabled() }}</span>
          </label>
        </div>
      </div>

      <!-- 运行状态 -->
      <div class="border-t border-ink/8 pt-4">
        <div class="flex items-center justify-between">
          <span class="text-sm font-medium text-ink-soft">{{
            m.settings_affinity_status_title()
          }}</span>
          <button
            type="button"
            class="glass-button-ghost glass-button-ghost-danger px-3 py-2 text-sm disabled:opacity-50"
            :disabled="clearAffinity.busy"
            @click="clearAffinityBindings"
          >
            <AppIcon
              :name="clearAffinity.busy ? 'spinner' : 'trash'"
              :class="clearAffinity.busy ? 'animate-spin' : ''"
              :size="13"
            />
            {{
              clearAffinity.busy ? m.settings_affinity_clearing() : m.settings_affinity_clear_btn()
            }}
          </button>
        </div>
        <p v-if="affinityStats" class="mt-2 text-xs text-ink-faint">
          {{ m.settings_affinity_stats_active() }}
          <span class="font-mono text-ink-soft">{{ affinityStats.stats.entries }}</span>
          {{ m.settings_affinity_stats_entries() }} （{{ m.settings_affinity_stats_cap() }}
          <span class="font-mono text-ink-soft">{{ affinity.max_entries }}</span
          >），{{ m.settings_affinity_stats_hits() }}
          <span class="font-mono text-ink-soft">{{ affinityStats.stats.hits }}</span> /
          {{ m.settings_affinity_stats_misses() }}
          <span class="font-mono text-ink-soft">{{ affinityStats.stats.misses }}</span
          >，{{ m.settings_affinity_stats_evictions() }}
          <span class="font-mono text-ink-soft">{{ affinityStats.stats.evictions }}</span
          >。
        </p>
      </div>

      <p v-if="!valid" class="text-xs text-danger" role="alert">
        {{ m.settings_affinity_val_err() }}
      </p>
    </template>
  </section>
</template>
