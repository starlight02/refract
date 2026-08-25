/**
 * Effect 在这个前端只承担一件事：**把失败当值来传，而不是当异常来抓**。
 *
 * 从 `effect/Effect`、`effect/Result` 做 named import，而不是
 * `import { Effect } from 'effect'`。后者是 `export * as` 命名空间，
 * bundler 只能摇掉整棵模块树，摇不掉 Effect.js 里用不到的导出。
 */
import {
  catch as recover,
  result,
  runPromise,
  runSync,
  succeed,
  try as trySync,
  tryPromise,
} from 'effect/Effect'
import { type Result } from 'effect/Result'

export { isFailure, isSuccess, type Result } from 'effect/Result'

/**
 * 把「解析不了就算了」折叠成值。
 *
 * 取代散落各处的 `try { JSON.parse } catch { }`：调用方拿到的是
 * `T | undefined`，不是一个随时会炸的表达式。
 */
export function parseJson<T = unknown>(text: string): T | undefined {
  return runSync(trySync(() => JSON.parse(text) as T).pipe(recover(() => succeed(undefined))))
}

/**
 * 失败无关紧要（或已在别处呈现）时，用兜底值收场。
 *
 * 典型场景是辅助数据：端点健康度拉不到就当空列表，不该挡住主列表渲染。
 * 显式写出兜底值，比一个空 `catch {}` 更说明意图。
 */
export function orElse<T>(task: () => Promise<T>, fallback: T): Promise<T>
export function orElse<T>(task: () => Promise<T>): Promise<T | undefined>
export function orElse<T>(task: () => Promise<T>, fallback?: T): Promise<T | undefined> {
  return runPromise(tryPromise(task).pipe(recover(() => succeed(fallback))))
}

/**
 * 把一次异步动作的结局收成 Effect 的 `Result`，由调用方裁决去向。
 *
 * 存在的理由很具体：store 的查询动作带「陈旧响应守卫」，必须先判定这次响应
 * 是否还是最新的，**之后**才决定写 items 还是写 error。所以失败不能像
 * `withStoreError` 那样在 await 结束的瞬间就落地。
 */
export function settled<T>(task: () => Promise<T>): Promise<Result<T, unknown>> {
  return runPromise(result(tryPromise({ try: task, catch: (rejection) => rejection })))
}
