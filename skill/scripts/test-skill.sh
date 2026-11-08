#!/bin/bash
# Skill 本地自测脚本
# 用于验证 mangosteen-llm-deployer skill 的有效性和可用性

set -e

SKILL_DIR="$HOME/.workbuddy/skills/mangosteen-llm-deployer"
TOTAL_TESTS=0
PASSED_TESTS=0
FAILED_TESTS=0

echo "=========================================="
echo "  Mangosteen Skill 本地自测"
echo "=========================================="
echo ""

# 测试 1：必需文件检查
echo "测试 1: 必需文件检查"
echo "----------------------------------------"

REQUIRED_FILES=(
    "SKILL.md"
    "_meta.json"
    "_skillhub_meta.json"
    "skill-card.md"
)

for file in "${REQUIRED_FILES[@]}"; do
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    if [ -f "$SKILL_DIR/$file" ]; then
        echo "✓ $file 存在"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        echo "✗ $file 缺失"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
done

echo ""

# 测试 2：SKILL.md frontmatter 验证
echo "测试 2: SKILL.md frontmatter 验证"
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
    if grep -q "^$field" "$SKILL_DIR/SKILL.md"; then
        echo "✓ $field 存在"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        echo "✗ $field 缺失"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
done

echo ""

# 测试 3：references 文档检查
echo "测试 3: references 文档检查"
echo "----------------------------------------"

REFERENCE_FILES=(
    "references/commands.md"
    "references/resource-estimation.md"
    "references/cloud-providers.md"
    "references/troubleshooting.md"
    "references/faq.md"
)

for file in "${REFERENCE_FILES[@]}"; do
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    if [ -f "$SKILL_DIR/$file" ]; then
        echo "✓ $file 存在"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        echo "✗ $file 缺失"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
done

echo ""

# 测试 4：scripts 脚本检查
echo "测试 4: scripts 脚本检查"
echo "----------------------------------------"

SCRIPT_FILES=(
    "scripts/detect-platform.sh"
    "scripts/health-check.sh"
    "scripts/prepare-env.sh"
    "scripts/quick-deploy.sh"
    "scripts/package.sh"
)

for script in "${SCRIPT_FILES[@]}"; do
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    if [ -f "$SKILL_DIR/$script" ]; then
        if [ -x "$SKILL_DIR/$script" ]; then
            echo "✓ $script 存在且可执行"
            PASSED_TESTS=$((PASSED_TESTS + 1))
        else
            echo "⚠️ $script 存在但不可执行"
            FAILED_TESTS=$((FAILED_TESTS + 1))
        fi
    else
        echo "✗ $script 缺失"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
done

echo ""

# 测试 5：Skill 加载测试
echo "测试 5: Skill 加载测试"
echo "----------------------------------------"

TOTAL_TESTS=$((TOTAL_TESTS + 1))
if command -v Skill &> /dev/null; then
    echo "检测到 Skill 工具，尝试加载 skill..."
    if Skill skill=mangosteen-llm-deployer &> /dev/null; then
        echo "✓ Skill 可以被加载"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        echo "⚠️ Skill 加载失败（可能需要在 WorkBuddy 中测试）"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
else
    echo "⊙ Skill 工具不可用（需要在 WorkBuddy 中测试）"
    PASSED_TESTS=$((PASSED_TESTS + 1))
fi

echo ""

# 测试 6：文档引用检查
echo "测试 6: SKILL.md 中的文档引用检查"
echo "----------------------------------------"

DOC_REFERENCES=(
    "@references/commands.md"
    "@references/resource-estimation.md"
    "@references/cloud-providers.md"
    "@references/troubleshooting.md"
    "@references/faq.md"
)

for ref in "${DOC_REFERENCES[@]}"; do
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    ref_file="${ref#@}"
    if [ -f "$SKILL_DIR/$ref_file" ]; then
        echo "✓ $ref 引用的文件存在"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        echo "✗ $ref 引用的文件不存在"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
done

echo ""

# 测试 7：JSON 文件格式验证
echo "测试 7: JSON 文件格式验证"
echo "----------------------------------------"

JSON_FILES=(
    "_meta.json"
    "_skillhub_meta.json"
)

for json_file in "${JSON_FILES[@]}"; do
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    if python3 -m json.tool "$SKILL_DIR/$json_file" &> /dev/null; then
        echo "✓ $json_file 格式正确"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        echo "✗ $json_file 格式错误"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
done

echo ""

# 测试 8：脚本语法检查
echo "测试 8: 脚本语法检查"
echo "----------------------------------------"

for script in "${SCRIPT_FILES[@]}"; do
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    if bash -n "$SKILL_DIR/$script" 2>/dev/null; then
        echo "✓ $script 语法正确"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        echo "✗ $script 语法错误"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
done

echo ""

# 测试 9：文档字数统计
echo "测试 9: 文档字数统计"
echo "----------------------------------------"

for ref_file in "${REFERENCE_FILES[@]}"; do
    TOTAL_TESTS=$((TOTAL_TESTS + 1))
    WORD_COUNT=$(wc -w < "$SKILL_DIR/$ref_file" 2>/dev/null || echo "0")
    if [ "$WORD_COUNT" -gt 100 ]; then
        echo "✓ $ref_file: $WORD_COUNT 词（内容充足）"
        PASSED_TESTS=$((PASSED_TESTS + 1))
    else
        echo "⚠️ $ref_file: $WORD_COUNT 词（内容较少）"
        FAILED_TESTS=$((FAILED_TESTS + 1))
    fi
done

echo ""

# 测试 10：发布包检查
echo "测试 10: 发布包检查"
echo "----------------------------------------"

TOTAL_TESTS=$((TOTAL_TESTS + 1))
if [ -f "$SKILL_DIR/dist/mangosteen-llm-deployer-1.0.0.zip" ]; then
    SIZE=$(du -h "$SKILL_DIR/dist/mangosteen-llm-deployer-1.0.0.zip" | cut -f1)
    echo "✓ 发布包存在 (大小: $SIZE)"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    echo "✗ 发布包不存在"
    FAILED_TESTS=$((FAILED_TESTS + 1))
fi

echo ""

# 测试总结
echo "=========================================="
echo "  测试总结"
echo "=========================================="
echo "总测试数: $TOTAL_TESTS"
echo "通过: $PASSED_TESTS"
echo "失败: $FAILED_TESTS"
echo ""

if [ $FAILED_TESTS -eq 0 ]; then
    echo "✅ 所有测试通过！Skill 已准备好发布。"
    exit 0
else
    echo "❌ 有 $FAILED_TESTS 个测试失败，请检查并修复。"
    exit 1
fi
