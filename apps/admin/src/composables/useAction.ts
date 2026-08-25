import { computed, reactive, ref, shallowRef } from 'vue'
import { hasInterrupts, squash } from 'effect/Cause'
import { ensuring, runPromiseExit, sync, tryPromise } from 'effect/Effect'
import { isSuccess } from 'effect/Exit'
import { toErrorMessage } from '@/utils/error'
import { useToastStore } from '@/stores/toast'

export interface Notice {
  tone: 'success' | 'danger'
  text: string
}

/**
 * 一次「用户动作」及其全部 UI 状态：忙碌标记、结果文案、可选 toast、可取消。
 *
 * 为什么由它**持有**状态，而不是把 ref 传进某个 `run(task, { loading, error })`：
 * 动作和它的状态本来是同一件事，拆开写就必然在每个调用点重抄
 * 「置忙 → 清文案 → 成功写一遍 → 失败写一遍 → 复位」这五步 —— 这个前端里
 * 原本重复了十九遍。
 *
 * 失败在这里被折叠成**值**（notice），所以调用点没有 try/catch 可写；
 * 中断（用户取消、切走）走 Effect 的中断语义，天然不会被误报成错误，
 * 也不必再靠 `instanceof DOMException` 猜。
 *
 * @param fallback 拒绝值榨不出文案时的兜底提示
 * @param options.toast 是否同时弹 toast（设置页那种「保存即反馈」的动作要）
 */
export function useAction(fallback: string, options: { toast?: boolean } = {}) {
  const busy = ref(false)
  const notice = shallowRef<Notice | null>(null)
  /** 只展示错误的页面直接绑这个，不必自己判 tone。 */
  const error = computed(() => (notice.value?.tone === 'danger' ? notice.value.text : null))
  const toasts = options.toast === true ? useToastStore() : null

  let controller: AbortController | null = null

  function report(tone: Notice['tone'], text: string): void {
    notice.value = { tone, text }
    if (tone === 'success') toasts?.success(text)
    else toasts?.danger(text)
  }

  /**
   * 跑一次动作。成功回调返回字符串即作为成功文案，返回 void 则安静成功。
   * 返回值为 `undefined` 表示没成功（失败或被取消）。
   */
  async function run<T>(
    task: (signal: AbortSignal) => Promise<T>,
    onOk?: (value: T) => string | void,
  ): Promise<T | undefined> {
    controller = new AbortController()
    notice.value = null
    busy.value = true

    // ensuring 而不是 finally：中断时也保证复位，且复位属于这条 effect 本身。
    const outcome = await runPromiseExit(
      // catch 原样透出拒绝值：裸 tryPromise 会包成 UnknownError，
      // 那样 ApiError 的 message/detail 就全丢了。
      tryPromise({ try: task, catch: (rejection) => rejection }).pipe(
        ensuring(
          sync(() => {
            busy.value = false
          }),
        ),
      ),
      { signal: controller.signal },
    )

    if (isSuccess(outcome)) {
      const text = onOk?.(outcome.value)
      if (typeof text === 'string') report('success', text)
      return outcome.value
    }

    // 取消不是失败：用户自己叫停的，什么都不该弹。
    if (hasInterrupts(outcome.cause)) return undefined
    report('danger', toErrorMessage(squash(outcome.cause), fallback))
    return undefined
  }

  // reactive 解包嵌套 ref：模板里才能写 `save.busy` 而不是永远 truthy 的 Ref 对象。
  return reactive({
    busy,
    notice,
    error,
    run,
    /** 供「重新提交前先撤掉上一次」的场景，如 Playground 连发。 */
    cancel: () => controller?.abort(),
    /** 校验失败等不经过网络的错误，直接写文案。 */
    fail: (text: string) => report('danger', text),
    clear: () => {
      notice.value = null
    },
  })
}
