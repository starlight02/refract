<script setup lang="ts">
/**
 * 渠道列表里的一张卡片：身份、健康、测试结果、行内操作。
 */
import { computed } from 'vue'
import ProtocolBadge from '@/components/ProtocolBadge.vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import AppIcon from '@/components/AppIcon.vue'
import * as m from '@/paraglide/messages'
import type { Channel, ChannelTestResult, EndpointHealth, Protocol } from '@refract/contracts'

const props = defineProps<{
  channel: Channel
  selecting: boolean
  selected: boolean
  testResult?: ChannelTestResult | 'pending'
  suspended: EndpointHealth[]
  resetting: ReadonlySet<string>
  testingAll: boolean
  balanceBusy: boolean
  copying: boolean
  pendingDelete: boolean
  deleting: boolean
}>()

const emit = defineEmits<{
  select: []
  'toggle-enabled': [enabled: boolean]
  test: []
  balance: []
  duplicate: []
  edit: []
  'ask-delete': []
  'confirm-delete': []
  'cancel-delete': []
  'reset-breaker': [health: EndpointHealth]
}>()

const ch = computed(() => props.channel)

function protocols(channel: Channel): Protocol[] {
  return channel.endpoints.map((e) => e.protocol)
}

function modelCount(channel: Channel): number {
  const names = new Set<string>()
  for (const ep of channel.endpoints) for (const m of ep.models) names.add(m.name)
  return names.size
}

function hasTranscode(channel: Channel): boolean {
  return channel.endpoints.some((e) => e.transcode.enabled && e.transcode.accepted.length > 0)
}

function addressLabel(channel: Channel): string {
  const a = channel.address
  if (!a.unofficial) return m.ch_card_official_addr()
  if (a.full_address) return a.base_url || m.ch_card_unfilled_full_addr()
  return (
    [a.base_url, a.version_prefix, a.path].filter(Boolean).join('') || m.ch_card_unfilled_addr()
  )
}

function canProbeBalance(channel: Channel): boolean {
  return channel.endpoints.some(
    (ep) => ep.enabled && (ep.protocol === 'chat' || ep.protocol === 'responses'),
  )
}

function suspendRemaining(h: EndpointHealth): string {
  if (!h.suspended_until) return ''
  const ms = new Date(h.suspended_until).getTime() - Date.now()
  if (ms <= 0) return m.ch_card_recovering_soon()
  const secs = Math.ceil(ms / 1000)
  if (secs < 60) return m.ch_card_recovering_secs({ secs })
  const mins = Math.ceil(secs / 60)
  if (mins < 60) return m.ch_card_recovering_mins({ mins })
  return m.ch_card_recovering_hours({ hours: Math.ceil(mins / 60) })
}

function resetKey(h: EndpointHealth): string {
  return `${h.channel_id}:${h.protocol}`
}
</script>

<template>
  <article
    class="glass glass-specular glass-interactive p-4"
    :class="[
      ch.enabled ? '' : 'opacity-55',
      selecting && selected ? 'ring-2 ring-accent/45' : '',
      selecting ? 'cursor-pointer select-none' : '',
    ]"
    @click="selecting ? emit('select') : undefined"
  >
    <div class="flex flex-col items-stretch gap-4 sm:flex-row sm:items-start sm:justify-between">
      <div v-if="selecting" class="flex shrink-0 items-start pt-0.5">
        <span
          class="grid size-5 place-items-center rounded-md border transition-colors"
          :class="
            selected
              ? 'border-accent bg-accent text-white'
              : 'border-ink/25 bg-transparent text-transparent'
          "
          role="checkbox"
          :aria-checked="selected"
          :aria-label="m.ch_card_select_aria({ name: ch.name })"
        >
          <AppIcon name="check" :size="12" />
        </span>
      </div>

      <div class="min-w-0 flex-1">
        <div class="flex flex-wrap items-center gap-2">
          <h3 class="whitespace-nowrap text-base font-semibold text-ink">{{ ch.name }}</h3>
          <span
            v-if="ch.kind === 'aggregate'"
            class="proto-badge"
            style="color: var(--color-accent)"
          >
            {{ m.ch_card_aggregate() }}
          </span>
          <ProtocolBadge
            v-for="p in protocols(ch)"
            :key="p"
            :protocol="p"
            :compact="ch.kind === 'aggregate' && ch.endpoints.length > 2"
          />

          <span
            v-if="hasTranscode(ch)"
            class="proto-badge"
            style="color: var(--color-warning)"
            :title="m.ch_card_transcode_title()"
          >
            {{ m.ch_card_transcode_tag() }}
          </span>

          <span
            v-if="ch.auto_disabled"
            class="proto-badge"
            style="color: var(--color-danger)"
            :title="m.ch_card_auto_disabled_title()"
          >
            {{ m.ch_card_auto_disabled_tag() }}
          </span>
        </div>

        <div class="mt-1.5 truncate font-mono text-xs text-ink-faint">
          {{ addressLabel(ch) }}
        </div>

        <div class="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-ink-faint">
          <span
            >{{ m.ch_card_priority() }}
            <span class="tabular font-medium text-ink-soft">{{ ch.priority }}</span></span
          >
          <span
            >{{ m.ch_card_weight() }}
            <span class="tabular font-medium text-ink-soft">{{ ch.weight }}</span></span
          >
          <span
            ><span class="tabular font-medium text-ink-soft">{{ modelCount(ch) }}</span>
            {{ m.ch_card_models_count({ count: '' }).trim() }}</span
          >
          <span v-if="ch.endpoints.length > 1">
            <span class="tabular font-medium text-ink-soft">{{ ch.endpoints.length }}</span>
            {{ m.ch_card_endpoints_count({ count: '' }).trim() }}
          </span>
          <span
            v-if="ch.balance != null"
            :title="m.ch_card_balance_updated({ time: ch.balance_updated_at ?? '' })"
          >
            {{ m.ch_card_balance() }}
            <span
              class="tabular font-medium"
              :class="ch.balance < 1 ? 'text-danger' : 'text-ink-soft'"
            >
              ${{ ch.balance.toFixed(2) }}
            </span>
          </span>
          <span v-for="t in ch.tags ?? []" :key="t" class="rounded bg-ink/8 px-1.5 py-0.5">{{
            t
          }}</span>
        </div>

        <div
          v-if="testResult"
          class="mt-2.5 rounded-lg px-3 py-2 text-xs"
          :class="
            testResult === 'pending'
              ? 'bg-ink/5 text-ink-faint'
              : testResult.success
                ? 'bg-success/12 text-success'
                : 'bg-danger/12 text-danger'
          "
        >
          <template v-if="testResult === 'pending'">{{ m.common_testing() }}</template>
          <template v-else>
            <span class="inline-flex items-center gap-1.5">
              <AppIcon :name="testResult.success ? 'check' : 'x'" :size="13" />
              {{ testResult.message
              }}<template v-if="testResult.latency_ms != null">
                · {{ testResult.latency_ms }}ms</template
              >
            </span>
          </template>
        </div>

        <div
          v-for="h in suspended"
          :key="`susp-${ch.id}-${h.protocol}`"
          class="mt-2.5 rounded-lg bg-danger/12 px-3 py-2 text-xs text-danger"
        >
          <div class="flex flex-wrap items-center gap-2">
            <span class="font-semibold">{{ m.ch_card_suspended_msg({ proto: h.protocol }) }}</span>
            <span class="text-danger/80">{{ suspendRemaining(h) }}</span>
            <span>{{ m.ch_card_consecutive_fails({ count: h.consecutive_fails }) }}</span>
            <button
              type="button"
              class="ml-auto inline-flex items-center gap-1 rounded-md bg-danger/15 px-2 py-1 font-medium hover:bg-danger/25 disabled:opacity-50"
              :disabled="resetting.has(resetKey(h))"
              @click.stop="emit('reset-breaker', h)"
            >
              <AppIcon
                v-if="resetting.has(resetKey(h))"
                name="spinner"
                class="animate-spin"
                :size="11"
              />
              {{
                resetting.has(resetKey(h))
                  ? m.ch_card_resetting_breaker()
                  : m.ch_card_reset_breaker()
              }}
            </button>
          </div>
          <p v-if="h.last_error" class="mt-1.5 line-clamp-2 text-danger/80">
            {{ h.last_error }}
          </p>
        </div>
      </div>

      <div v-if="!selecting" class="flex shrink-0 flex-wrap items-center gap-1.5 sm:justify-end">
        <GlassSwitch
          class="mr-1"
          :model-value="ch.enabled"
          :label="
            ch.enabled
              ? m.ch_card_toggle_disable({ name: ch.name })
              : m.ch_card_toggle_enable({ name: ch.name })
          "
          tone="success"
          @update:model-value="emit('toggle-enabled', $event)"
        />

        <button
          type="button"
          class="glass-button-ghost px-2.5 py-1.5 text-xs"
          :disabled="testResult === 'pending' || testingAll"
          @click="emit('test')"
        >
          <AppIcon
            :name="testResult === 'pending' ? 'spinner' : 'bolt'"
            :class="testResult === 'pending' ? 'animate-spin' : ''"
            :size="13"
          />
          {{ testResult === 'pending' ? m.common_testing() : m.common_test() }}
        </button>

        <button
          v-if="canProbeBalance(ch)"
          type="button"
          class="glass-button-ghost px-2.5 py-1.5 text-xs"
          :disabled="balanceBusy"
          :title="m.ch_card_query_balance_tip()"
          @click="emit('balance')"
        >
          <AppIcon
            :name="balanceBusy ? 'spinner' : 'download'"
            :class="balanceBusy ? 'animate-spin' : ''"
            :size="13"
          />
          {{ balanceBusy ? m.common_clearing() : m.ch_card_balance() }}
        </button>

        <button
          type="button"
          class="glass-button-ghost px-2.5 py-1.5 text-xs"
          :disabled="copying"
          @click="emit('duplicate')"
        >
          <AppIcon
            :name="copying ? 'spinner' : 'copy'"
            :class="copying ? 'animate-spin' : ''"
            :size="13"
          />
          {{ copying ? m.common_copying() : m.common_copy() }}
        </button>

        <button
          type="button"
          class="glass-button-ghost px-2.5 py-1.5 text-xs"
          @click="emit('edit')"
        >
          <AppIcon name="pencil" :size="13" />
          {{ m.common_edit() }}
        </button>

        <button
          v-if="!pendingDelete"
          type="button"
          class="glass-button-ghost glass-button-ghost-danger px-2.5 py-1.5 text-xs !text-ink-faint hover:!text-danger"
          @click="emit('ask-delete')"
        >
          <AppIcon name="trash" :size="13" />
          {{ m.common_delete() }}
        </button>
        <template v-else>
          <button
            type="button"
            class="inline-flex items-center gap-1 rounded-lg bg-danger px-3 py-1.5 text-xs font-medium text-white hover:brightness-105 disabled:opacity-50"
            :disabled="deleting"
            @click="emit('confirm-delete')"
          >
            <AppIcon v-if="deleting" name="spinner" class="animate-spin" :size="12" />
            {{ deleting ? m.common_deleting() : m.common_confirm() }}
          </button>
          <button
            type="button"
            class="glass-button-ghost px-2.5 py-1.5 text-xs cursor-pointer"
            :disabled="deleting"
            @click="emit('cancel-delete')"
          >
            {{ m.common_cancel() }}
          </button>
        </template>
      </div>
    </div>
  </article>
</template>
