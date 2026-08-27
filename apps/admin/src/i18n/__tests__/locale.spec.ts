import { describe, expect, it, beforeEach } from 'vite-plus/test'
import * as m from '@/paraglide/messages'
import { getLocale, setLocale } from '@/paraglide/runtime'
import { initLocale } from '@/utils/locale'

describe('i18n locale switching', () => {
  beforeEach(async () => {
    await setLocale('zh-Hans')
  })

  it('defaults to zh-Hans messages', () => {
    expect(getLocale()).toBe('zh-Hans')
    expect(m.common_save()).toBe('保存')
    expect(m.nav_dashboard()).toBe('仪表盘')
  })

  it('switches to en messages dynamically', async () => {
    await setLocale('en')
    expect(getLocale()).toBe('en')
    expect(m.common_save()).toBe('Save')
    expect(m.nav_dashboard()).toBe('Dashboard')
  })

  it('switches back to zh-Hans', async () => {
    await setLocale('en')
    expect(m.common_save()).toBe('Save')
    await setLocale('zh-Hans')
    expect(m.common_save()).toBe('保存')
  })

  it('interpolates parameters correctly across languages', async () => {
    await setLocale('zh-Hans')
    expect(m.ch_card_recovering_secs({ secs: 30 })).toBe('30 秒后自动恢复')
    await setLocale('en')
    expect(m.ch_card_recovering_secs({ secs: 30 })).toBe('Auto-recovering in 30s')
  })

  it('initializes locale strategy safely in environment', () => {
    initLocale()
    expect(['zh-Hans', 'en']).toContain(getLocale())
  })
})
