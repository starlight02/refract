#!/bin/sh
# E2E 专用：把真实后端二进制跑在独立端口 + 一次性数据库上。
#
# 不 mock 后端 —— 这套测试的价值就在于前端、管理 API、网关路由、SQLite
# 全部走真的。Playwright 的 webServer 会启动本脚本并在测试结束后杀掉它。
set -e

# 脚本位于 apps/admin/e2e/，回退三级是仓库根目录。
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

# 前端产物必须先存在：二进制用 rust-embed 在编译期内嵌 dist。
if [ ! -f apps/admin/dist/index.html ]; then
  echo "[e2e] apps/admin/dist missing — building frontend first" >&2
  pnpm install --frozen-lockfile --silent
  pnpm --filter @refract/admin build
fi

# build.rs 监听 apps/admin/dist 变化，dist 比二进制新时会重新内嵌；
# 两者都新鲜时这一步是秒级 no-op。
cargo build --locked -p refract-server --quiet

# 在临时目录里运行：避免读到仓库里真实使用的 refract.toml / refract.db，
# 也保证每次测试都是一份全新数据。
RUNTIME_DIR="$(mktemp -d /tmp/refract-e2e.XXXXXX)"
export REFRACT_LISTEN=127.0.0.1:4539
export REFRACT_DATABASE="$RUNTIME_DIR/e2e.db"
export REFRACT_REQUIRE_AUTH=false
# 管理令牌只由服务端签发。清掉宿主可能残留的旧变量，再把明文抄到用例能读的文件。
unset REFRACT_ADMIN_TOKEN
unset REFRACT_MASTER_KEY
unset REFRACT_PROXY
ISSUED_TOKEN_FILE="$ROOT/apps/admin/e2e/.issued-admin-token"
rm -f "$ISSUED_TOKEN_FILE"
"$ROOT/target/debug/refract-server" &
SERVER_PID=$!

cleanup() {
  kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$RUNTIME_DIR"
  rm -f "$ISSUED_TOKEN_FILE"
}
trap cleanup EXIT INT TERM

i=0
while [ "$i" -lt 150 ]; do
  if [ -f "$RUNTIME_DIR/.admin_token" ]; then
    sed -n 's/^admin_token=//p' "$RUNTIME_DIR/.admin_token" | tr -d '\r' > "$ISSUED_TOKEN_FILE"
    break
  fi
  i=$((i + 1))
  sleep 0.2
done
if [ ! -s "$ISSUED_TOKEN_FILE" ]; then
  echo "[e2e] bootstrap did not write $RUNTIME_DIR/.admin_token" >&2
  exit 1
fi

wait "$SERVER_PID"
