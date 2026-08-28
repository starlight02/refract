import { createRouter, createWebHistory } from 'vue-router'
import type { RouteLocationNormalized } from 'vue-router'
import { AUTH_REQUIRED_EVENT } from '@/api/client'
import * as m from '@/paraglide/messages'
import { refreshSession, session } from '@/stores/session'
import DashboardView from '../views/DashboardView.vue'

const router = createRouter({
  history: createWebHistory(import.meta.env.BASE_URL),
  routes: [
    {
      path: '/register',
      name: 'register',
      component: () => import('../views/RegisterView.vue'),
      meta: { public: true },
    },
    {
      path: '/reset',
      name: 'reset',
      component: () => import('../views/ResetView.vue'),
      meta: { public: true },
    },
    {
      path: '/admin/dashboard',
      name: 'admin-dashboard',
      component: DashboardView,
      props: { scope: 'admin' },
      meta: { role: 'admin' },
    },
    {
      path: '/admin/channels',
      name: 'admin-channels',
      component: () => import('../views/ChannelsView.vue'),
      props: { scope: 'admin' },
      meta: { role: 'admin' },
    },
    {
      path: '/admin/channels/new',
      name: 'admin-channel-new',
      component: () => import('../views/ChannelEditorView.vue'),
      props: { scope: 'admin' },
      meta: { role: 'admin' },
    },
    {
      path: '/admin/channels/:id/edit',
      name: 'admin-channel-edit',
      component: () => import('../views/ChannelEditorView.vue'),
      props: { scope: 'admin' },
      meta: { role: 'admin' },
    },
    {
      path: '/admin/models',
      name: 'admin-models',
      component: () => import('../views/ModelsView.vue'),
      meta: { role: 'admin' },
    },
    {
      path: '/admin/users',
      name: 'admin-users',
      component: () => import('../views/UsersAdminView.vue'),
      meta: { role: 'admin' },
    },
    {
      path: '/admin/playground',
      name: 'admin-playground',
      component: () => import('../views/PlaygroundView.vue'),
      meta: { role: 'admin' },
    },
    {
      path: '/admin/logs',
      name: 'admin-logs',
      component: () => import('../views/LogsView.vue'),
      props: { scope: 'admin' },
      meta: { role: 'admin' },
    },
    {
      path: '/admin/settings',
      name: 'admin-settings',
      component: () => import('../views/SettingsView.vue'),
      meta: { role: 'admin' },
    },
    {
      path: '/dashboard',
      name: 'dashboard',
      component: DashboardView,
      props: { scope: 'me' },
    },
    {
      path: '/channels',
      name: 'channels',
      component: () => import('../views/ChannelsView.vue'),
      props: { scope: 'me' },
    },
    {
      path: '/channels/new',
      name: 'channel-new',
      component: () => import('../views/ChannelEditorView.vue'),
      props: { scope: 'me' },
    },
    {
      path: '/channels/:id/edit',
      name: 'channel-edit',
      component: () => import('../views/ChannelEditorView.vue'),
      props: { scope: 'me' },
    },
    {
      path: '/keys',
      name: 'keys',
      component: () => import('../views/KeysView.vue'),
      props: { scope: 'me' },
    },
    {
      path: '/logs',
      name: 'logs',
      component: () => import('../views/LogsView.vue'),
      props: { scope: 'me' },
    },
    {
      path: '/wallet',
      name: 'wallet',
      component: () => import('../views/WalletView.vue'),
    },
    {
      path: '/profile',
      name: 'profile',
      component: () => import('../views/ProfileView.vue'),
    },
    {
      path: '/models',
      redirect: '/admin/models',
    },
    {
      path: '/playground',
      redirect: '/admin/playground',
    },
    {
      path: '/settings',
      redirect: '/admin/settings',
    },
    {
      path: '/',
      name: 'root',
      redirect: () => ({ path: '/admin/dashboard' }),
    },
    {
      path: '/:pathMatch(.*)*',
      redirect: '/',
    },
  ],
})

/** 路由守卫与 App 壳层共享：首次导航可能早于弹窗监听挂载。 */
export const authGate = { needsLogin: false }

const CREATE_ROUTES = new Set([
  'channel-new',
  'channel-edit',
  'admin-channel-new',
  'admin-channel-edit',
])

function homeForRole(): string {
  return session.user?.role === 'admin' ? '/admin/dashboard' : '/dashboard'
}

router.beforeEach(async (to: RouteLocationNormalized) => {
  if (to.meta.public) {
    authGate.needsLogin = false
    return true
  }

  try {
    await refreshSession()
    authGate.needsLogin = Boolean(session.configured && !session.authenticated)
    if (authGate.needsLogin) {
      window.dispatchEvent(new CustomEvent(AUTH_REQUIRED_EVENT))
    }
  } catch {
    // 网络错误不挡导航；后端不可达由 App 横幅处理。
  }

  if (to.path === '/' || to.name === 'root') {
    if (session.authenticated && session.user?.role !== 'admin') {
      return { path: '/dashboard' }
    }
    return { path: '/admin/dashboard' }
  }

  if (to.meta.role === 'admin' && session.authenticated && session.user?.role !== 'admin') {
    return { path: '/dashboard' }
  }

  if (
    session.authenticated &&
    (session.restricted || session.user?.status === 'pending_verification') &&
    typeof to.name === 'string' &&
    CREATE_ROUTES.has(to.name)
  ) {
    return { path: homeForRole() }
  }

  return true
})

router.afterEach((to) => {
  const titles: Record<string, () => string> = {
    'admin-dashboard': m.title_dashboard,
    'admin-channels': m.title_channels,
    'admin-channel-new': m.title_channel_new,
    'admin-channel-edit': m.title_channel_edit,
    'admin-models': m.title_models,
    'admin-playground': m.title_playground,
    'admin-logs': m.title_logs,
    'admin-settings': m.title_settings,
    'admin-users': m.title_users,
    dashboard: m.title_dashboard,
    channels: m.title_channels,
    'channel-new': m.title_channel_new,
    'channel-edit': m.title_channel_edit,
    logs: m.title_logs,
    keys: m.title_keys,
    wallet: m.title_wallet,
    profile: m.title_profile,
    register: m.title_register,
    reset: m.title_reset,
  }
  const getter = titles[to.name as string]
  const title = getter ? getter() : 'Refract'
  document.title = `${title} · Refract`
})

export default router
