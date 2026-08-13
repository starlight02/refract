import { createRouter, createWebHistory } from 'vue-router'
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

// 让路由组件按首字母大写命名，开箱即得正确的页面标题。
router.afterEach((to) => {
  const titles: Record<string, string> = {
    dashboard: '仪表盘',
    channels: '渠道',
    'channel-new': '新建渠道',
    'channel-edit': '编辑渠道',
    logs: '请求日志',
    keys: 'API 密钥',
    settings: '设置',
  }
  const title = titles[to.name as string] ?? 'Refract'
  document.title = `${title} · Refract`
})

export default router
