# Refract Admin

Vue 3.6 Vapor Mode management UI for Refract. Vite+ provides formatting, linting, tests, and builds; Tailwind CSS, Reka UI, and liquid-glass-vue provide the interface layer.

```sh
pnpm install
pnpm --filter @refract/admin dev
```

The development server proxies `/api`, `/v1`, `/v1beta`, `/health`, and `/metrics` to `127.0.0.1:3939`, so run `cargo run -p refract-server` from the repository root when exercising real data.

Quality gates:

```sh
pnpm --filter @refract/admin check
pnpm --filter @refract/admin test:unit
pnpm --filter @refract/admin build
pnpm --filter @refract/admin test:e2e
```

The E2E suite builds and starts the real Rust server with an isolated SQLite database. The production `apps/admin/dist` output is embedded into `refract-server` at Rust compile time.
