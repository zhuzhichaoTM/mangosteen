#!/bin/bash
# 环境健康检查脚本
# 用于检查 mangosteen 运行环境是否正常

set -e

echo "=== Mangosteen 环境健康检查 ==="
echo ""

# 检查 mangosteen 是否安装
if command -v mangosteen &> /dev/null; then
    echo "✓ Mangosteen 已安装"
    mangosteen --version
else
    echo "✗ Mangosteen 未安装"
    echo "请先安装 mangosteen："
    echo "  macOS/Linux: brew install mangosteen"
    echo "  或从 Release 下载对应平台的压缩包"
    exit 1
fi

echo ""

# 检查配置文件
echo "检查配置文件..."
CONFIG_PATH=$(mangosteen config path 2>/dev/null || echo "")
if [ -n "$CONFIG_PATH" ] && [ -f "$CONFIG_PATH" ]; then
    echo "✓ 配置文件存在: $CONFIG_PATH"
else
    echo "✗ 配置文件不存在"
    echo "运行 'mangosteen config init' 生成配置文件"
fi

echo ""

# 验证配置
echo "验证配置..."
if mangosteen config validate &> /dev/null; then
    echo "✓ 配置文件合法"
else
    echo "✗ 配置文件有误"
    echo "运行 'mangosteen config validate' 查看详细错误"
fi

echo ""

# 检查引擎
echo "检查推理引擎..."
if mangosteen engine env &> /dev/null; then
    echo "✓ 引擎配置正常"
    mangosteen engine env
else
    echo "✗ 引擎配置有问题"
    echo "运行 'mangosteen doctor' 查看详细错误"
fi

echo ""

# 检查模型目录
echo "检查模型目录..."
if mangosteen models list &> /dev/null; then
    MODEL_COUNT=$(mangosteen models list 2>/dev/null | wc -l)
    echo "✓ 模型目录正常，当前有 $MODEL_COUNT 个模型"
else
    echo "✗ 模型目录有问题"
    echo "运行 'mangosteen doctor' 查看详细错误"
fi

echo ""

# 检查端口占用
echo "检查默认端口 11516..."
if command -v lsof &> /dev/null; then
    if lsof -i :11516 &> /dev/null; then
        echo "⚠ 端口 11516 已被占用"
        echo "运行 'mangosteen stop --all' 清理残留进程"
    else
        echo "✓ 端口 11516 可用"
    fi
elif command -v netstat &> /dev/null; then
    if netstat -an | grep 11516 &> /dev/null; then
        echo "⚠ 端口 11516 已被占用"
        echo "运行 'mangosteen stop --all' 清理残留进程"
    else
        echo "✓ 端口 11516 可用"
    fi
else
    echo "⊙ 无法检查端口占用（缺少 lsof/netstat）"
fi

echo ""

# 运行完整自检
echo "运行完整自检..."
if mangosteen doctor &> /dev/null; then
    echo "✓ 完整自检通过"
else
    echo "⚠ 完整自检发现问题"
    echo "运行 'mangosteen doctor' 查看详细错误"
fi

echo ""
echo "=== 健康检查完成 ==="
