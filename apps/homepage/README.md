# Refract 首页

这是 Refract（协议优先 LLM 网关）的产品首页，含中 / 英切换、可交互的协议棱镜，以及登录页。

上游网关本体在：[github.com/starlight02/refract](https://github.com/starlight02/refract)

## 本地运行

需要 Node.js 22+。

```sh
pnpm install
pnpm --filter @refract/homepage dev
```

浏览器打开终端里提示的地址（默认 `http://localhost:3940`）。

```sh
pnpm --filter @refract/homepage build    # 生产构建
pnpm --filter @refract/homepage preview  # 预览构建产物
```

## 结构

| 路径                                   | 内容                     |
| -------------------------------------- | ------------------------ |
| `src/routes/index.tsx`                 | 首页入口                 |
| `src/components/site/`                 | 导航、棱镜、各区块、页脚 |
| `src/lib/copy.ts`                      | 中英文案                 |
| `src/styles.css`                       | 设计 token               |
| `src/routes/login.tsx`                 | 登录                     |
| `public/favicon.svg` / `public/og.jpg` | 图标与分享图             |
