<script setup lang="ts">
/**
 * 上游模型探测结果弹窗：筛选、多选、导入。
 */
import { computed } from 'vue'
import {
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from 'reka-ui'
import AppIcon from '@/components/AppIcon.vue'
import * as m from '@/paraglide/messages'
import type { ChannelEndpoint, ModelProbe, Protocol } from '@refract/contracts'
export interface ProbeDialogState {
  open: boolean
  protocol: Protocol | null
  targetEndpoint: ChannelEndpoint | null
  loading: boolean
  error: string | null
  models: ModelProbe[]
  selected: Set<string>
  filterQuery: string
}

const probeDialog = defineModel<ProbeDialogState>({ required: true })

const emit = defineEmits<{
  apply: []
  retry: []
}>()

const filteredProbeModels = computed(() => {
  const q = probeDialog.value.filterQuery.trim().toLowerCase()
  if (!q) return probeDialog.value.models
  return probeDialog.value.models.filter(
    (m) =>
      m.id.toLowerCase().includes(q) ||
      (m.display_name && m.display_name.toLowerCase().includes(q)),
  )
})

function toggleProbeSelected(id: string) {
  const s = new Set(probeDialog.value.selected)
  if (s.has(id)) s.delete(id)
  else s.add(id)
  probeDialog.value.selected = s
}

function selectAllFiltered() {
  const s = new Set(probeDialog.value.selected)
  for (const m of filteredProbeModels.value) s.add(m.id)
  probeDialog.value.selected = s
}

function deselectAllFiltered() {
  const s = new Set(probeDialog.value.selected)
  for (const m of filteredProbeModels.value) s.delete(m.id)
  probeDialog.value.selected = s
}
</script>

<template>
  <DialogRoot v-model:open="probeDialog.open">
    <DialogPortal>
      <DialogOverlay
        class="fixed inset-0 z-50 bg-black/40 backdrop-blur-md data-[state=closed]:opacity-0 data-[state=open]:opacity-100"
      />
      <DialogContent
        class="glass-thick glass-specular fixed top-1/2 left-1/2 z-50 flex max-h-[85vh] w-[calc(100%-2rem)] max-w-2xl -translate-x-1/2 -translate-y-1/2 flex-col !bg-canvas/95 p-6 shadow-2xl outline-none dark:!bg-[#12141c]/95"
      >
        <DialogTitle class="flex items-center justify-between text-lg font-semibold">
          <span class="flex items-center gap-2">
            <AppIcon name="globe" :size="20" class="text-accent" />
            {{ m.probe_dialog_title() }}
          </span>
          <DialogClose
            class="rounded-lg p-1 text-ink-faint transition-colors hover:bg-ink/5 hover:text-ink cursor-pointer"
          >
            <AppIcon name="x" :size="18" />
          </DialogClose>
        </DialogTitle>

        <DialogDescription class="mt-1 text-xs text-ink-faint">
          {{ m.probe_dialog_desc() }}
        </DialogDescription>

        <div
          v-if="probeDialog.loading"
          class="flex flex-col items-center justify-center py-16 text-center"
        >
          <div
            class="size-8 animate-spin rounded-full border-2 border-accent border-t-transparent"
          ></div>
          <p class="mt-3 text-sm text-ink-soft">{{ m.probe_dialog_loading() }}</p>
          <p class="mt-1 text-xs text-ink-faint">
            {{ m.probe_dialog_proto_hint({ protocol: probeDialog.protocol ?? '' }) }}
          </p>
        </div>

        <div
          v-else-if="probeDialog.error"
          class="my-4 rounded-xl border border-danger/30 bg-danger/10 p-4"
        >
          <p class="text-sm font-semibold text-danger">{{ m.probe_dialog_failed() }}</p>
          <p class="mt-1 text-xs text-danger/90">{{ probeDialog.error }}</p>
          <p class="mt-2 text-[0.75rem] text-ink-faint">
            {{ m.probe_dialog_failed_hint() }}
          </p>
          <div class="mt-3 flex justify-end gap-2">
            <DialogClose as="template">
              <button type="button" class="glass-button-ghost px-3 py-1.5 text-xs">
                {{ m.common_close() }}
              </button>
            </DialogClose>
            <button
              v-if="probeDialog.targetEndpoint"
              type="button"
              class="glass-button-primary px-3 py-1.5 text-xs font-medium"
              @click="emit('retry')"
            >
              {{ m.common_retry() }}
            </button>
          </div>
        </div>

        <div v-else class="mt-4 flex min-h-0 flex-1 flex-col gap-3">
          <div class="flex flex-wrap items-center justify-between gap-2">
            <div class="relative min-w-56 flex-1">
              <input
                v-model="probeDialog.filterQuery"
                type="search"
                :placeholder="m.probe_dialog_search_placeholder()"
                class="glass-field w-full px-3 py-1.5 text-xs outline-none"
              />
            </div>
            <div class="flex items-center gap-2 text-xs">
              <span class="text-ink-faint">
                {{
                  m.probe_dialog_stats({
                    total: probeDialog.models.length,
                    selected: probeDialog.selected.size,
                  })
                }}
              </span>
              <button
                type="button"
                class="glass-button-ghost px-2 py-1 text-xs"
                @click="selectAllFiltered"
              >
                {{ m.probe_dialog_select_all() }}
              </button>
              <button
                type="button"
                class="glass-button-ghost px-2 py-1 text-xs"
                @click="deselectAllFiltered"
              >
                {{ m.probe_dialog_deselect_all() }}
              </button>
            </div>
          </div>

          <div
            v-if="probeDialog.models.length === 0"
            class="py-10 text-center text-sm text-ink-faint"
          >
            {{ m.probe_dialog_empty_upstream() }}
          </div>
          <div
            v-else-if="filteredProbeModels.length === 0"
            class="py-10 text-center text-sm text-ink-faint"
          >
            {{ m.probe_dialog_no_match({ query: probeDialog.filterQuery }) }}
          </div>
          <div
            v-else
            class="glass max-h-72 min-h-36 flex-1 divide-y divide-ink/5 overflow-y-auto rounded-xl p-2.5"
          >
            <div
              v-for="probeItem in filteredProbeModels"
              :key="probeItem.id"
              class="flex cursor-pointer items-center justify-between rounded-lg px-2.5 py-1.5 transition-colors hover:bg-ink/5"
              @click="toggleProbeSelected(probeItem.id)"
            >
              <div class="min-w-0 flex-1 pr-2">
                <p class="truncate font-mono text-xs font-medium text-ink">{{ probeItem.id }}</p>
                <p
                  v-if="probeItem.display_name && probeItem.display_name !== probeItem.id"
                  class="truncate text-[0.7rem] text-ink-faint"
                >
                  {{ probeItem.display_name }}
                </p>
              </div>
              <div
                class="grid size-5 shrink-0 place-items-center rounded border transition-colors"
                :class="
                  probeDialog.selected.has(probeItem.id)
                    ? 'border-accent bg-accent text-white'
                    : 'border-ink/20 bg-transparent'
                "
              >
                <AppIcon v-if="probeDialog.selected.has(probeItem.id)" name="check" :size="12" />
              </div>
            </div>
          </div>
          <div class="mt-2 flex items-center justify-end gap-3 border-t border-ink/8 pt-2">
            <button
              type="button"
              class="glass-button-ghost px-4 py-2 text-xs"
              @click="probeDialog.open = false"
            >
              {{ m.common_cancel() }}
            </button>
            <button
              type="button"
              class="glass-button-primary px-4 py-2 text-xs font-medium disabled:opacity-50"
              :disabled="probeDialog.selected.size === 0"
              @click="emit('apply')"
            >
              {{ m.probe_dialog_import_selected({ count: probeDialog.selected.size }) }}
            </button>
          </div>
        </div>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>
