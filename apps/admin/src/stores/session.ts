import { computed, reactive } from 'vue'
import { auth } from '@/api/client'
import type { SessionUser } from '@refract/contracts'

/**
 * App 壳层与路由守卫共用的会话。
 *
 * 不用 Pinia：router.beforeEach 可能早于 pinia 安装完成，模块级 reactive
 * 让两边读到同一份状态。
 */
export const session = reactive({
  loaded: false,
  authenticated: false,
  configured: false,
  restricted: false,
  user: null as SessionUser | null,
})

export const isAdmin = computed(() => session.user?.role === 'admin')
export const isRestricted = computed(
  () => session.restricted || session.user?.status === 'pending_verification',
)

export async function refreshSession(): Promise<typeof session> {
  try {
    const sess = await auth.session()
    session.authenticated = sess.authenticated
    session.configured = sess.configured
    session.user = sess.user
    session.restricted = sess.user?.status === 'pending_verification'
  } catch {
    // 网络错误不改已有状态；后端不可达由 App 横幅处理。
  }
  session.loaded = true
  return session
}

export function applyLogin(result: {
  authenticated: boolean
  restricted?: boolean
  user: SessionUser
}): void {
  session.authenticated = result.authenticated
  session.configured = true
  session.user = result.user
  session.restricted = Boolean(result.restricted) || result.user.status === 'pending_verification'
  session.loaded = true
}

export function clearSession(): void {
  session.authenticated = false
  session.user = null
  session.restricted = false
  session.loaded = true
}
