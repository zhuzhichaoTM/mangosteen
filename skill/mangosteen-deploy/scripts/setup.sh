#!/usr/bin/env bash
# Mangosteen 部署助手 —— 平台检测与环境检查（macOS / Windows(Git-Bash/WSL) / Linux）
# 用法：bash scripts/setup.sh
set -u

info() { printf '\033[36m→\033[0m %s\n' "$*"; }
ok()   { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '\033[33m⚠\033[0m %s\n' "$*"; }
fail() { printf '\033[31m✗\033[0m %s\n' "$*"; }

info "Mangosteen 部署助手 · 环境检查"

# 1. 平台识别
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Darwin)
    if [ "$ARCH" = "arm64" ]; then
      PLATFORM="macos-aarch64"; FILE="mangosteen-macos-aarch64.tar.gz"
    else
      PLATFORM="macos-x86_64"; FILE="mangosteen-macos-x86_64.tar.gz"
    fi ;;
  Linux)
    PLATFORM="linux-x86_64"; FILE="mangosteen-linux-x86_64.tar.gz" ;;
  MINGW*|MSYS*|CYGWIN*)
    PLATFORM="windows-x86_64"; FILE="mangosteen-windows-x86_64.zip" ;;
  *) fail "未知平台：$OS，请手动到 Releases 页下载" ; exit 1 ;;
esac
ok "平台：$PLATFORM"

# 2. mangosteen 是否可用
if command -v mangosteen >/dev/null 2>&1; then
  ok "已安装：$(mangosteen version 2>&1 | head -1)"
  info "下一步：mangosteen config init && mangosteen doctor"
  exit 0
fi

warn "未检测到 mangosteen，请按以下步骤安装："
echo "  1. 下载：https://github.com/zhuzhichaoTM/mangosteen/releases"
echo "     需要文件：${FILE}"
echo "     （GitHub 慢时可加前缀 https://gh-proxy.com/）"
case "$PLATFORM" in
  macos-*)
    echo '  2. tar -xzf <下载文件> && sudo cp mangosteen /usr/local/bin/'
    echo '  3. mangosteen version 验证' ;;
  windows-*)
    echo '  2. 解压 zip，把解压目录加入系统 PATH，重开终端'
    echo '  3. mangosteen version 验证' ;;
  linux-*)
    echo '  2. tar -xzf <下载文件> && sudo cp mangosteen /usr/local/bin/'
    echo '  3. mangosteen version 验证' ;;
esac