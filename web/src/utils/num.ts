/**
 * number input 的空值归一。
 *
 * `v-model.number` 清空输入框时会留下空字符串（parseFloat('') 是 NaN，
 * Vue 保留原始字符串），直接进 JSON 会被后端 serde 拒收
 * （`invalid type: string ""`）。提交前用这两个函数归一：
 * 空串/非有限值回落到默认值，或按可空语义归一成 null。
 *
 * 注意 `Number('') === 0`：不能只靠 Number.isFinite 判断，
 * 空串必须在类型分支里显式拦截，否则 fallback 永远不会生效。
 */

/** 空串/非有限值回落 fallback；合法数字原样返回。 */
export function numOr(value: unknown, fallback: number): number {
  if (typeof value === 'string') {
    if (value.trim() === '') return fallback
    const n = Number(value)
    return Number.isFinite(n) ? n : fallback
  }
  if (typeof value === 'number') return Number.isFinite(value) ? value : fallback
  return fallback
}

/** 可空版：空串/非有限值归一成 null（后端语义：继承全局/回落默认）。 */
export function numOrNull(value: unknown): number | null {
  if (typeof value === 'string') {
    if (value.trim() === '') return null
    const n = Number(value)
    return Number.isFinite(n) ? n : null
  }
  if (typeof value === 'number') return Number.isFinite(value) ? value : null
  return null
}
