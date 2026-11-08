#!/bin/bash
# Mangosteen Skill 版本更新脚本
# 用于在代码仓更新后快速更新 skills

set -e

# 配置
SKILL_NAME="mangosteen-llm-deployer"
SKILL_DIR="/Users/zhuzhichao/Documents/mangosteen/docs/workbuddy-skill"
MANGOSTEEN_REPO="$HOME/Documents/mangosteen"

# 颜色输出
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo "=========================================="
echo "  Mangosteen Skill 版本更新"
echo "=========================================="
echo ""
echo "Skill: $SKILL_NAME"
echo "Mangosteen 仓库: $MANGOSTEEN_REPO"
echo ""

# 步骤 1：检查 mangosteen 仓库
echo "步骤 1: 检查 mangosteen 仓库"
echo "----------------------------------------"

if [ ! -d "$MANGOSTEEN_REPO" ]; then
    echo -e "${RED}✗${NC} Mangosteen 仓库不存在: $MANGOSTEEN_REPO"
    exit 1
fi

echo -e "${GREEN}✓${NC} 仓库存在"

cd "$MANGOSTEEN_REPO"

# 检查是否有未提交的更改
if [ -n "$(git status --porcelain)" ]; then
    echo -e "${YELLOW}⚠${NC} 仓库有未提交的更改"
    git status --short
    echo ""
    read -p "是否继续更新？(y/n) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        echo "已取消"
        exit 0
    fi
fi

echo ""

# 步骤 2：拉取最新代码
echo "步骤 2: 拉取最新代码"
echo "----------------------------------------"

git pull origin main
echo -e "${GREEN}✓${NC} 代码已更新"

echo ""

# 步骤 3：检查文档更新
echo "步骤 3: 检查文档更新"
echo "----------------------------------------"

# 检查 README.md 是否更新
if [ "$MANGOSTEEN_REPO/README.md" -nt "$SKILL_DIR/README.md" ]; then
    echo -e "${YELLOW}⚠${NC} README.md 已更新"
    echo "建议更新 skill 的 README.md"
fi

# 检查 CHANGELOG 是否更新
if [ -f "$MANGOSTEEN_REPO/CHANGELOG.md" ] && [ "$MANGOSTEEN_REPO/CHANGELOG.md" -nt "$SKILL_DIR/CHANGELOG.md" ]; then
    echo -e "${YELLOW}⚠${NC} CHANGELOG.md 已更新"
    echo "建议更新 skill 的 CHANGELOG.md"
fi

echo ""

# 步骤 4：更新版本号
echo "步骤 4: 更新版本号"
echo "----------------------------------------"

# 读取当前版本
CURRENT_VERSION=$(grep "^version:" "$SKILL_DIR/SKILL.md" | head -1 | cut -d'"' -f2)
echo "当前版本: $CURRENT_VERSION"

# 提示输入新版本
read -p "输入新版本号 (留空保持不变): " NEW_VERSION

if [ -n "$NEW_VERSION" ]; then
    # 更新 SKILL.md
    sed -i '' "s/^version: \"$CURRENT_VERSION\"/version: \"$NEW_VERSION\"/" "$SKILL_DIR/SKILL.md"
    
    # 更新 _meta.json
    sed -i '' "s/\"version\": \"$CURRENT_VERSION\"/\"version\": \"$NEW_VERSION\"/" "$SKILL_DIR/_meta.json"
    
    # 更新 _skillhub_meta.json
    sed -i '' "s/\"version\": \"$CURRENT_VERSION\"/\"version\": \"$NEW_VERSION\"/" "$SKILL_DIR/_skillhub_meta.json"
    
    # 更新 CHANGELOG.md
    sed -i '' "s/\[$CURRENT_VERSION\]/[$NEW_VERSION]/" "$SKILL_DIR/CHANGELOG.md"
    
    echo -e "${GREEN}✓${NC} 版本已更新: $CURRENT_VERSION -> $NEW_VERSION"
    VERSION=$NEW_VERSION
else
    echo "版本保持不变: $CURRENT_VERSION"
    VERSION=$CURRENT_VERSION
fi

echo ""

# 步骤 5：运行生成脚本
echo "步骤 5: 运行生成脚本"
echo "----------------------------------------"

bash "$SKILL_DIR/scripts/generate.sh"

echo ""

# 步骤 6：生成更新报告
echo "步骤 6: 生成更新报告"
echo "----------------------------------------"

REPORT_FILE="$SKILL_DIR/dist/update-report-$(date +%Y%m%d-%H%M%S).md"
cat > "$REPORT_FILE" <<EOF
# Skill 更新报告

## 基本信息

- Skill: $SKILL_NAME
- 旧版本: $CURRENT_VERSION
- 新版本: $VERSION
- 更新时间: $(date)
- Mangosteen 仓库: $MANGOSTEEN_REPO

## 更新内容

### 文档更新
- README.md: $(test "$MANGOSTEEN_REPO/README.md" -nt "$SKILL_DIR/README.md" && echo "已更新" || echo "未更新")
- CHANGELOG.md: $(test -f "$MANGOSTEEN_REPO/CHANGELOG.md" && test "$MANGOSTEEN_REPO/CHANGELOG.md" -nt "$SKILL_DIR/CHANGELOG.md" && echo "已更新" || echo "未更新")

### 代码更新
- 仓库已拉取最新代码

### 生成结果
- 发布包: $SKILL_DIR/dist/$SKILL_NAME-$VERSION.zip
- 生成报告: $SKILL_DIR/dist/generation-report-$VERSION.md

## 下一步

1. 检查更新内容
2. 测试新版本
3. 上传到开放平台
EOF

echo -e "${GREEN}✓${NC} 更新报告: $REPORT_FILE"

echo ""
echo "=========================================="
echo "  更新完成"
echo "=========================================="
echo ""
echo "旧版本: $CURRENT_VERSION"
echo "新版本: $VERSION"
echo "发布包: $SKILL_DIR/dist/$SKILL_NAME-$VERSION.zip"
echo "更新报告: $REPORT_FILE"
echo ""
echo "下一步："
echo "  1. 检查更新内容"
echo "  2. 测试新版本"
echo "  3. 上传到开放平台"
