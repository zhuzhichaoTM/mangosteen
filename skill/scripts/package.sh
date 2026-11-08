#!/bin/bash
# 打包验证脚本
# 用于验证 skill 文件完整性并创建发布包

set -e

SKILL_DIR="$HOME/.workbuddy/skills/mangosteen-llm-deployer"
OUTPUT_DIR="$HOME/.workbuddy/skills/mangosteen-llm-deployer/dist"
VERSION="1.0.0"

echo "=== Mangosteen Skill 打包验证 ==="
echo ""

# 检查必需文件
echo "检查必需文件..."

REQUIRED_FILES=(
    "SKILL.md"
    "_meta.json"
    "_skillhub_meta.json"
    "skill-card.md"
)

for file in "${REQUIRED_FILES[@]}"; do
    if [ -f "$SKILL_DIR/$file" ]; then
        echo "✓ $file"
    else
        echo "✗ $file 缺失"
        exit 1
    fi
done

echo ""

# 检查可选目录
echo "检查可选目录..."

OPTIONAL_DIRS=(
    "references"
    "scripts"
    "assets"
)

for dir in "${OPTIONAL_DIRS[@]}"; do
    if [ -d "$SKILL_DIR/$dir" ]; then
        COUNT=$(find "$SKILL_DIR/$dir" -type f | wc -l)
        echo "✓ $dir/ ($COUNT 个文件)"
    else
        echo "⊙ $dir/ 不存在（可选）"
    fi
done

echo ""

# 验证 SKILL.md frontmatter
echo "验证 SKILL.md frontmatter..."

if grep -q "^name:" "$SKILL_DIR/SKILL.md" && \
   grep -q "^version:" "$SKILL_DIR/SKILL.md" && \
   grep -q "^description:" "$SKILL_DIR/SKILL.md"; then
    echo "✓ SKILL.md frontmatter 完整"
else
    echo "✗ SKILL.md frontmatter 不完整"
    exit 1
fi

echo ""

# 验证脚本权限
echo "验证脚本权限..."

SCRIPTS=(
    "scripts/detect-platform.sh"
    "scripts/health-check.sh"
    "scripts/prepare-env.sh"
    "scripts/quick-deploy.sh"
)

for script in "${SCRIPTS[@]}"; do
    if [ -f "$SKILL_DIR/$script" ]; then
        if [ -x "$SKILL_DIR/$script" ]; then
            echo "✓ $script (可执行)"
        else
            echo "⚠️ $script (不可执行)"
        fi
    fi
done

echo ""

# 统计文件
echo "文件统计："
echo "  Markdown 文件: $(find "$SKILL_DIR" -name "*.md" | wc -l)"
echo "  Shell 脚本: $(find "$SKILL_DIR" -name "*.sh" | wc -l)"
echo "  JSON 文件: $(find "$SKILL_DIR" -name "*.json" | wc -l)"
echo "  总文件数: $(find "$SKILL_DIR" -type f | wc -l)"

echo ""

# 创建输出目录
mkdir -p "$OUTPUT_DIR"

# 打包
echo "创建发布包..."
cd "$SKILL_DIR"
zip -r "$OUTPUT_DIR/mangosteen-llm-deployer-$VERSION.zip" \
    SKILL.md \
    README.md \
    QUICKSTART.md \
    CHANGELOG.md \
    DESIGN.md \
    PUBLISH_PLAN.md \
    _meta.json \
    _skillhub_meta.json \
    skill-card.md \
    references/ \
    scripts/ \
    assets/ \
    -x "*.DS_Store" \
    -x "*.git*"

echo "✓ 发布包已创建: $OUTPUT_DIR/mangosteen-llm-deployer-$VERSION.zip"

# 显示包大小
SIZE=$(du -h "$OUTPUT_DIR/mangosteen-llm-deployer-$VERSION.zip" | cut -f1)
echo "  包大小: $SIZE"

echo ""
echo "=== 验证完成 ==="
echo ""
echo "发布包位置: $OUTPUT_DIR/mangosteen-llm-deployer-$VERSION.zip"
echo ""
echo "下一步："
echo "  1. 上传到 WorkBuddy 开放平台"
echo "  2. 或发布到 ClawHub"
