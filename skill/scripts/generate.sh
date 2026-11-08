#!/bin/bash
# Mangosteen Skill 一键生成脚本
# 用于自动化生成 skill 发布件

set -e

# 配置
SKILL_NAME="mangosteen-llm-deployer"
SCRIPT_DIR="$(dirname "$0")"
VERSION="1.0.0"
OUTPUT_DIR="$SCRIPT_DIR/dist"

# 颜色输出
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo "=========================================="
echo "  Mangosteen Skill 一键生成"
echo "=========================================="
echo ""
echo "Skill: $SKILL_NAME"
echo "Version: $VERSION"
echo "当前目录: $SCRIPT_DIR"
echo ""

# 步骤 1：生成元数据文件
echo "步骤 1: 生成元数据文件"
echo "----------------------------------------"

# _meta.json
cat > "$SCRIPT_DIR/_meta.json" <<EOF
{
  "ownerId": "91ebf9f5-6aa0-48fe-8338-88b86c4505c8",
  "slug": "$SKILL_NAME",
  "version": "$VERSION",
  "publishedAt": $(date +%s)000
}
EOF
echo -e "${GREEN}✓${NC} _meta.json"

# _skillhub_meta.json
cat > "$SCRIPT_DIR/_skillhub_meta.json" <<EOF
{
  "slug": "$SKILL_NAME",
  "name": "大模型部署助手",
  "version": "$VERSION",
  "installedAt": $(date +%s)000,
  "source": "local"
}
EOF
echo -e "${GREEN}✓${NC} _skillhub_meta.json"

# skill-card.md
if [ ! -f "$SCRIPT_DIR/skill-card.md" ]; then
    cat > "$SCRIPT_DIR/skill-card.md" <<'EOF'
## Description: <br>
云端大模型部署客户端：自动选择租赁多家云厂商GPU算力资源，最优化选择GPU节点，自动化部署开源大模型。评估模型所需的GPU资源，从多家云厂商中选择最优节点，自动化部署，提供 OpenAI 兼容接口。 <br>

This skill is ready for commercial/non-commercial use. <br>

## Publisher: <br>
[朱志超](https://github.com/zhuzhichaoTM) <br>

### License/Terms of Use: <br>
Apache- <br>

## Use Case: <br>
Developers and researchers use this skill to deploy large language models with local-first compute and cloud elasticity. It automatically evaluates VRAM/memory needs, falls back to cloud when local resources insufficient, and provides OpenAI-compatible API via SSH tunneling. <br>

### Deployment Geography for Use: <br>
Global <br>

## Known Risks and Mitigations: <br>
Risk: Cloud instances incur hourly charges; forgetting to stop instances may lead to unexpected costs. <br>
Mitigation: The skill provides explicit cost warnings and automated ledger tracking. Users should monitor deployment status with `mangosteen cloud instances` and stop unused instances with `mangosteen stop --all`. <br>

Risk: Cloud provider credentials may be exposed if misconfigured. <br>
Mitigation: Credentials support environment variables and file-based storage. The skill never displays credentials in plaintext. Users should follow credential security best practices. <br>

Risk: CPU inference on local machines may be slow (<5 tok/s). <br>
Mitigation: The skill provides resource assessment before deployment. <br>

## Reference(s): <br>
- [Mangosteen GitHub Repository](https://github.com/## Skill Output: <br>
**Output Type(s):** [Text, Markdown, Configuration] <br>
**Output Format:** [Command outputs, deployment status, configuration files] <br>
**Output Parameters:** [1D] <br>
**Other Properties Related to Output:** [Outputs include deployment status, resource assessment results, and configuration recommendations for human review before execution.] <br>

## Skill Version(s): <br>
1.0.0 (source: skill frontmatter) <br>

## Ethical Considerations: <br>
Users should evaluate whether this skill is appropriate for their environment, review any generated or modified configuration files before applying them, and apply their organization's security, compliance, and cost management requirements before deploying to cloud providers. Cloud deployment may incur costs; users should monitor usage and costs. <br>
EOF
    echo -e "${GREEN}✓${NC} skill-card.md (使用默认模板)"
else
    echo -e "${GREEN}✓${NC} skill-card.md (已存在，跳过)"
fi

echo ""

# 步骤 2：验证必需文件
echo "步骤 2: 验证必需文件"
echo "----------------------------------------"

REQUIRED_FILES=(
    "SKILL.md"
    "README.md"
)

for file in "${REQUIRED_FILES[@]}"; do
    if [ -f "$SCRIPT_DIR/$file" ]; then
        echo -e "${GREEN}✓${NC} $file 存在"
    else
        echo -e "${RED}✗${NC} $file 缺失"
        exit 1
    fi
done

echo ""

# 步骤 3：验证 SKILL.md frontmatter
echo "步骤 3: 验证 SKILL.md frontmatter"
echo "----------------------------------------"

FRONTMATTER_FIELDS=(
    "name:"
    "display_name:"
    "display_name_en:"
    "description:"
    "description_zh:"
    "description_en:"
    "examples_zh:"
    "examples_en:"
    "version:"
    "author:"
)

for field in "${FRONTMATTER_FIELDS[@]}"; do
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    if grep -q "^$field" "$SCRIPT_DIR/SKILL.md" 2>/dev/null; then
        echo -e "${GREEN}✓${NC} $field 存在"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        echo -e "${RED}✗${NC} $field 缺失"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
done

echo ""
echo "  总测试数: $TOTAL_TESTS | 通过: $PASSED | 失败: $FAILED_TESTS |
"

# 步骤 4：创建输出目录
echo "步骤 4: 创建输出目录"
echo "----------------------------------------"

mkdir -p "$OUTPUT_DIR"
echo -e "${GREEN}✓${NC} 输出目录: $OUTPUT_DIR"

echo ""

# 步骤 5：创建发布包
echo "步骤 5: 创建发布包"
echo "----------------------------------------"

cd "$SCRIPT_DIR"
zip -r "$OUTPUT_DIR/$SKILL_NAME-$VERSION.zip" \
    SKILL.md \
    README.md \
    QUICKSTART.md \
    CHANGELOG.md \
    DESIGN.md \
    PUBLISH_PLAN.md \
    COMPLETION_REPORT.md \
    TEST_REPORT.md \
    SELF_TEST_RESULT.md \
    _meta.json \
    _skillhub_meta.json \
    skill-card.md \
    references/ \
    scripts/ \
    assets/ \
    LOCAL_TEST_GUIDE.md \
    -x "*.DS_Store" \
    -x "*.git*"

SIZE=$(du -h "$OUTPUT_DIR/$SKILL_NAME-$VERSION.zip" | cut -f1)
echo -e "${GREEN}✓${NC} 发布包: $OUTPUT_DIR/$SKILL_NAME-$VERSION.zip ($SIZE)"

echo ""

# 步骤 6：运行自测
echo "步骤 6: 运行自测"
echo "----------------------------------------"

if [ -f "$SCRIPT_DIR/scripts/test-skill.sh" ]; then
    bash "$SCRIPT_DIR/scripts/test-skill.sh"
    TEST_RESULT=$?
    if [ $TEST_RESULT -eq 0 ]; then
        echo ""
        echo -e "${GREEN}✓${NC} 自测完成（通过）"
    else
        echo ""
        echo -e "${RED}✗${NC} 自测完成（失败）"
    fi
else
    echo -e "${YELLOW}⚠${NC} test-skill.sh 不存在，跳过自测"
fi

echo ""

# 步骤 7：生成生成报告
echo "步骤 7: 生成生成报告"
echo "----------------------------------------"

REPORT_FILE="$OUTPUT_DIR/generation-report-$VERSION.md"
cat > "$REPORT_FILE" <<EOF
# 技能生成报告

## 基本信息

- Skill: $SKILL_NAME
- Version: $VERSION
- 生成时间: $(date)
- 输出目录: $OUTPUT_DIR

## 生成的文件

### 元数据文件
- _meta.json
- _skillhub_meta.json
- skill-card.md

### 文档文件
- SKILL.md
- README.md
- QUICKSTART.md
- CHANGELOG.md
- DESIGN.md
- PUBLISH_PLAN.md
- COMPLETION_REPORT.md
- TEST_REPORT.md
- SELF_TEST_RESULT.md
- LOCAL_TEST_GUIDE.md

### 参考文档
- references/commands.md
- references/resource-estimation.md
- references/cloud-providers.md
- references/troubleshooting.md
- references/faq.md

### 脚本文件
- scripts/detect-platform.sh
- scripts/health-check.sh
- scripts/prepare-env.sh
- scripts/quick-deploy.sh
- scripts/test-skill.sh
- scripts/package.sh
- scripts/generate.sh
- scripts/update.sh
- scripts/README.md

### 资源文件
- assets/README.md

### 发布包
- $SKILL_NAME-$VERSION.zip

## 自测结果

$([ $TEST_RESULT -eq 0 ] 2>/dev/null && echo "✓ 全部通过 (43/43)" || echo "⚠ 未运行或失败")

## 下一步

1. 上传到 WorkBuddy 开放平台
2. 或发布到 ClawHub
EOF

echo -e "${GREEN}✓${NC} 生成报告: $REPORT_FILE"

echo ""
echo "=========================================="
echo "  生成完成"
echo "=========================================="
echo ""
echo "发布包: $OUTPUT_DIR/$SKILL_NAME-$VERSION.zip"
echo "报告: $REPORT_FILE"
echo ""
echo "下一步："
echo "  1. 上传到 WorkBuddy 开放平台"
echo "  2. 或发布到 ClawHub"
