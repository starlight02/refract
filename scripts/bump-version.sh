#!/usr/bin/env bash
# ==============================================================================
# Refract 版本递增与发版准备脚本
#
# 用法：
#   ./scripts/bump-version.sh patch          # 0.1.0 -> 0.1.1
#   ./scripts/bump-version.sh minor          # 0.1.0 -> 0.2.0
#   ./scripts/bump-version.sh major          # 0.1.0 -> 1.0.0
#   ./scripts/bump-version.sh 0.3.5          # 指定明确版本
#   ./scripts/bump-version.sh minor --tag    # 升级并自动生成签名提交与签名 Tag
# ==============================================================================
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TARGET="${1:-}"
CREATE_TAG=false

for arg in "$@"; do
  if [ "$arg" = "--tag" ]; then
    CREATE_TAG=true
  fi
done

if [ -z "$TARGET" ] || [ "$TARGET" = "--tag" ]; then
  echo "用法: $0 <patch|minor|major|x.y.z> [--tag]" >&2
  exit 1
fi

# 1. 提取当前版本（以 Cargo.toml 为单一事实源）
CURRENT_VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
if [ -z "$CURRENT_VERSION" ]; then
  echo "错误: 无法从 Cargo.toml 读取当前版本" >&2
  exit 1
fi

IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_VERSION"

case "$TARGET" in
  patch)
    NEW_PATCH=$((PATCH + 1))
    NEW_VERSION="${MAJOR}.${MINOR}.${NEW_PATCH}"
    ;;
  minor)
    NEW_MINOR=$((MINOR + 1))
    NEW_VERSION="${MAJOR}.${NEW_MINOR}.0"
    ;;
  major)
    NEW_MAJOR=$((MAJOR + 1))
    NEW_VERSION="${NEW_MAJOR}.0.0"
    ;;
  *)
    if [[ "$TARGET" =~ ^[0-9]+\.[0-9]+\.[0-9]+.*$ ]]; then
      NEW_VERSION="$TARGET"
    else
      echo "错误: 无效的版本参数 '$TARGET' (必须是 patch, minor, major 或 x.y.z 格式)" >&2
      exit 1
    fi
    ;;
esac

echo "==> 版本递增: v${CURRENT_VERSION} -> v${NEW_VERSION}"

# 2. 更新 Cargo.toml
node -e "
const fs = require('fs');
let toml = fs.readFileSync('Cargo.toml', 'utf8');
toml = toml.replace(/^version = \".*?\"/m, 'version = \"${NEW_VERSION}\"');
fs.writeFileSync('Cargo.toml', toml);
"

# 3. 同步 package.json 与 web/package.json
./scripts/sync-version.sh "$NEW_VERSION"
# 4. 刷新 Cargo.lock 与前端格式
echo "==> 刷新 Cargo.lock 与前端产物..."
cargo check --workspace --quiet
(cd web && pnpm exec vp check --fix >/dev/null 2>&1 || true)

echo "==> 已成功将版本更新为 v${NEW_VERSION}"

if [ "$CREATE_TAG" = true ]; then
  echo "==> 创建签名提交与签名 Tag..."
  git add Cargo.toml Cargo.lock package.json web/package.json
  git commit -m "chore: release v${NEW_VERSION}"
  git tag -s "v${NEW_VERSION}" -m "chore: release v${NEW_VERSION}"
  echo "==> 已创建签名 Tag: v${NEW_VERSION}"
  echo "==> 推送命令: git push origin main --tags"
else
  echo "提示: 若要自动提交并打标，可重新运行带 '--tag' 参数，或手动提交:"
  echo "  git commit -m \"chore: release v${NEW_VERSION}\""
  echo "  git tag -s v${NEW_VERSION} -m \"chore: release v${NEW_VERSION}\""
fi
