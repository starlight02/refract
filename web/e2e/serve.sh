#!/bin/sh
# E2E 专用：把真实后端二进制跑在独立端口 + 一次性数据库上。
#
# 不 mock 后端 —— 这套测试的价值就在于前端、管理 API、网关路由、SQLite
# 全部走真的。Playwright 的 webServer 会启动本脚本并在测试结束后杀掉它。
set -e

# 脚本位于 web/e2e/，回退两级是仓库根目录。
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# 前端产物必须先存在：二进制用 rust-embed 在编译期内嵌 dist。
if [ ! -f web/dist/index.html ]; then
  echo "[e2e] web/dist missing — building frontend first" >&2
  (cd web && pnpm install --frozen-lockfile --silent && pnpm run build)
fi

# build.rs 监听 web/dist 变化，dist 比二进制新时会重新内嵌；
# 两者都新鲜时这一步是秒级 no-op。
cargo build --locked -p refract-server --quiet

# 在临时目录里运行：避免读到仓库里真实使用的 refract.toml / refract.db，
# 也保证每次测试都是一份全新数据。
RUNTIME_DIR="$(mktemp -d /tmp/refract-e2e.XXXXXX)"
export REFRACT_LISTEN=127.0.0.1:4539
export REFRACT_DATABASE="$RUNTIME_DIR/e2e.db"
export REFRACT_REQUIRE_AUTH=false
# 显式清除可能从宿主继承的管理令牌与加密密钥，保证全新库处于无令牌的开放初始态。
unset REFRACT_ADMIN_TOKEN
unset REFRACT_MASTER_KEY
unset REFRACT_PROXY
"$ROOT/target/debug/refract-server" &
SERVER_PID=$!

cleanup() {
  kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$RUNTIME_DIR"
}
trap cleanup EXIT INT TERM

wait "$SERVER_PID"
