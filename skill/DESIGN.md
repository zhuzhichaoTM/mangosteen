# Mangosteen Skill 封装方案总结

## 一、封装设计思路

### 1.1 定位与价值

**Skill 名称**：`mangosteen-llm-deployer`

**核心价值**：让 WorkBuddy 能够智能调度本地/云端算力，自动完成模型部署、资源评估、生命周期管理

**触发场景**：
- 用户需要部署本地大模型推理
- 需要评估模型资源需求
- 需要云端算力弹性部署
- 需要 OpenAI 兼容的推理接口

### 1.2 设计原则

1. **本地优先**：遵循 Mangosteen 的核心设计原则，本地资源满足时绝不主动上云
2. **自动化决策**：用户只需说"部署 X 模型"，剩下的资源评估、选型、部署由 skill 自动完成
3. **渐进式交互**：关键决策点（如云端计费）必须向用户展示详细信息并要求确认
4. **跨平台兼容**：Skill 本身是文档，不依赖特定平台，但封装的 Mangosteen 工具支持 macOS/Windows/Linux

### 1.3 核心工作流

1. **前置检查**：环境自检、配置验证
2. **资源评估**：自动计算显存/内存需求
3. **部署决策**：本地足够→本地部署；不足→询问用户是否接受云端
4. **执行部署**：调用 mangosteen 命令
5. **生命周期管理**：停止/恢复/删除

## 二、目录结构

```
~/.workbuddy/skills/mangosteen-llm-deployer/
├── SKILL.md                    # 核心定义文件（必选）
├── README.md                   # Skill 说明文档
├── references/                 # 参考文档（可选）
│   ├── commands.md           # 命令速查表
│   ├── resource-estimation.md # 资源评估公式详解
│   ├── cloud-providers.md    # 云服务商配置指南
│   └── troubleshooting.md    # 故障排查手册
└── scripts/                   # 辅助脚本（可选）
    ├── detect-platform.sh     # 平台检测
    └── health-check.sh        # 环境健康检查
```

### 文件说明

- **SKILL.md**：遵循 WorkBuddy skill 规范，包含 YAML frontmatter（name/description/examples/version/author）和 Markdown 正文
- **references/**：详细参考文档，按需加载，避免 SKILL.md 过于冗长
- **scripts/**：辅助脚本，用于平台检测和环境健康检查

## 三、跨平台支持策略

### 3.1 Skill 层面（完全跨平台）

Skill 本身是纯文本文档（Markdown + Bash 脚本），天然跨平台：
- SKILL.md：纯 Markdown，无平台特定内容
- references/*.md：纯 Markdown，无平台特定内容
- scripts/*.sh：Bash 脚本，需要 Bash 环境

**Bash 脚本兼容性**：
- macOS：自带 Bash（zsh 可调用 bash）
- Linux：自带 Bash
- Windows：需要 Git Bash 或 WSL

### 3.2 Mangosteen 工具层面（完全跨平台）

Mangosteen 原生支持三个平台：

| 平台 | 架构 | 发布包 |
|------|------|--------|
| macOS | Intel (x86_64) | mangosteen-macos-x86_64.tar.gz |
| macOS | Apple Silicon (aarch64) | mangosteen-macos-aarch64.tar.gz |
| Linux | x86_64 | mangosteen-linux-x86_64.tar.gz |
| Linux | aarch64 | mangosteen-linux-aarch64.tar.gz |
| Windows | x86_64 | mangosteen-windows-x86_64.zip |

**跨平台兼容性保证**：
1. **平台差异收敛**：所有 `cfg(target_os)` 都在 `mangosteen-platform` crate 中
2. **路径处理**：统一使用 `PathBuf`，自动处理路径分隔符
3. **进程管理**：通过 `platform::process` 统一封装
4. **禁止 POSIX 假设**：业务代码不出现 `/dev/null`、`/bin/sh`、`~` 展开、`$HOME`、`ps/pgrep/kill` 等

### 3.3 推理引擎集成（跨平台）

Mangosteen 以 llama.cpp 的 `llama-server` 为唯一推理引擎，集成方式与 Ollama 一致：

**查找顺序**：
1. `local.engine_path`（配置指定）
2. 可执行文件同级 `runtime/<平台-架构>/`
3. `PATH`

**库搜索路径注入**：
- macOS：自动注入 `DYLD_FALLBACK_LIBRARY_PATH`
- Linux：自动注入 `LD_LIBRARY_PATH`
- Windows：自动注入 `PATH`

**不需要用户手工设环境变量**，Mangosteen 启动引擎前会自动探测库目录并注入。

### 3.4 配置文件路径（跨平台）

配置文件位置遵循平台规范：

| 平台 | 路径 |
|------|------|
| macOS | `~/Library/Application Support/mangosteen/config.toml` |
| Linux | `~/.config/mangosteen/config.toml` |
| Windows | `%APPDATA%\mangosteen/config.toml` |

Mangosteen 通过 `mangosteen-platform/paths.rs` 统一处理路径规范。

## 四、SKILL.md 核心内容设计

### 4.1 Frontmatter 字段

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
examples_en:
  - "Deploy qwen2.5-7b model with 8192 context"
  - "Assess if llama3.3-70b can run locally"
  - "Find the cheapest cloud instance with 24GB VRAM"
version: 1.0.0
author: "朱志超"
tags: [llm, deployment, local-inference, cloud-compute, ai-infra]
allowed-tools: Bash, Read, Write, Edit
trigger_keywords: [部署模型, 大模型推理, 本地推理, 云端算力, 模型下载, 评估资源, mangosteen]
---
```

### 4.2 正文结构

1. **角色定位**：说明 skill 的能力和边界
2. **前置条件检查**：环境自检流程
3. **核心工作流**：按场景分类（部署/选型/管理/生命周期/自检）
4. **关键概念**：本地优先策略、资源评估公式、云端四维打分
5. **OpenAI 兼容接口**：如何使用标准 OpenAI SDK
6. **配置说明**：关键配置项和位置
7. **跨平台兼容性**：各平台发布包和安装方式
8. **安全与成本提醒**：云端计费警告、凭证安全
9. **Web UI（可选）**：图形界面使用方法
10. **参考文档**：引用 references/ 下的详细文档

### 4.3 使用指令式语言

正文中使用"指令式"语言（动词开头），而不是第二人称：
- ✅ "检查 mangosteen 是否安装"
- ✅ "运行环境自检"
- ❌ "你应该检查 mangosteen 是否安装"
- ❌ "你需要运行环境自检"

## 五、参考文档设计

### 5.1 commands.md

提供完整的命令速查表，按功能分类：
- 核心命令（deploy/run/stop/status/ps/delete）
- 模型管理（models search/pull/list/show/add/remove/verify/sources）
- 资源评估（assess）
- 云端算力（cloud providers/specs/pick/cheapest/instances/destroy）
- 配置管理（config show/init/path/validate）
- 引擎管理（engine env/locate）
- 服务与界面（serve/tui/ui）
- 环境自检（doctor）
- 常用组合

### 5.2 resource-estimation.md

详细解释资源评估公式：
- 总需求公式：权重 + KV Cache + 计算缓冲 + 运行时开销
- 各项详解：决定因素、典型值、计算方式
- KV Cache 的重要性：为什么单独算、GQA 的影响
- 量化自动降档策略：f16 → q8_0 → q4_0 → CPU
- 实际案例：7B/70B 模型的资源需求
- 调度决策：本地优先策略的三个层面

### 5.3 cloud-providers.md

云服务商配置指南：
- 内置服务商：优云智算（UCloud RPC）
- 通用 REST 服务商配置：profile 模板
- 凭证配置方式：环境变量/文件/内联
- 自定义模型源：内网私有仓库
- 调度策略配置：本地优先、四维打分权重
- 云端实例生命周期：stop_service/start_service/delete
- 云端选点示例：pick/cheapest/destroy
- 服务商 API 改版：无需改代码，覆盖 profile 即可

### 5.4 troubleshooting.md

故障排查手册：
- 环境自检：doctor 命令
- 引擎相关问题：动态库找不到、架构不匹配、引擎未装
- 部署失败：查看失败原因、引擎日志、常见失败原因
- 云端实例忘了关：查看/停止/销毁
- 端口被占用：清理残留进程、修改端口范围
- 配置问题：查看/验证/重新生成
- 模型下载问题：下载失败、SHA-256 校验失败、下载很慢
- SSH 连接问题：连接超时、认证失败
- Web UI 问题：无法打开浏览器、前端无法加载
- 性能问题：推理速度慢、内存占用高
- 跨平台问题：macOS/Windows/Linux 特有问题
- 常用诊断命令

## 六、辅助脚本设计

### 6.1 detect-platform.sh

功能：检测当前操作系统和架构，推荐对应的下载文件

输出：
- 操作系统（linux/macos/windows）
- 架构（x86_64/aarch64/i386）
- 推荐的下载文件名
- 是否已安装 mangosteen

兼容性：
- macOS：uname -s 输出 Darwin
- Linux：uname -s 输出 Linux
- Windows：uname -s 输出 MINGW/MSYS/CYGWIN（Git Bash/WSL）

### 6.2 health-check.sh

功能：检查 mangosteen 运行环境是否正常

检查项：
- mangosteen 是否安装
- 配置文件是否存在
- 配置文件是否合法
- 引擎配置是否正常
- 模型目录是否正常
- 端口 11516 是否被占用
- 完整自检是否通过

兼容性：
- 使用 lsof 检查端口（macOS/Linux）
- 使用 netstat 检查端口（Linux/Windows）
- 如果都不存在，跳过端口检查

## 七、WorkBuddy Skill 规范遵循

### 7.1 必需字段

SKILL.md 包含所有必需的 frontmatter 字段：
- name（必选）
- display_name（开放平台要求）
- display_name_en（开放平台要求）
- description（必选）
- description_zh（开放平台要求）
- description_en（开放平台要求）
- examples_zh（开放平台要求）
- examples_en（开放平台要求）
- version（必选）
- author（开放平台要求）

### 7.2 可选字段

- tags：技能标签
- allowed-tools：工具白名单
- trigger_keywords：触发关键词

### 7.3 目录结构

遵循 WorkBuddy skill 规范：
- SKILL.md（必选）
- references/（可选）
- scripts/（可选）
- assets/（可选，本 skill 不需要）

### 7.4 加载策略

遵循三层加载系统：
- Metadata：始终在上下文中（~100 words）
- Skill Body：技能触发时加载（<5k words）
- Bundled Resources：按需加载（无限）

## 八、下一步建议

### 8.1 测试验证

1. 在 WorkBuddy 中触发该 skill，验证是否能正确识别
2. 测试各个工作流（部署/评估/选型/管理）
3. 测试跨平台兼容性（macOS/Windows/Linux）
4. 测试辅助脚本（detect-platform.sh/health-check.sh）

### 8.2 发布到开放平台

如果需要发布到 WorkBuddy 开放平台：
1. 打包成 zip
2. 添加 _meta.json 和 skill-card.md（参考现有 skill）
3. 上传到开放平台进行审核

### 8.3 持续优化

根据使用反馈优化：
- 增加更多工作流场景
- 完善故障排查手册
- 优化命令速查表
- 增加更多辅助脚本

## 九、总结

本封装方案将 Mangosteen 的核心功能封装成 WorkBuddy skill，具有以下特点：

1. **完全跨平台**：Skill 本身跨平台，Mangosteen 工具也支持 macOS/Windows/Linux
2. **遵循规范**：完全遵循 WorkBuddy skill 规范（frontmatter/目录结构/加载策略）
3. **渐进式文档**：SKILL.md 提供核心流程，references/ 提供详细文档
4. **辅助脚本**：提供平台检测和环境健康检查脚本
5. **本地优先**：遵循 Mangosteen 的核心设计原则
6. **安全提醒**：明确提醒云端计费和凭证安全

用户在 WorkBuddy 中只需说"部署 qwen2.5-7b 模型"，skill 就会自动完成资源评估、部署决策、执行部署等全流程。
