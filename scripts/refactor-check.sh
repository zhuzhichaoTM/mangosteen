#!/usr/bin/env bash
# Mangosteen 架构重构校验脚本。
#
# 用法：
#   ./scripts/refactor-check.sh        # 全检查，失败则停止
#   ./scripts/refactor-check.sh --quiet  # 仅最终摘要

set -euo pipefail

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
TMP="${TMPDIR:-/tmp}"

log()  { [ "$QUIET" -eq 0 ] && printf '\033[36m→\033[0m %s\n' "$*" || true; }
ok()   { [ "$QUIET" -eq 0 ] && printf '\033[32m✓\033[0m %s\n' "$*" || true; }
fail() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; }

run_check() {
  local title="$1" output="$2"
  shift 2
  log "$title"
  if "$@" > "$output" 2>&1; then
    ok "$title"
  else
    fail "$title 失败"
    [ "$QUIET" -eq 0 ] && sed 's/^/    /' "$output" >&2
    exit 1
  fi
}

run_check "cargo fmt --all -- --check" "$TMP/mn.fmt" cargo fmt --all -- --check
run_check "cargo clippy --workspace --all-targets -- -D warnings" "$TMP/mn.clippy" cargo clippy --workspace --all-targets -- -D warnings
run_check "cargo test --workspace" "$TMP/mn.test" cargo test --workspace
run_check "cargo test -p mangosteen-cli --test layering -- --nocapture" "$TMP/mn.layering" cargo test -p mangosteen-cli --test layering -- --nocapture
run_check "cargo test -p mangosteen-gateway --test e2e" "$TMP/mn.e2e" cargo test -p mangosteen-gateway --test e2e

if [ "$QUIET" -eq 1 ]; then
  printf '🎯 refactor-check: 全部通过（fmt + clippy + test + layering + e2e）\n'
fi
