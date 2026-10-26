#!/usr/bin/env bash
# Mangosteen 架构重构校验脚本。
#
# 用法：
#   ./scripts/refactor-check.sh        # 全检查，失败则停止（set -e）
#   ./scripts/refactor-check.sh --quiet  # 仅最终结果摘要
#
# 镜像 release.yml 的 verify 步骤，便于本地按阶段校验。
# 不升级就报错退出，非零返回码。

set -euo pipefail

# 自动把 rustup 的 bin 加入 PATH（CI 环境没 ~/.profile）
if ! command -v cargo >/dev/null 2>&1; then
  for d in "${CARGO_HOME:-$HOME/.cargo}/bin" "/usr/local/cargo/bin" "$HOME/.cargo/bin"; do
    if [ -x "$d/cargo" ]; then
      export PATH="$d:$PATH"
      break
    fi
  done
fi

if ! command -v cargo >/dev/null 2>&1; then
  printf '\033[31m✗\033[0m 找不到 cargo。请先安装 Rust：https://rustup.rs\n' >&2
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

QUIET=0
[ "${1:-}" = "--quiet" ] && QUIET=1

log()  { [ "$QUIET" -eq 0 ] && printf '\033[36m→\033[0m %s\n' "$*" || true; }
ok()   { [ "$QUIET" -eq 0 ] && printf '\033[32m✓\033[0m %s\n' "$*" || true; }
fail() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; }

# --------------------------------------------------- 1. 格式
log "cargo fmt --all -- --check"
if cargo fmt --all -- --check > "${TMPDIR:-/tmp}/mn.fmt" 2>&1; then
  ok "fmt"
else
  fail "cargo fmt 检测到格式问题"
  [ "$QUIET" -eq 0 ] && sed 's/^/    /' "${TMPDIR:-/tmp}/mn.fmt" >&2
  exit 1
fi

# --------------------------------------------------- 2. clippy
log "cargo clippy --workspace --all-targets -- -D warnings"
if cargo clippy --workspace --all-targets -- -D warnings > "${TMPDIR:-/tmp}/mn.clippy" 2>&1; then
  ok "clippy（零警告）"
else
  fail "clippy 输出警告/错误"
  [ "$QUIET" -eq 0 ] && sed 's/^/    /' "${TMPDIR:-/tmp}/mn.clippy" >&2
  exit 1
fi

# --------------------------------------------------- 3. 全量测试
log "cargo test --workspace"
if cargo test --workspace > "${TMPDIR:-/tmp}/mn.test" 2>&1; then
  ok "cargo test --workspace"
else
  fail "cargo test --workspace 失败"
  [ "$QUIET" -eq 0 ] && sed 's/^/    /' "${TMPDIR:-/tmp}/mn.test" >&2
  exit 1
fi

# --------------------------------------------------- 4. 架构守卫
log "cargo test -p mangosteen-cli --test layering -- --nocapture"
if cargo test -p mangosteen-cli --test layering -- --nocapture > "${TMPDIR:-/tmp}/mn.layering" 2>&1; then
  ok "layering（架构约束）"
else
  fail "layering 架构约束失败"
  [ "$QUIET" -eq 0 ] && sed 's/^/    /' "${TMPDIR:-/tmp}/mn.layering" >&2
  exit 1
fi

# --------------------------------------------------- 5. e2e
log "cargo test -p mangosteen-gateway --test e2e"
if cargo test -p mangosteen-gateway --test e2e > "${TMPDIR:-/tmp}/mn.e2e" 2>&1; then
  ok "e2e"
else
  fail "e2e 失败"
  [ "$QUIET" -eq 0 ] && sed 's/^/    /' "${TMPDIR:-/tmp}/mn.e2e" >&2
  exit 1
fi

# --------------------------------------------------- 摘要
if [ "$QUIET" -eq 1 ]; then
  printf '🎯 refactor-check: 全部通过（fmt + clippy + test + layering + e2e）\n'
fi
