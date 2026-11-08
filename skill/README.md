# Mangosteen 大模型部署助手 Skill

## 功能特性

- ✅ 云端大模型部署客户端
- ✅ 自动选择多家云厂商GPU算力资源
- ✅ 最优化选择GPU节点（价格、延迟、可用性、硬件四维打分）
- ✅ 自动化部署开源大模型（支持 llama.cpp 引擎）
- ✅ 通过 SSH 隧道将推理服务映射到本地 OpenAI 兼容接口
- ✅ 跨平台支持（macOS/Windows/Linux）

## 安装

此 skill 已安装在用户级技能目录：
```
~/.workbuddy/skills/mangosteen-llm-deployer/
```

## 使用示例

### 部署模型

```
部署 qwen2.5-7b 模型，上下文 8192
```

### 评估资源

```
评估 llama3.3-70b 能不能在本地跑
```

### 云端选型

```
找一个 24GB 显存的最便宜的云端实例
```

### 模型管理

```
下载 Qwen2.5-7B 模型
```

### 生命周期管理

```
停止所有部署
```

## 前置条件

### macOS

```bash
# 使用 Homebrew
brew install mangosteen
# 或从 Release 下载 mangosteen-macos-x86_64.tar.gz
```

### Linux

```bash
# 从 Release 下载 mangosteen-linux-x86_64.tar.gz
```

### Windows

```bash
# 从 Release 下载 mangosteen-windows-x86_64.zip
```

## 目录结构

```
mangosteen-llm-deployer/
├── SKILL.md                    # 核心定义文件
├── README.md
├── references/                 # 参考文档
│   ├── commands.md           # 命令速查表
│   ├── resource-estimation.md # 资源评估公式
│   ├── cloud-providers.md    # 云服务商配置指南
│   ├── troubleshooting.md    # 故障排查手册
│   └── faq.md             # 命键问题 FAQ
├── scripts/                   # 辅助脚本
│   ├── detect-platform.sh     # 平台检测
│   └── health-check.sh        # 环境健康检查
└── README.md                  # 本文件
```

## 核心概念

### 云端算力选择

节点选择器对每个候选（服务商 × 规格）做四维归一化打分：

```
总分 = w_price·价格分 + w_latency·延迟分 + w_availability·无核节点分 + w_hardware·硬件分
```

| 维度 | 归一化方式 |
|------|-----------|
| 价格 | 候选集内最低价为 1，其它按反比 |
| 延迟 |  琗选集内最低延迟为 1；未探测到给中性 0.�的内核节点 |
| 可用性 | 库存富余度 × 0.6 + 支持按量释放 × 0.2 + 有报价 × 0.2 |
| 硬件 | GPU 型号偏好系数 × 0.5 + 显存富余度 × 0. |
| | 硬件 | GPU 型号、版本（H100/ A100/ L20） |

硬性约束（不满足直接淘汰）：最低显存、最低内存、最低磁盘、价格上限、服务商白名单、地域白名单、库存要求。

### 资源评估

一次云端部署占用的显存/内存由四块构成：

```
总需求 = 权重 + KV Cache + 计算缓冲 + 运行时开销
```

## 安全提醒

⚠️ **云端计费警告**：
- 云端实例按小时计费，用完必须销毁
- 台账会记录所有实例，建议定期检查 `mangosteen  -h all
- 周测规则：同一时刻只允许 1 台 GPU 实例运行，严禁并发创建

⚠️ **凭证安全**：
- 云服务商凭证支持环境变量、文件、内联三种方式
- `config show` 不会明文输出凭证

## 维护与更新

### 一键生成发布件

使用生成脚本自动化生成 skill 发布件：

```bash
bash /Users/zhuzhichao/Documents/mangosteen/docs/workbuddy-skill/scripts/generate.sh
```

该脚本会：
1. 生成元数据文件
2. 镎验必需文件
3. 验证 SKILL.md frontmatter
4. 验证脚本权限
5. 创建发布包
6. 运行自测
7. 生成生成报告

**输出**：
- 发布包：`mangosteen-llm-deployer-1.0.0.zip`
- 生成报告：`generation-report-1.0.0.md`

### 代码仓更新后更新 skill

当 mangosteen 代码仓更新后，使用更新脚本快速更新 skill：

```bash
bash /Users/zhuzhichao/Documents/mangosteen/docs/workbuddy-skill/scripts/update.sh
```

该脚本会：
1. 检查 mangosteen 仓库
2. 拉取最新代码
3. 检查文档更新
4. 更新版本号（可选）
5. 运行生成脚本
6. 生成更新报告

**输出**：
- 更新后的发布包
- 更新报告：`update-report-YYYYMMDD-HHMMSS.md`

### 本地自测

运行自测脚本验证 skill 的有效性和可用性：

```bash
bash /Users/zhuzhichao/Documents/mangosteen/docs/workbuddy-skill/scripts/test-skill.sh
```

### 脚本说明

详细脚本说明请查看：
- `scripts/README.md` - 所有脚本的使用指南

## 相关链接

- Mangosteen 仓库: https://github.com/zhuzhichaoTM/mangosteen
- Mangosteen 文档：见项目 README.md