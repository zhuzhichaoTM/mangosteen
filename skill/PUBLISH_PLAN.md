# Mangosteen Skill 发布方案分析

## 一、当前状态

### 1.1 已有文件 ✅

```
~/.workbuddy/skills/mangosteen-llm-deployer/
├── SKILL.md                              ✅ 核心定义文件
├── README.md                             ✅ Skill 说明文档
├── DESIGN.md                             ✅ 封装设计文档
├── references/                           ✅ 参考文档
│   ├── commands.md                      ✅ 命令速查表
│   ├── resource-estimation.md            ✅ 资源评估公式详解
│   ├── cloud-providers.md               ✅ 云服务商配置指南
│   └── troubleshooting.md               ✅ 故障排查手册
└── scripts/                              ✅ 辅助脚本
    ├── detect-platform.sh                ✅ 平台检测
    └── health-check.sh                   ✅ 环境健康检查
```

### 1.2 SKILL.md Frontmatter 检查 ✅

```yaml
---
name: mangosteen-llm-deployer
display_name: 大模型部署助手
display_name_en: LLM Deployment Assistant
description: 当用户需要部署本地大模型、评估模型资源需求、使用云端算力弹性部署、或需要 OpenAI 兼容推理接口时使用此技能。
description_zh: 个人级大模型部署客户端，本地算力优先+云端弹性补位。
description_en: Personal LLM deployment client with local-first compute and cloud elasticity.
examples_zh:
  - "部署 qwen2.5-7b 模型，上下文 8192"
  - "评估 llama3.3-70b 能不能在本地跑"
  - "找一个 24GB 显存的最便宜的云端实例"
  - "启动模型推理服务"
  - "停止所有部署"
examples_en:
  - "Deploy qwen2.5-7b model with 8192 context"
  - "Assess if llama3.3-70b can run locally"
  - "Find the cheapest cloud instance with 24GB VRAM"
  - "Start model inference service"
  - "Stop all deployments"
version: 1.0.0
author: "朱志超"
tags: [llm, deployment, local-inference, cloud-compute, ai-infra]
allowed-tools: Bash, Read, Write, Edit
trigger_keywords: [部署模型, 大模型推理, 本地推理, 云端算力, 模型下载, 评估资源, mangosteen]
---
```

**检查结果**：
- ✅ name：符合规范
- ✅ display_name/display_name_en：符合开放平台要求
- ✅ description/description_zh/description_en：符合开放平台要求
- ✅ examples_zh/examples_en：符合开放平台要求
- ✅ version：符合规范
- ✅ author：符合开放平台要求
- ✅ tags：有助于分类
- ✅ allowed-tools：可选，已填写
- ✅ trigger_keywords：有助于触发

## 二、缺失内容分析

### 2.1 发布元数据文件 ❌

#### 缺失文件

1. **_meta.json**（发布元数据）
   - 作用：记录 skill 的发布信息
   - 必需字段：ownerId, slug, version, publishedAt
   - 参考：system-architect-agent 的 _meta.json

2. **_skillhub_meta.json**（SkillHub 元数据）
   - 作用：记录从 SkillHub 安装的信息
   - 必需字段：slug, name, version, installedAt, source
   - 参考：system-architect-agent 的 _skillhub_meta.json

3. **skill-card.md**（技能卡片）
   - 作用：在技能市场展示的详细说明
   - 必需字段：Description, Publisher, License/Terms of Use, Use Case, Deployment Geography, Known Risks and Mitigations, Reference(s), Skill Output, Skill Version(s), Ethical Considerations
   - 参考：system-architect-agent 的 skill-card.md

4. **clawhub.json**（ClawHub 配置，可选）
   - 作用：发布到 ClawHub 时的配置
   - 当前不需要（如果只在 WorkBuddy 开放平台发布）

### 2.2 图标资源 ❌

#### 缺失内容

1. **图标文件**
   - 作用：在技能市场展示
   - 推荐格式：PNG/SVG
   - 推荐尺寸：512x512 px
   - 位置：assets/icon.png 或 icon.svg

2. **截图/演示图**（可选）
   - 作用：展示 skill 的使用效果
   - 位置：assets/screenshots/

### 2.3 文档补充 ⚠️

#### 需要补充的内容

1. **SKILL.md 中的快速开始指南**
   - 当前：已有完整工作流
   - 缺失：针对不同场景的快速开始（纯本地/云端/混合）

2. **前置条件说明**
   - 当前：有前置条件检查章节
   - 缺失：明确的 mangosteen 安装指引（下载链接/编译指引）

3. **配置示例**
   - 当前：在 references/cloud-providers.md 中有配置示例
   - 缺失：针对不同场景的完整配置文件模板

4. **常见问题 FAQ**
   - 当前：在 references/troubleshooting.md 中有故障排查
   - 缺失：用户常见问题的 FAQ

### 2.4 测试验证 ⚠️

#### 需要补充的内容

1. **测试用例**
   - 本地部署测试
   - 云端部署测试（需要凭证）
   - 模型管理测试
   - 故障恢复测试

2. **测试文档**
   - 测试环境要求
   - 测试步骤
   - 预期结果

### 2.5 发布打包 ⚠️

#### 需要补充的内容

1. **打包脚本**
   - 自动生成 zip 包
   - 自动验证文件完整性
   - 自动生成元数据文件

2. **版本管理**
   - CHANGELOG.md
   - 版本发布流程

## 三、内容不足分析

### 3.1 SKILL.md 内容不足

#### 不足 1：环境准备章节不够详细

**当前**：
```markdown
## 前置条件检查

在执行任何部署操作前，必须先检查环境：
```

**问题**：
- 没有说明 mangosteen 从哪里获取
- 没有说明如何安装 mangosteen
- 没有说明如何将 mangosteen 添加到 PATH
- 没有说明不同平台的安装差异

**建议补充**：
```markdown
## 前置条件

### 安装 Mangosteen

#### 方式 1：从 Release 下载（推荐）

1. 访问 https://github.com/zhuzhichaoTM/mangosteen/releases
2. 下载对应平台的发布包：
   - macOS (Intel): mangosteen-macos-x86_64.tar.gz
   - macOS (Apple Silicon): mangosteen-macos-aarch64.tar.gz
   - Linux (x86_64): mangosteen-linux-x86_64.tar.gz
   - Linux (aarch64): mangosteen-linux-aarch64.tar.gz
   - Windows (x86_64): mangosteen-windows-x86_64.zip
3. 解压到 PATH 目录
4. 验证安装：mangosteen --version

#### 方式 2：从源码编译

```bash
git clone https://github.com/zhuzhichaoTM/mangosteen.git
cd mangosteen
cargo build --release
export PATH="$PWD/target/release:$PATH"
```
```

#### 不足 2：缺少配置文件模板

**当前**：
```markdown
### 配置说明

配置文件位置：
- macOS: `~/Library/Application Support/mangosteen/config.toml`
- Linux: `~/.config/mangosteen/config.toml`
- Windows: `%APPDATA%\mangosteen/config.toml`
```

**问题**：
- 没有提供完整的配置文件模板
- 用户不知道从哪里开始配置

**建议补充**：
```markdown
### 配置文件模板

#### 最小配置（纯本地 CPU 推理）

```toml
config_version = 1

[gateway]
port = 11516
host = "127.0.0.1"
require_api_key = false

[scheduler]
local_first = true
strategy = "cheapest"
max_fallbacks = 2

[local]
default_context = 8192
allow_cpu_fallback = true
engine = "llama_cpp"

[storage]
models_dir = "~/Library/Application Support/mangosteen/models"
default_source = "modelscope"
```

#### 云端配置（启用优云智算）

```toml
config_version = 1

[providers.compshare]
driver = "compshare"
enabled = true
display_name = "优云智算"

[providers.compshare.secret]
env = "COMPSHARE_PRIVATE_KEY"

[providers.compshare.options]
public_key = "your-public-key"
region = "cn-wlcb"
zone = "cn-wlcb-01"
image_id = "compshareImage-xxxx"
charge_type = "Postpay"
```
```

#### 不足 3：缺少故障排查的快速指引

**当前**：
- 有详细的故障排查文档（references/troubleshooting.md）
- 但在 SKILL.md 中没有快速指引

**建议补充**：
```markdown
## 常见问题快速排查

### 问题 1：部署失败 - 无可用云端算力节点

**原因**：配置文件中强制使用 SSH，但没有配置设备

**快速解决**：
1. 检查配置：`mangosteen config show`
2. 简化配置：移除 `model_whitelist` 和 `remote_devices`
3. 重新部署：`mangosteen deploy <模型>`

### 问题 2：引擎动态库找不到

**原因**：llama-server 的动态库路径未注入

**快速解决**：
1. 查看注入路径：`mangosteen engine env`
2. 检查引擎位置：`mangosteen engine locate`
3. 手动指定（如果需要）

### 问题 3：端口被占用

**快速解决**：
```bash
mangosteen stop --all
```
```

### 3.2 references 文档不足

#### 不足 1：缺少快速参考

**当前**：
- commands.md 有完整的命令速查表
- 但缺少最常用的 5 个命令的快速参考

**建议补充**：
```markdown
## 最常用的 5 个命令

1. `mangosteen deploy <模型>` - 部署模型
2. `mangosteen status` - 查看状态
3. `mangosteen stop --all` - 停止所有部署
4. `mangosteen models list` - 查看本地模型
5. `mangosteen doctor` - 环境自检
```

#### 不足 2：缺少配置最佳实践

**当前**：
- cloud-providers.md 有配置示例
- 但缺少针对不同场景的最佳实践

**建议补充**：
```markdown
## 配置最佳实践

### 场景 1：个人开发者（本地 CPU）

```toml
[local]
allow_cpu_fallback = true
default_context = 8192
```

### 场景 2：有 GPU 的开发者

```toml
[local]
allow_cpu_fallback = false
kv_cache = "f16"
```

### 场景 3：企业用户（云端优先）

```toml
[scheduler]
local_first = false
max_price_per_hour = 10.0
```
```

### 3.3 scripts 脚本不足

#### 不足 1：缺少环境准备脚本

**当前**：
- detect-platform.sh - 平台检测
- health-check.sh - 环境健康检查

**建议补充**：
```bash
scripts/prepare-env.sh - 环境准备脚本
```

功能：
- 检查 mangosteen 是否安装
- 检查配置文件是否存在
- 检查配置文件是否有问题配置
- 提供修复建议

#### 不足 2：缺少一键部署脚本

**建议补充**：
```bash
scripts/quick-deploy.sh - 快速部署脚本
```

功能：
- 评估资源
- 选择部署方式（本地/云端）
- 执行部署
- 验证部署结果

### 3.4 skill-card.md 内容不足

#### 需要补充的字段

根据 system-architect-agent 的 skill-card.md，需要补充：

1. **Publisher**：发布者信息
   - 当前：朱志超
   - 建议：添加 GitHub 链接或组织信息

2. **License/Terms of Use**：许可证
   - 当前：未声明
   - 建议：Apache-2.0（与 mangosteen 项目一致）

3. **Use Case**：使用场景
   - 当前：有 description
   - 建议：更详细的使用场景说明

4. **Deployment Geography**：部署地域
   - 当前：未声明
   - 建议：Global（无地域限制）

5. **Known Risks and Mitigations**：已知风险和缓解措施
   - 当前：未声明
   - 建议：云端计费风险、凭证安全风险

6. **Reference(s)**：参考链接
   - 当前：未声明
   - 建议：GitHub 仓库、文档链接

7. **Skill Output**：技能输出
   - 当前：未声明
   - 建议：Text, Markdown, Configuration

8. **Ethical Considerations**：伦理考虑
   - 当前：未声明
   - 建议：用户应评估技能适用性

## 四、发布方案

### 方案 A：最小化发布（快速验证）

#### 步骤

1. **生成必需的元数据文件**
   - _meta.json
   - _skillhub_meta.json
   - skill-card.md

2. **补充 SKILL.md 的前置条件章节**
   - 添加 mangosteen 安装指引
   - 添加配置文件模板

3. **补充 skill-card.md**
   - 填充所有必需字段
   - 添加许可证声明

4. **打包验证**
   - 打包成 zip
   - 验证文件完整性
   - 上传到开放平台测试

#### 优点
- 快速发布
- 最小改动
- 快速验证

#### 缺点
- 文档不够详细
- 缺少图标
- 缺少快速参考

### 方案 B：完整发布（推荐）

#### 步骤

1. **生成必需的元数据文件**
   - _meta.json
   - _skillhub_meta.json
   - skill-card.md

2. **补充 SKILL.md**
   - 详细的前置条件说明
   - 配置文件模板
   - 常见问题快速排查

3. **补充 references 文档**
   - 快速参考（commands.md）
   - 配置最佳实践（cloud-providers.md）
   - FAQ（新建 references/faq.md）

4. **补充 scripts**
   - prepare-env.sh
   - quick-deploy.sh

5. **添加图标**
   - 设计 skill 图标
   - 放置到 assets/icon.png

6. **补充文档**
   - QUICKSTART.md（快速开始）
   - CHANGELOG.md（版本历史）

7. **打包验证**
   - 打包成 zip
   - 文件完整性验证
   - 功能测试
   - 上传到开放平台

#### 优点
- 文档完整
- 用户体验好
- 易于维护

#### 缺点
- 工作量较大
- 需要设计图标

### 方案 C：分阶段发布

#### 阶段 1：最小化发布（方案 A）
- 快速验证流程
- 收集用户反馈

#### 阶段 2：补充文档
- 补充 SKILL.md
- 补充 references
- 补充 scripts

#### 阶段 3：完善体验
- 添加图标
- 添加快速开始
- 添加 FAQ

#### 优点
- 循序渐进
- 风险可控
- 用户反馈驱动

#### 缺点
- 周期较长
- 需要多次发布

## 五、评审建议

### 5.1 立即执行（最小化发布）

**需要补充的内容**：
1. _meta.json
2. _skillhub_meta.json
3. skill-card.md
4. SKILL.md 前置条件章节（mangosteen 安装指引）

**预计工作量**：30 分钟

**风险评估**：低

### 5.2 建议执行（完整发布）

**需要补充的内容**：
1. 所有元数据文件
2. SKILL.md 完整补充
3. references 文档补充
4. scripts 补充
5. 图标设计
6. 快速开始文档

**预计工作量**：2-3 小时

**风险评估**：中

### 5.3 暂缓执行（分阶段发布）

**第一阶段**：最小化发布（方案 A）
**第二阶段**：根据用户反馈补充

**预计工作量**：分阶段累计 3-4 小时

**风险评估**：低

## 六、推荐方案

### 推荐：方案 B（完整发布）

**理由**：
1. **一次性投入，长期受益**：完整发布后维护成本低
2. **用户体验好**：详细的文档和脚本降低使用门槛
3. **专业形象**：完整的元数据和图标提升可信度
4. **便于推广**：完整的 skill-card 便于在市场展示

**执行顺序**：
1. 先补充必需的元数据文件（1 小时）
2. 补充 SKILL.md 和 references（1 小时）
3. 补充 scripts（30 分钟）
4. 设计图标（30 分钟，可简化）
5. 打包验证（30 分钟）

**总计**：约 3.5 小时

## 七、评审问题

请评审以下问题：

1. **发布范围**：
   - 只在 WorkBuddy 开放平台发布？
   - 还是要发布到 ClawHub？

2. **图标设计**：
   - 是否需要设计专用图标？
   - 还是可以暂时使用默认图标？

3. **文档详细程度**：
   - 当前文档是否足够详细？
   - 是否需要补充更多示例？

4. **脚本需求**：
   - 是否需要 prepare-env.sh 和 quick-deploy.sh？
   - 还是当前的 detect-platform.sh 和 health-check.sh 足够？

5. **发布时机**：
   - 是否现在就发布？
   - 还是等 mangosteen 配置问题解决后再发布？

请根据以上分析，选择合适的方案并决定是否执行。
