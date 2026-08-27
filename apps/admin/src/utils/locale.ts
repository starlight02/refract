import { defineCustomClientStrategy, getLocale, isServer } from '@/paraglide/runtime'

/**
 * 注册浏览器语言偏好探测策略并同步 `<html lang="...">` 属性。
 *
 * 解决标准 BCP 47 标签 `zh-Hans` 与浏览器常见的 `zh-CN`/`zh-SG`/`zh` 等
 * 区域标签之间的模糊匹配问题：
 * 1. 优先匹配中文变体（zh-CN / zh-SG / zh-Hans / zh / zh-TW 等）-> zh-Hans
 * 2. 优先匹配英文变体（en-US / en-GB / en 等）-> en
 * 3. 都不匹配时返回 undefined，自动回退到 baseLocale
 */
export function initLocale(): void {
  if (!isServer && typeof navigator !== 'undefined') {
    defineCustomClientStrategy('custom-browser', {
      getLocale: () => {
        const languages = navigator.languages
        if (!languages?.length) return undefined
        for (const lang of languages) {
          const lower = lang.toLowerCase()
          if (lower === 'zh' || lower.startsWith('zh-') || lower === 'zh_cn' || lower === 'cmn') {
            return 'zh-Hans'
          }
          if (lower === 'en' || lower.startsWith('en-')) {
            return 'en'
          }
        }
        return undefined
      },
      setLocale: () => {},
    })
  }

  if (typeof document !== 'undefined') {
    const loc = getLocale()
    document.documentElement.lang = loc === 'zh-Hans' ? 'zh-Hans' : 'en'
  }
}
