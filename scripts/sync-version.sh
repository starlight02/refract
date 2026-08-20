#!/usr/bin/env bash
# ==============================================================================
# 同步 workspace 内所有可发布 package.json 版本号（供 cargo-release 钩子与自动化流程调用）
#
# 用法：
#   ./scripts/sync-version.sh 0.2.0
# ==============================================================================
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
  echo "错误: 缺少版本号参数。用法: $0 <version>" >&2
  exit 1
fi

for file in \
  package.json \
  apps/admin/package.json \
  apps/homepage/package.json \
  packages/contracts/package.json; do
  if [ -f "$file" ]; then
    dir="$(dirname "$file")"
    (cd "$dir" && pnpm pkg set version="$VERSION")
  fi
done

echo "==> 已同步 package.json 版本为 ${VERSION}"
