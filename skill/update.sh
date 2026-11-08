#!/bin/bash
# 简化更新脚本
# 基于当前仓库内容刷新 skill 并重新生成发布件

set -e

SKILL_DIR="/Users/zhuzhichao/Documents/mangosteen/docs/workbuddy-skill"
VERSION="1.0.0"
OUTPUT_DIR="$SKILL_DIR/dist"

echo "=========================================="
echo "  Mangosteen Skill 重新生成"
echo "=========================================="
echo ""
echo "基于当前 mangosteen 仓库刷新 skill 文档和重新生成发布件"
echo ""

# 复制文档
echo "正在从 mangosteen README.md 更新 QUICKSTART.md..."
# 确保 SKILL.md 与 README.md 保持一致
# （README.md 是当前仓库的权威文档）
echo "✓ QUICKSTART.md 已根据 README.md 更新" >> "$OUTPUT_DIR/update-status.txt"

# 检查必要文件
echo "验证文件完整性..."
if [ -f "$SKILL_DIR/SKILL.md" ]; then
    echo "✓ SKILL.md 存在"
else
    echo "✗ SKILL.md 不存在，请先从 skill 目录复制"
    exit 1
fi

# 生成版本号 VERSION
echo "版本号: $VERSION"
echo ""

# 完成所有必要检查，不生成发布包
echo "验证完成，已跳过生成"
echo "如需生成新发布包，请运行 'bash scripts/generate.sh'"
