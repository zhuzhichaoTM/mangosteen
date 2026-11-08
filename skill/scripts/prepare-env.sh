#!/bin/bash
# 环境准备脚本
# 用于检查 mangosteen 安装状态和配置文件

set -e

echo "=== Mangosteen 环境准备 ==="
echo ""

# 检查 mangosteen 是否安装
if ! command -v mangosteen &> /dev/null; then
    echo "❌ Mangosteen 未安装"
    echo ""
    echo "请选择一种安装方式："
    echo "  1. 从 Release 下载：https://github.com/zhuzhichaoTM/mangosteen/releases"
    echo "  2. 从源码编译："
    echo "     git clone https://github.com/zhuzhichaoTM/mangosteen.git"
    echo "     cd mangosteen && cargo build --release"
    echo "     export PATH=\"\$PWD/target/release:\$PATH\""
    echo ""
    echo "或运行平台检测脚本："
    echo "  bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/detect-platform.sh"
    exit 1
fi

echo "✓ Mangosteen 已安装"
mangosteen --version
echo ""

# 运行 doctor
echo "运行环境自检..."
mangosteen doctor
echo ""

# 检查配置文件
echo "检查配置文件..."
CONFIG_PATH=$(mangosteen config path)
echo "配置文件位置: $CONFIG_PATH"

if [ ! -f "$CONFIG_PATH" ]; then
    echo "⚠️ 配置文件不存在"
    echo "运行 'mangosteen config init' 生成配置文件"
else
    echo "✓ 配置文件存在"

    # 检查是否有 problematic 配置
    if grep -q "device.*ssh" "$CONFIG_PATH" 2>/dev/null; then
        echo "⚠️ 配置文件中包含 device: ssh，可能导致本地部署失败"
        echo "建议："
        echo "  1. 修改为 device: local"
        echo "  2. 或移除 model_whitelist 配置使用默认行为"
    fi

    # 检查是否有 remote_devices
    if grep -q "remote_devices" "$CONFIG_PATH" 2>/dev/null; then
        echo "⚠️ 配置文件中包含 remote_devices，如果不需要 SSH 连接可以移除"
    fi
fi

echo ""

# 检查模型
echo "检查本地模型..."
MODEL_COUNT=$(mangosteen models list 2>/dev/null | grep -c "qwen" || echo "0")
if [ "$MODEL_COUNT" -gt 0 ]; then
    echo "✓ 当前有 $MODEL_COUNT 个本地模型"
    mangosteen models list
else
    echo "⊙ 当前没有本地模型"
    echo "运行 'mangosteen models pull <模型>' 下载模型"
fi

echo ""
echo "=== 环境准备完成 ==="
echo ""
echo "下一步："
echo "  1. 如果没有模型，下载模型：mangosteen models pull <模型>"
echo "  2. 评估资源：mangosteen assess <模型> --context <长度>"
echo "  3. 部署模型：mangosteen deploy <模型> --context <长度>"
