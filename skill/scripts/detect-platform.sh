#!/bin/bash
# 平台检测脚本
# 用于检测当前操作系统和架构，帮助选择正确的 mangosteen 版本

set -e

echo "=== Mangosteen 平台检测 ==="
echo ""

# 检测操作系统
OS="$(uname -s)"
case "$OS" in
    Linux*)
        OS_NAME="linux"
        ;;
    Darwin*)
        OS_NAME="macos"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        OS_NAME="windows"
        ;;
    *)
        echo "❌ 不支持的操作系统: $OS"
        exit 1
        ;;
esac

# 检测架构
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64)
        ARCH_NAME="x86_64"
        ;;
    aarch64|arm64)
        ARCH_NAME="aarch64"
        ;;
    i386|i686)
        ARCH_NAME="i386"
        ;;
    *)
        echo "❌ 不支持的架构: $ARCH"
        exit 1
        ;;
esac

echo "操作系统: $OS_NAME"
echo "架构: $ARCH_NAME"
echo ""

# 推荐的下载文件名
if [ "$OS_NAME" = "windows" ]; then
    FILE_NAME="mangosteen-windows-${ARCH_NAME}.zip"
elif [ "$OS_NAME" = "macos" ]; then
    FILE_NAME="mangosteen-macos-${ARCH_NAME}.tar.gz"
else
    FILE_NAME="mangosteen-linux-${ARCH_NAME}.tar.gz"
fi

echo "推荐的下载文件: $FILE_NAME"
echo ""

# 检测是否已安装
if command -v mangosteen &> /dev/null; then
    echo "✓ Mangosteen 已安装"
    mangosteen --version
else
    echo "✗ Mangosteen 未安装"
    echo ""
    echo "安装方法："
    echo "  1. 从 https://github.com/zhuzhichaoTM/mangosteen/releases 下载 $FILE_NAME"
    echo "  2. 解压到 PATH 目录"
    echo "  3. 或使用包管理器（如果可用）"
fi

echo ""
echo "=== 检测完成 ==="
