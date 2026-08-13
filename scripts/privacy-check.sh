#!/bin/sh
# 提交前隐私检查：阻止真实邮箱、API 密钥、本机绝对路径进入版本库。
#
# 扫描对象是「暂存区里的文件内容」（git show :file），而不是工作区 ——
# 部分暂存（git add -p）时只审查真正要提交的内容。
#
# 误报的逃生口：确认某一行是刻意为之（例如文档里的演示值），在该行
# 加上 privacy-allow 标记（放在注释里即可），这一行就会被跳过。
# 测试代码里的假凭据（sk-demo、sk-endpoint-super-secret 等）长度远低于
# 真实密钥的特征长度，不会触发下面的阈值。
set -u

# ── 待扫描文件 ──
# 只看新增/修改/拷贝/重命名的暂存文件；纯二进制格式直接跳过。
files=$(git diff --cached --name-only --diff-filter=ACMR)
[ -z "$files" ] && exit 0

# ── 模式定义（ERE）──
EMAIL='[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'
# 白名单域：文档示例与占位域名不算隐私。
EMAIL_ALLOW='@(([A-Za-z0-9-]+\.)*example\.(com|org|net)|[A-Za-z0-9.-]*\.(invalid|test)|localhost|users\.noreply\.github\.com)'

# 本机路径：家目录字面值 + 各平台的用户目录形态。
HOME_ESCAPED=$(printf '%s' "${HOME:-}" | sed 's/[.[\*^$()+?{|]/\\&/g')
PATHS="/Users/[A-Za-z0-9._-]+|/home/[A-Za-z0-9._-]+|C:\\\\Users\\\\"
[ -n "$HOME_ESCAPED" ] && PATHS="$HOME_ESCAPED|$PATHS"

# 密钥：按真实凭据的格式与长度特征匹配，短的演示值不会命中。
KEYS='sk-[A-Za-z0-9_-]{40,}|sk-ant-[A-Za-z0-9_-]{30,}|AIza[0-9A-Za-z_-]{35}|gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,}|xox[baprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----|eyJ[A-Za-z0-9_-]{17,}\.eyJ[A-Za-z0-9_-]{17,}'

fail=0

# 按文件豁免「邮箱」这一类检查（本机路径与密钥检查不受影响）：
#   Cargo.toml —— authors 字段是作者刻意公开的署名邮箱。
email_check_skipped() {
  case "$1" in
    Cargo.toml) return 0 ;;
    *) return 1 ;;
  esac
}

# 打印一组命中（不经管道调用 —— 管道右侧是子 shell，fail 传不回来）。
emit() {
  # $1=类别 $2=文件 $3=grep -n 的输出（行号:内容，可多行）
  if [ "$fail" -eq 0 ]; then
    echo "✗ 隐私检查未通过 —— 以下内容不应进入提交：" >&2
  fi
  fail=1
  printf '%s\n' "$3" | sed "s|^|  [$1] $2:|" >&2
}

# 注意：$files 由 git 输出，仓库内无带空格路径；逐行处理保持简单。
for f in $files; do
  case "$f" in
    # 锁文件与二进制资产：无手写内容，跳过以免噪声。
    *.lock | */pnpm-lock.yaml | pnpm-lock.yaml | *.png | *.jpg | *.jpeg | *.gif | *.webp | *.ico | *.woff | *.woff2 | *.gz | *.zip) continue ;;
  esac

  content=$(git show ":$f" 2>/dev/null) || continue

  if ! email_check_skipped "$f"; then
    hits=$(printf '%s\n' "$content" | grep -nE "$EMAIL" | grep -vE "$EMAIL_ALLOW" | grep -v 'privacy-allow' || true)
    [ -n "$hits" ] && emit "邮箱" "$f" "$hits"
  fi

  hits=$(printf '%s\n' "$content" | grep -nE "$PATHS" | grep -v 'privacy-allow' || true)
  [ -n "$hits" ] && emit "本机路径" "$f" "$hits"

  hits=$(printf '%s\n' "$content" | grep -nE "$KEYS" | grep -v 'privacy-allow' || true)
  [ -n "$hits" ] && emit "疑似密钥" "$f" "$hits"
done

if [ "$fail" -ne 0 ]; then
  echo "" >&2
  echo "  处理方式：移除上述内容后重新暂存；若确认是刻意保留的演示值，" >&2
  echo "  在该行加 privacy-allow 注释标记。" >&2
  exit 1
fi
exit 0
