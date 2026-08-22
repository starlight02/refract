<script setup lang="ts">
/**
 * 模型调试台。
 *
 * 请求经 `/api/playground/chat` 走网关完整的分发管线（路由、转码、熔断、
 * 日志），与真实客户端唯一的区别是鉴权面 —— 不需要先建一把网关密钥
 * 就能验证渠道配置。
 */
import { computed, nextTick, onMounted, ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import { models as modelsApi, playground } from '@/api/client'
import { settle, tryParseJson } from '@/utils/async'
import { toErrorMessage } from '@/utils/error'

interface ChatTurn {
  role: 'user' | 'assistant'
  content: string
  /** 请求失败时的错误消息，展示成醒目的失败气泡。 */
  error?: string
}

const modelList = ref<string[]>([])
const model = ref('')
const system = ref('')
const draft = ref('')
const turns = ref<ChatTurn[]>([])
const busy = ref(false)
const scroller = ref<HTMLElement | null>(null)

let controller: AbortController | null = null

onMounted(async () => {
  modelList.value = (await settle(modelsApi.list())) ?? []
  if (!model.value && modelList.value.length > 0) model.value = modelList.value[0]!
})

const canSend = computed(() => !busy.value && model.value !== '' && draft.value.trim() !== '')

async function scrollToEnd() {
  await nextTick()
  scroller.value?.scrollTo({ top: scroller.value.scrollHeight })
}

/** 从一段 SSE 缓冲里消费完整事件，返回剩余的不完整尾部。 */
function consumeSse(buffer: string, onDelta: (text: string) => void): string {
  const events = buffer.split('\n\n')
  const rest = events.pop() ?? ''
  for (const event of events) {
    for (const line of event.split('\n')) {
      if (!line.startsWith('data:')) continue
      const payload = line.slice(5).trim()
      if (payload === '' || payload === '[DONE]') continue
      const parsed = tryParseJson<{
        choices?: { delta?: { content?: string | null } }[]
      }>(payload)
      const delta = parsed?.choices?.[0]?.delta?.content
      if (delta) onDelta(delta)
    }
  }
  return rest
}

async function send() {
  if (!canSend.value) return
  const question = draft.value.trim()
  draft.value = ''
  turns.value.push({ role: 'user', content: question })
  // 必须取回入列后的响应式代理再修改 —— 改 push 前的原始对象不会
  // 触发依赖更新，Vapor 的细粒度渲染下气泡会永远停在空状态。
  turns.value.push({ role: 'assistant', content: '' })
  const reply = turns.value[turns.value.length - 1]!
  busy.value = true
  await scrollToEnd()

  const messages: { role: string; content: string }[] = []
  if (system.value.trim()) messages.push({ role: 'system', content: system.value.trim() })
  for (const turn of turns.value) {
    // 进行中的空回复不参与上下文。
    if (turn === reply || turn.error) continue
    messages.push({ role: turn.role, content: turn.content })
  }

  controller = new AbortController()
  try {
    const response = await playground.chat({
      model: model.value,
      messages,
      stream: true,
    })
    if (!response.ok) {
      const text = await response.text()
      const parsed = tryParseJson<{ error?: { message?: string } }>(text)
      reply.error = parsed?.error?.message || text || `${response.status} ${response.statusText}`
      return
    }

    const reader = response.body?.getReader()
    if (!reader) {
      reply.error = '响应没有可读的流'
      return
    }
    const decoder = new TextDecoder()
    let buffer = ''
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      buffer += decoder.decode(value, { stream: true })
      buffer = consumeSse(buffer, (delta) => {
        reply.content += delta
      })
      await scrollToEnd()
    }
  } catch (e) {
    if (!(e instanceof DOMException && e.name === 'AbortError')) {
      reply.error = toErrorMessage(e, '请求失败')
    }
  } finally {
    busy.value = false
    controller = null
    await scrollToEnd()
  }
}

function stop() {
  controller?.abort()
}

function clear() {
  stop()
  turns.value = []
}
</script>

<template>
  <div class="flex h-[calc(100vh-2.5rem)] flex-col gap-4 max-md:h-auto max-md:min-h-[70vh]">
    <header class="flex flex-wrap items-end justify-between gap-4">
      <div>
        <h1 class="text-2xl font-semibold">调试台</h1>
        <p class="mt-1 text-sm text-ink-faint">
          请求走网关完整管线：路由、转码、熔断与日志一个不少。
        </p>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <label class="flex items-center gap-2 text-sm text-ink-soft">
          模型
          <select v-model="model" class="glass-field min-w-44 outline-none" aria-label="调试模型">
            <option v-if="modelList.length === 0" value="" disabled>没有可用模型</option>
            <option v-for="m in modelList" :key="m" :value="m">{{ m }}</option>
          </select>
        </label>
        <button
          type="button"
          class="glass-button-ghost px-3 py-2 text-sm"
          :disabled="turns.length === 0"
          @click="clear"
        >
          清空会话
        </button>
      </div>
    </header>

    <input
      v-model="system"
      type="text"
      placeholder="System 提示词（可选）"
      aria-label="System 提示词"
      class="glass-field w-full outline-none"
    />

    <!-- 会话区 -->
    <section
      ref="scroller"
      class="glass glass-specular min-h-64 flex-1 space-y-4 overflow-y-auto p-5"
      aria-label="会话记录"
    >
      <p v-if="turns.length === 0" class="py-16 text-center text-sm text-ink-faint">
        选择模型，发出第一条消息试试。
      </p>
      <div
        v-for="(turn, i) in turns"
        :key="i"
        class="flex"
        :class="turn.role === 'user' ? 'justify-end' : 'justify-start'"
      >
        <div
          class="shape-round max-w-[85%] rounded-2xl px-4 py-2.5 text-sm leading-relaxed whitespace-pre-wrap"
          :class="
            turn.error
              ? 'bg-danger/10 text-danger border border-danger/25'
              : turn.role === 'user'
                ? 'bg-accent text-white shadow-sm shadow-accent/25'
                : 'glass border border-ink/10 text-ink shadow-xs'
          "
        >
          <template v-if="turn.error">{{ turn.error }}</template>
          <template v-else-if="turn.content">{{ turn.content }}</template>
          <div v-else class="inline-flex items-center gap-1.5 py-1 px-0.5" aria-label="思考中">
            <span
              class="size-2 rounded-full bg-accent/70 animate-bounce [animation-delay:-0.3s]"
            ></span>
            <span
              class="size-2 rounded-full bg-accent/70 animate-bounce [animation-delay:-0.15s]"
            ></span>
            <span class="size-2 rounded-full bg-accent/70 animate-bounce"></span>
          </div>
        </div>
      </div>
    </section>

    <!-- 输入区 -->
    <form class="flex items-end gap-2" @submit.prevent="send">
      <textarea
        v-model="draft"
        rows="2"
        placeholder="输入消息，Enter 发送，Shift+Enter 换行"
        aria-label="消息输入"
        class="glass-field flex-1 resize-none px-3 py-2.5 text-sm outline-none"
        @keydown.enter.exact.prevent="send"
      ></textarea>
      <button
        v-if="busy"
        type="button"
        class="glass-button-ghost !h-auto shrink-0 px-4 py-2.5 text-sm"
        @click="stop"
      >
        停止
      </button>
      <button
        v-else
        type="submit"
        class="glass-button-primary !h-auto flex shrink-0 items-center gap-1.5 px-4 py-2.5 text-sm font-medium disabled:opacity-50"
        :disabled="!canSend"
      >
        <AppIcon name="bolt" :size="15" />
        发送
      </button>
    </form>
  </div>
</template>
