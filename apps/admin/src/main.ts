import './assets/main.css'

import { createApp, vaporInteropPlugin } from 'vue'
import { createPinia } from 'pinia'

import App from './App.vue'
import router from './router'
import { initLocale } from './utils/locale'

initLocale()
const app = createApp(App)

// Vapor 互操作。vite.config.ts 开了 `features.vapor`，所有 SFC 都编译成
// Vapor（无虚拟 DOM）；但 vue-router 的 RouterView/RouterLink 和 reka-ui
// 都还是 VDOM 组件。没有这个插件，Vapor 组件树里挂 VDOM 子组件会在运行时
// 失败。装上它，两种渲染器就能互相嵌套。
app.use(vaporInteropPlugin)
app.use(createPinia())
app.use(router)

app.mount('#app')
