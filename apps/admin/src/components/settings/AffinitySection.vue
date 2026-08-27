<script setup lang="ts">
/**
 * 渠道亲和性：规则草稿、预设、运行状态与清空绑定。
 */
import { ref, watch } from 'vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import AppIcon from '@/components/AppIcon.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
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
const clearAffinity = useAction('清除失败', { toast: true })

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
      return `已清除 ${res.cleared} 条绑定。`
    },
  )
}
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <label class="flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="affinity.enabled" label="启用渠道亲和性" />
      <div>
        <span class="text-sm font-semibold text-ink-soft uppercase">渠道亲和性</span>
        <p class="mt-1 text-xs text-ink-faint">
          按规则（API 密钥 / 请求头 / 请求体字段）把调用方绑定到固定渠道，后续请求优先命中同一渠道。
          仅参与路由选择，不影响密钥池与熔断。改动保存后立即生效。
        </p>
      </div>
    </label>

    <template v-if="affinity.enabled">
      <!-- 预设 -->
      <div class="border-t border-ink/8 pt-4">
        <span class="mb-2 block text-sm font-medium text-ink-soft">常用预设</span>
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
          <span class="text-sm font-medium text-ink-soft">亲和规则</span>
          <button
            type="button"
            class="glass-button-ghost px-3 py-2 text-sm"
            @click="addAffinityRule"
          >
            <AppIcon name="plus" :size="14" />
            添加规则
          </button>
        </div>
        <p v-if="rules.length === 0" class="text-xs text-ink-faint">
          尚未配置规则；启用后无规则时不产生任何绑定。
        </p>

        <div v-for="(draft, ri) in rules" :key="ri" class="mb-4 rounded-xl border border-ink/8 p-4">
          <div class="flex items-start gap-3">
            <label class="flex flex-1 flex-col gap-1.5">
              <span class="text-xs font-medium text-ink-soft"
                >规则名（缓存键的一部分，需唯一）</span
              >
              <input
                v-model="draft.name"
                type="text"
                class="glass-field px-3 py-2 text-sm outline-none"
                placeholder="例如 by-api-key"
              />
            </label>
            <button
              type="button"
              class="glass-button-ghost glass-button-ghost-danger shrink-0 px-2 py-2"
              :aria-label="`删除规则 ${draft.name}`"
              @click="removeAffinityRule(ri)"
            >
              <AppIcon name="trash" :size="13" />
            </button>
          </div>

          <div class="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-3">
            <label class="flex flex-col gap-1.5">
              <span class="text-xs text-ink-soft">模型正则（空 = 全部模型）</span>
              <input
                v-model="draft.model_regex"
                type="text"
                class="glass-field px-3 py-2 font-mono text-sm outline-none"
                placeholder="^(gpt|claude)-.*"
              />
            </label>
            <label class="flex flex-col gap-1.5">
              <span class="text-xs text-ink-soft">路径正则（空 = 全部路径）</span>
              <input
                v-model="draft.path_regex"
                type="text"
                class="glass-field px-3 py-2 font-mono text-sm outline-none"
                placeholder="/v1/chat/completions"
              />
            </label>
            <label class="flex flex-col gap-1.5">
              <span class="text-xs text-ink-soft">取值正则（空 = 原样绑定）</span>
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
              <span class="text-xs font-medium text-ink-soft"
                >绑定来源（按顺序取第一个命中值）</span
              >
              <button
                type="button"
                class="glass-button-ghost px-2 py-1 text-xs"
                @click="addSourceRow(draft)"
              >
                <AppIcon name="plus" :size="12" />
                来源
              </button>
            </div>
            <div v-for="(row, si) in draft.sources" :key="si" class="mb-2 flex items-center gap-2">
              <select
                v-model="row.kind"
                class="glass-field w-40 px-2 py-2 text-sm outline-none"
                aria-label="来源类型"
              >
                <option value="api_key_id">调用方 API 密钥</option>
                <option value="header">请求头</option>
                <option value="body">请求体字段</option>
              </select>
              <input
                v-if="row.kind !== 'api_key_id'"
                v-model="row.value"
                type="text"
                class="glass-field flex-1 px-3 py-2 font-mono text-sm outline-none"
                :placeholder="
                  row.kind === 'header'
                    ? '请求头名，如 X-User-Id'
                    : 'JSON 指针，如 /metadata/user_id'
                "
              />
              <button
                v-if="draft.sources.length > 1"
                type="button"
                class="glass-button-ghost shrink-0 px-2 py-2"
                :aria-label="`删除来源 ${si + 1}`"
                @click="removeSourceRow(draft, si)"
              >
                <AppIcon name="x" :size="13" />
              </button>
            </div>
          </div>

          <div class="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-3">
            <label class="flex flex-col gap-1.5">
              <span class="text-xs text-ink-soft">TTL（秒，空 = 用全局默认）</span>
              <input
                v-model.number="draft.ttl_secs"
                type="number"
                min="1"
                max="604800"
                class="glass-field tabular px-3 py-2 text-sm outline-none"
                placeholder="默认"
              />
            </label>
            <label class="flex cursor-pointer items-center gap-2 self-end pb-2">
              <input
                v-model="draft.include_model"
                type="checkbox"
                class="accent-[var(--color-accent)]"
              />
              <span class="text-xs text-ink-soft">模型参与绑定键（不同模型分开绑定）</span>
            </label>
            <label class="flex cursor-pointer items-center gap-2 self-end pb-2">
              <input
                v-model="draft.skip_retry_on_failure"
                type="checkbox"
                class="accent-[var(--color-accent)]"
              />
              <span class="text-xs text-ink-soft">失败后不切换其他渠道（保持绑定）</span>
            </label>
          </div>
        </div>
      </div>

      <!-- 全局参数 -->
      <div class="border-t border-ink/8 pt-4">
        <span class="mb-2 block text-sm font-medium text-ink-soft">全局参数</span>
        <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <label class="flex flex-col gap-1.5">
            <span class="text-xs text-ink-soft">最大绑定条数（LRU 上限）</span>
            <input
              v-model.number="affinity.max_entries"
              type="number"
              min="1"
              class="glass-field tabular w-40 px-3 py-2 text-sm outline-none"
            />
          </label>
          <label class="flex flex-col gap-1.5">
            <span class="text-xs text-ink-soft">默认 TTL（秒，1–604800）</span>
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
            <span class="text-xs text-ink-soft">绑定渠道成功后更新 TTL（推荐开启）</span>
          </label>
          <label class="flex cursor-pointer items-center gap-2">
            <input
              v-model="affinity.keep_on_channel_disabled"
              type="checkbox"
              class="accent-[var(--color-accent)]"
            />
            <span class="text-xs text-ink-soft">渠道被禁用时保留绑定（否则失效回退重选）</span>
          </label>
        </div>
      </div>

      <!-- 运行状态 -->
      <div class="border-t border-ink/8 pt-4">
        <div class="flex items-center justify-between">
          <span class="text-sm font-medium text-ink-soft">绑定状态</span>
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
            {{ clearAffinity.busy ? '清除中…' : '清空绑定' }}
          </button>
        </div>
        <p v-if="affinityStats" class="mt-2 text-xs text-ink-faint">
          当前绑定
          <span class="font-mono text-ink-soft">{{ affinityStats.stats.entries }}</span> 条
          （容量上限 <span class="font-mono text-ink-soft">{{ affinity.max_entries }}</span
          >），命中 <span class="font-mono text-ink-soft">{{ affinityStats.stats.hits }}</span> /
          未命中 <span class="font-mono text-ink-soft">{{ affinityStats.stats.misses }}</span
          >，淘汰 <span class="font-mono text-ink-soft">{{ affinityStats.stats.evictions }}</span
          >。
        </p>
      </div>

      <p v-if="!valid" class="text-xs text-danger" role="alert">
        规则不合法：名称需非空且唯一；每条规则至少一个来源；请求头名不能为空；body 路径需以 /
        开头；TTL 需为 1–604800 的整数。
      </p>
    </template>
  </section>
</template>
