import { createRouter, createWebHistory } from 'vue-router'
import { AUTH_REQUIRED_EVENT, auth } from '@/api/client'
import DashboardView from '../views/DashboardView.vue'

const router = createRouter({
  history: createWebHistory(import.meta.env.BASE_URL),
  routes: [
    {
      path: '/',
      name: 'dashboard',
      // 仪表盘是首屏，急加载，避免白屏闪烁。
      component: DashboardView,
    },
    {
      path: '/channels',
      name: 'channels',
      component: () => import('../views/ChannelsView.vue'),
    },
    {
      path: '/channels/new',
      name: 'channel-new',
      component: () => import('../views/ChannelEditorView.vue'),
    },
    {
      path: '/channels/:id/edit',
      name: 'channel-edit',
      component: () => import('../views/ChannelEditorView.vue'),
    },
    {
      path: '/models',
      name: 'models',
      component: () => import('../views/ModelsView.vue'),
    },
    {
      path: '/playground',
      name: 'playground',
      component: () => import('../views/PlaygroundView.vue'),
    },
    {
      path: '/logs',
      name: 'logs',
      component: () => import('../views/LogsView.vue'),
    },
    {
      path: '/keys',
      name: 'keys',
      component: () => import('../views/KeysView.vue'),
    },
    {
      path: '/settings',
      name: 'settings',
      component: () => import('../views/SettingsView.vue'),
    },
    {
      // 未知路径一律回仪表盘：单页管理后台没有「找不到页面」的场景，
      // 兜底总比重定向到裸地址更像产品。
      path: '/:pathMatch(.*)*',
      redirect: '/',
    },
  ],
})

/** 路由守卫与 App 壳层共享：首次导航可能早于弹窗监听挂载。 */
export const authGate = { needsLogin: false }

router.beforeEach(async () => {
  try {
    const sess = await auth.session()
    authGate.needsLogin = Boolean(sess.configured && !sess.authenticated)
    if (authGate.needsLogin) {
      window.dispatchEvent(new CustomEvent(AUTH_REQUIRED_EVENT))
    }
  } catch {
    // 网络错误不挡导航；后端不可达由 App 横幅处理。
  }
  return true
})

// 让路由组件按首字母大写命名，开箱即得正确的页面标题。
router.afterEach((to) => {
  const titles: Record<string, string> = {
    dashboard: '仪表盘',
    channels: '渠道',
    'channel-new': '新建渠道',
    'channel-edit': '编辑渠道',
    models: '模型',
    playground: '调试台',
    logs: '请求日志',
    keys: 'API 密钥',
    settings: '设置',
  }
  const title = titles[to.name as string] ?? 'Refract'
  document.title = `${title} · Refract`
})

export default router
