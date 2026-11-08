#!/bin/bash
# 快速部署脚本
# 用于快速评估资源并部署模型

set -e

# 检查参数
if [ -z "$1" ]; then
    echo "用法: $0 <模型名称> [上下文长度]"
    echo ""
    echo "示例:"
    echo "  $0 qwen2.5-0.5b-instruct 8192"
    echo "  $0 qwen2.5-7b-instruct"
    exit 1
fi

MODEL="$1"
CONTEXT="${2:-8192}"

echo "=== Mangosteen 快速部署 ==="
echo "模型: $MODEL"
echo "上下文: $CONTEXT"
echo ""

# 检查 mangosteen 是否安装
if ! command -v mangosteen &> /dev/null; then
    echo "❌ Mangosteen 未安装"
    echo "请先安装 mangosteen"
    exit 1
fi

echo "✓ Mangosteen 已安装"
echo ""

# 步骤 1：评估资源
echo "步骤 1: 评估资源需求..."
if mangosteen assess "$MODEL" --context "$CONTEXT" 2>/dev/null; then
    echo "✓ 资源评估完成"
else
    echo "⚠️ 资源评估失败，但继续部署"
fi
echo ""

# 步骤 2：确认部署
echo "步骤 2: 准备部署"
echo "模型: $MODEL"
echo "上下文: $CONTEXT"
echo ""
read -p "是否继续部署? (y/n) " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "已取消部署"
    exit 0
fi

# 步骤 3：执行部署
echo ""
echo "步骤 3: 执行部署..."
if mangosteen deploy "$MODEL" --context "$CONTEXT"; then
    echo ""
    echo "✓ 部署成功"
    echo ""
    echo "查看状态: mangosteen status"
    echo "OpenAI 接口: http://127.0.0.1:11516/v1"
else
    echo ""
    echo "✗ 部署失败"
    echo ""
    echo "查看错误: mangosteen doctor"
    echo "查看配置: mangosteen config show"
    exit 1
fi

echo ""
echo "=== 部署完成 ==="
