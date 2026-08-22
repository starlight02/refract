/**
 * 可选副作用：失败已经在别处呈现（store.error、横幅），这里不再冒泡。
 * 用具名函数代替 `.catch(() => {})` / 空 `catch {}`。
 */
export function settle<T>(promise: Promise<T>): Promise<T | undefined> {
  return promise.then(
    (value) => value,
    () => undefined,
  )
}

/** `JSON.parse` 失败时返回 undefined，避免为「不是 JSON」写空 catch。 */
export function tryParseJson<T = unknown>(text: string): T | undefined {
  try {
    return JSON.parse(text) as T
  } catch {
    return undefined
  }
}
