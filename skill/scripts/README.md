# Skills 生成脚本使用指南

## 概述

本目录包含用于自动化生成和更新 Mangosteen Skill 发布件的脚本。

## 脚本列表

### 1. generate.sh - 一键生成脚本

**功能**：自动化生成 skill 发布件

**使用方法**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/generate.sh
```

**执行步骤**：
1. 生成元数据文件（_meta.json, _skillhub_meta.json, skill-card.md）
2. 验证必需文件
3. 验证 SKILL.md frontmatter
4. 验证脚本权限
5. 创建输出目录
6. 创建发布包
7. 运行自测
8. 生成生成报告

**输出**：
- 发布包：`dist/mangosteen-llm-deployer-1.0.0.zip`
- 生成报告：`dist/generation-report-1.0.0.md`

### 2. update.sh - 版本更新脚本

**功能**：在代码仓更新后快速更新 skills

**使用方法**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/update.sh
```

**执行步骤**：
1. 检查 mangosteen 仓库
2. 拉取最新代码
3. 检查文档更新
4. 更新版本号（可选）
5. 运行生成脚本
6. 生成更新报告

**输出**：
- 更新后的发布包
- 更新报告：`dist/update-report-YYYYMMDD-HHMMSS.md`

### 3. test-skill.sh - 自测脚本

**功能**：验证 skill 的有效性和可用性

**使用方法**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/test-skill.sh
```

**测试覆盖**：
- 必需文件检查（4 个文件）
- SKILL.md frontmatter 验证（10 个字段）
- references 文档检查（5 个文档）
- scripts 脚本检查（5 个脚本）
- 文档引用检查（5 个引用）
- JSON 文件格式验证（2 个文件）
- 脚本语法检查（5 个脚本）
- 文档字数统计（5 个文档）
- 发布包检查（1 个包）

**输出**：
- 测试结果（通过/失败）
- 测试统计（总数/通过/失败）

### 4. package.sh - 打包验证脚本

**功能**：验证文件完整性并创建发布包

**使用方法**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/package.sh
```

**执行步骤**：
1. 检查必需文件
2. 检查可选目录
3. 验证 SKILL.md frontmatter
4. 验证脚本权限
5. 文件统计
6. 创建发布包

**输出**：
- 发布包：`dist/mangosteen-llm-deployer-1.0.0.zip`

### 5. prepare-env.sh - 环境准备脚本

**功能**：检查 mangosteen 安装状态和配置文件

**使用方法**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/prepare-env.sh
```

**执行步骤**：
1. 检查 mangosteen 是否安装
2. 运行环境自检
3. 检查配置文件
4. 检查 problematic 配置
5. 检查本地模型

### 6. quick-deploy.sh - 快速部署脚本

**功能**：快速评估资源并部署模型

**使用方法**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/quick-deploy.sh <模型名称> [上下文长度]
```

**示例**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/quick-deploy.sh qwen2.5-0.5b-instruct 8192
```

### 7. detect-platform.sh - 平台检测脚本

**功能**：检测当前平台并推荐对应的下载文件

**使用方法**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/detect-platform.sh
```

### 8. health-check.sh - 环境健康检查脚本

**功能**：检查 mangosteen 运行环境是否正常

**使用方法**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/health-check.sh
```

## 使用流程

### 首次生成

```bash
# 1. 一键生成
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/generate.sh

# 2. 查看生成报告
cat ~/.workbuddy/skills/mangosteen-llm-deployer/dist/generation-report-1.0.0.md

# 3. 上传到开放平台
# 访问 WorkBuddy 开放平台，上传 dist/mangosteen-llm-deployer-1.0.0.zip
```

### 代码仓更新后

```bash
# 1. 更新版本
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/update.sh

# 2. 查看更新报告
cat ~/.workbuddy/skills/mangosteen-llm-deployer/dist/update-report-*.md

# 3. 测试新版本
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/test-skill.sh

# 4. 上传到开放平台
```

### 仅验证（不生成）

```bash
# 运行自测
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/test-skill.sh
```

## 配置

### 版本号

版本号在以下文件中需要保持一致：
- SKILL.md（frontmatter）
- _meta.json
- _skillhub_meta.json
- CHANGELOG.md

使用 `update.sh` 脚本时会自动更新所有文件。

### 输出目录

默认输出目录：`dist/`

可在脚本中修改 `OUTPUT_DIR` 变量。

## 常见问题

### Q1: 如何修改版本号？

**A**: 使用 `update.sh` 脚本，会提示输入新版本号并自动更新所有相关文件。

### Q2: 如何自定义元数据？

**A**: 编辑 `generate.sh` 脚本中的元数据生成部分，或直接编辑 `_meta.json` 和 `skill-card.md`。

### Q3: 如何添加新的参考文档？

**A**: 在 `references/` 目录下添加新的 Markdown 文件，然后在 SKILL.md 中添加引用。

### Q4: 如何添加新的脚本？

**A**: 在 `scripts/` 目录下添加新的 Shell 脚本，添加执行权限，并在 `generate.sh` 中添加到打包列表。

### Q5: 如何测试脚本？

**A**: 运行 `test-skill.sh` 进行自动化测试，或在 WorkBuddy 中进行功能测试。

## 维护建议

1. **定期更新**：代码仓更新后及时运行 `update.sh`
2. **版本管理**：每次发布前更新版本号和 CHANGELOG
3. **测试验证**：每次生成后运行 `test-skill.sh` 验证
4. **文档同步**：确保 skill 文档与 mangosteen 文档保持同步
5. **备份发布包**：保留重要版本的发布包

## 自动化建议

可以将 `update.sh` 集成到 CI/CD 流程中：

```yaml
# 示例 GitHub Actions
- name: Update Skill
  run: |
    bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/update.sh

- name: Test Skill
  run: |
    bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/test-skill.sh

- name: Upload Release
  uses: actions/upload-artifact@v2
  with:
    name: skill-release
    path: ~/.workbuddy/skills/mangosteen-llm-deployer/dist/*.zip
```
