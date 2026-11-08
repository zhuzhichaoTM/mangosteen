---
name: mangosteen-llm-deployer
display_name: 大模型部署助手
display_name_en: LLM Deployment Assistant
description: 当用户需要部署大模型到云端GPU节点、评估模型资源需求、选择最优GPU节点、或需要 OpenAI 兼容推理接口时使用此技能。支持从多家云厂商中自动选择最优GPU节点（价格、延迟、可用性、硬件四维打分），自动化部署开源大模型（支持 llama.cpp 引擎），通过 SSH 隧道将推理服务映射到本地 OpenAI 兼容接口。
description_zh: 云端大模型部署客户端，自动选择租赁多家云厂商GPU算力资源，最优化选择GPU节点，自动化部署开源大模型。评估模型所需的GPU资源，从多家云厂商中选择最优节点，自动化部署，提供 OpenAI 兼容接口。
description_en: Cloud LLM deployment client that automatically selects optimal GPU nodes from multiple cloud providers, deploys open-source LLMs, and provides OpenAI-compatible API via SSH tunneling.
examples_zh:
  - "部署 qwen2.5-7b 到云端GPU节点"
  - "评估 llama3.3-70b 需要多大显存"
  - "找一个 24GB 显存的最便宜的云端实例"
  - "启动模型推理服务"
  - "停止所有部署"
examples_en:
  - "Deploy qwen2.5-7b to cloud GPU node"
  - "Assess llama3.3-70B VRAM requirements"
  - "Find the cheapest cloud instance with 24GB VRAM"
  - "Start model inference service"
  - "Stop all deployments"
version: 1.0.0
author: "朱志超"
tags: [llm, deployment, cloud-compute, gpu-inference, ai-infra]
allowed-tools: Bash, Read, Write, Edit
trigger_keywords: [部署模型, 云端部署, GPU算力, 云端算力, 模型部署, 评估资源, mangosteen]
---

# Mangosteen 大模型部署助手

## 角色定位

你是大模型部署专家，使用 Mangosteen 工具为用户提供云端GPU节点的模型部署服务。核心原则：**自动选择最优节点，一键部署**。

## 前置条件

### 安装 Mangosteen

#### 方式 1：从 Release 下载（推荐）

1. 访问 https://github.com/zhuzhichaoTM/mangosteen/releases
2. 下载对应平台的发布包：
   - macOS (Intel): `mangosteen-macos-x86_64.tar.gz`
   - macOS (Apple Silicon): `mangosteen-macos-aarch64.tar.gz`
   - Linux (x86_64): `mangosteen-linux-x86_64.tar.gz`
   - Linux (aarch64): `mangosteen-linux-aarch64.tar.gz`
   - Windows (x86_64): `mangosteen-windows-x86_64.zip`
3. 解压到 PATH 目录
4. 验证安装：`mangosteen --version`

#### 方式 2：从源码编译

```bash
git clone https://github.com/zhuzhichaoTM/mangosteen.git
cd mangosteen
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

#### 方式 3：使用辅助脚本检测平台

```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/detect-platform.sh
```

该脚本会自动检测当前平台并推荐对应的下载文件。

#### 方式 4：一键环境准备

```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/prepare-env.sh
```

该脚本会检查 mangosteen 安装状态、配置文件和云服务商凭证。

### 环境检查

在执行任何部署操作前，必须先检查环境：

```bash
# 1. 检查 mangosteen 是否安装
mangosteen --version

# 2. 运行环境自检
mangosteen doctor

# 3. 查看当前配置
mangosteen config show
```

如果 mangosteen 未安装，按照上述安装步骤进行安装。

### 配置云服务商凭证

配置文件位置（`mangosteen config path` 可查）：

| 平台 | 路径 |
|------|------|
| macOS | `~/Library/Application Support/mangosteen/config.toml` |
| Linux | `~/.config/mangosteen/config.toml` |
| Windows | `%APPDATA%\mangosteen\config.toml` |

配置云服务商凭证（示例使用优云智算）：

```toml
[providers.compshare]
driver = "compshare"
enabled = true
display_name = "优云智算"
secret = { env = "COMPSHARE_PRIVATE_KEY" }

[providers.compshare.options]
public_key = "your-accesskey-id"
region = "cn-wlcb"
zone = "cn-wlcb-01"
image_id = "compshareImage-xxxx"   # 必填：从优云智算控制台「镜像」页复制镜像 ID
charge_type = "Postpay"            # 按量计费：关机不收费
gpu_types = "4090,5090"
max_gpu_count = "1"
```

**凭证三种写法**（都不会在 `config show` 里明文输出）：

```toml
secret = { env = "MY_TOKEN" }         # 从环境变量读（推荐）
secret = { file = "/run/secrets/tok" } # 从文件读
secret = { inline = "xxx" }            # 直接内联（仅调试用）
```

## 核心工作流

### 场景 1：部署模型（最常用）

**用户意图**：部署某个模型到云端GPU节点进行推理

**执行步骤**：

1. **资源评估（推荐）**
   ```bash
   # 先评估，不部署
   mangosteen assess <模型名> --context <上下文长度>
   ```
   - 输出会显示：权重大小、KV Cache 需求、总显存需求
   - 提示需要多大的云端显存

2. **执行部署**
   ```bash
   mangosteen deploy <模型名> --context <上下文长度>
   ```
   - 系统会自动选择最优GPU节点（四维打分）
   - 弹窗显示候选规格、资源需求、预估时价
   - 用户确认后才创建实例

3. **验证部署**
   ```bash
   # 查看活跃部署
   mangosteen status

   # 查看 OpenAI 入口
   # 默认：http://127.0.0.1:11516/v1
   ```

**输出格式**：
```
✓ 部署完成：<模型名> (云端算力 · <服务商>)
  落位形态   全部层驻显存（GPU 全量卸载）
  上下文     <上下文长度>
  资源占用   权重 X GiB + KV Y GiB + 计算缓冲 Z GiB = 总计 T GiB
  OpenAI 入口 http://127.0.0.1:11516/v1
```

### 场景 2：云端算力选型

**用户意图**：寻找合适的云端 GPU 实例

**执行步骤**：

1. **查看可用规格**
   ```bash
   # 列出所有服务商和规格
   mangosteen cloud providers
   mangosteen cloud specs
   ```

2. **智能选点（带打分依据）**
   ```bash
   # 按最低显存和价格上限筛选，显示打分依据
   mangosteen cloud pick --min-vram <显存GB> --max-price <元/小时> --explain
   ```
   - 输出示例：
     ```
     ★ autodl/RTX 4090 [cn-west-1] ¥2.380/h · 42ms · 24.0GiB
       价格 1.00 延迟 1.00 可用 0.92 硬件 0.94 → 总分 0.9654
     ```

3. **最便宜优先（不考虑打分）**
   ```bash
   mangosteen cloud cheapest --min-vram <显存GB>
   ```

4. **创建实例（可选）**
   ```bash
   # 一步到位：选最便宜的并创建
   mangosteen cloud cheapest --min-vram <显存GB> --create --yes --name <实例名>

   # 用完销毁
   mangosteen cloud destroy --instance <实例ID> --yes
   ```

**重要提醒**：
- 云端实例按小时计费，用完必须销毁
- 台账会记录所有实例，可用 `mangosteen cloud instances` 查看
- 压测规则：同一时刻只允许 1 台 GPU 实例运行，严禁并发创建

### 场景 3：实例生命周期管理

**用户意图**：停止、恢复、删除部署

**执行步骤**：

1. **查看所有部署**
   ```bash
   mangosteen ps  # 列出全部部署记录
   ```

2. **停止部署**
   ```bash
   # 布止指定部署
   mangosteen stop <部署ID>

   # 停止所有
   mangosteen stop --all --yes
   ```

3. **删除部署**
   ```bash
   # 卸载（云端会销毁实例）
   mangosteen delete <部署ID>
   ```

**云端生命周期注意事项**：
- stop_service：停进程 → 关机 → 关隧道（实例保留）
- delete：销毁实例（回收资源）
- 台账每次状态变更立即落盘，避免忘记关机导致持续扣费

## 关键概念

### 云端算力选择

节点选择器对每个候选（服务商 × 规格）做四维归一化打分：

```
总分 = w_price·价格分 + w_latency·延迟分 + w_availability·可用性分 + w_hardware·硬件分
```

| 维度 | 归一化方式 |
|------|-----------|
| 价格 | 候选集内最低价为 1，其它按反比 |
| 延迟 | 最低延迟为 1；未探测到给中性 0.5 |
| 可用性 | 库存富余度 × 0.6 + 支持按量释放 × 0.2 + 有报价 × 0.2 |
| 硬件 | GPU 型号偏好系数 × 0.5 + 显存富余度 × 0.5 |

硬性约束（不满足直接淘汰）：最低显存、最低内存、最低磁盘、价格上限、服务商白名单、地域白名单、库存要求。

用 `--explain` 可以看到每个候选的明细：

```
$ mangosteen cloud pick --min-vram 24 --explain
★ autodl/REX 4090 [cn-west-1] ¥2.380/h · 42ms · 24.0GiB │ 价格 1.00 延迟 1.00 可用 0.92 硬件 0.94 → 总分 0.9654
  ucloud/RTX 4090 [cn-east-2] ¥2.650/h · 78ms · 24.0GiB │ 价格 0.90 延迟 0.54 可用 0.92 硬件 0.94 → 总分 0.8123
```

### 资源评估

一次云端部署占用的显存/内存由四块构成：

```
总需求 = 权重 + KV Cache + 计算缓冲 + 运行时开销
```

| 组成 | 决定因素 | 典型值（7B Q4_K_M, 8K ctx） |
|------|----------|---------------------------|
| 权重 | 参数量 × 量化位宽（有实测文件时以实测为准） | 4.36 GiB |
| KV Cache | 层数 × KV头数 × head_dim × 上下文 × KV精度 | 0.44 GiB |
| 计算缓冲 | 批大小 × 隐藏维度 | 0.19 GiB |
| 运行时开销 | CUDA context / Metal / 线程栈 | 0.06–0.39 GiB |

**为什么单独算 KV Cache**：消费级显卡上真正卡人的往往是它。
GQA 模型（如 Qwen2.5，28 heads / 4 KV heads）在 32K 上下文只需 ~1.7 GiB；
而不带 GQA 的老模型（如 Llama-2-7B，32 heads / 32 KV heads）8K 就要 4 GiB。
只看"参数量 × 2"会算错一个数量级。

**量化自动降档**：显存放不下时，先降 KV 精度（f16 → q8_0 → q4_0），
再考虑部分卸载，最后才考虑 CPU。每一步都会在 `warnings` 里说明。

## OpenAI 兼容接口

部署完成后，可以使用标准 OpenAI SDK：

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:11516/v1",
    api_key="not-needed"  # Mangosteen 不鉴权
)

resp = client.chat.completions.create(
    model="auto",  # "auto" = 用当前部署的模型
    messages=[{"role": "user", "content": "你好"}],
)
print(resp.choices[0].message.content)
```

## 配置说明

配置文件位置（`mangosteen config path` 可查）：

| 平台 | 路径 |
|------|------|
| macOS | `~/Library/Application Support/mangosteen/config.toml` |
| Linux | `~/.config/mangosteen/config.toml` |
| Windows | `%APPDATA%\mangosteen\config.toml` |

关键配置项：

```toml
[gateway]
port = 11516                # 默认值

[scheduler]
max_fallbacks = 2                     # 降级重试上限
max_price_per_hour = 5.0              # 云端价格上限（元/小时）

[scheduler.weights]
price = 40                            # 四维相对权重，不必加和为 1
latency = 15
availability = 30
hardware = 15

[storage]
models_dir = "~/Library/Application Support/mangosteen/models"
cache_dir = "~/Library/Caches/mangosteen"
verify_hash = false                   # 登记时是否立即算 SHA-256（大文件很慢）
allowed_read_roots = []               # 额外授权访问的目录
```

查看配置：
```bash
mangosteen config show
mangosteen config path
```

## 跨平台兼容性

Mangosteen 完全支持 macOS/Windows/Linux，三个平台都提供：
- macOS (Intel): `mangosteen-macos-x86_64.tar.gz`
- macOS (Apple Silicon): `mangosteen-macos-aarch64.tar.gz`
- Linux (x86_64): `mangosteen-linux-x86_64.tar.gz`
- Linux (aarch64): `mangosteen-linux-aarch64.tar.gz`
- Windows (x86_64): `mangosteen-windows-x64.zip`

**引擎集成**：
- 集成方式与 Ollama 一致：引擎和动态库作为一个整体
- 库搜索路径由 Mangosteen 自动注入（不依赖系统包管理器）
- 查找顺序：`local.engine_path` → 同级 `runtime/<平台-architecture>/` → `PATH`

## 安全与成本提醒

⚠️ **云端计费警告**：
- 云端实例按小时计费，用完必须销毁
- 台账会记录所有实例，建议定期检查 `mangosteen cloud instances`
- 压测规则：同一时刻只允许 1 台 GPU 实例运行，严禁并发创建

⚠️ **凭证安全**：
- 云服务商凭证支持三种写法：
  ```toml
  secret = { env = "MY_TOKEN" }         # 从环境变量读（推荐）
  secret = { file = "/run/secrets/tok" } # 从文件读
  secret = { inline = "xxx" }            # 直接内联（仅调试用）
  ```
- `config show` 不会明文输出凭证

## Web UI（可选）

```bash
mangosteen ui          # 启动控制台并自动打开浏览器
mangosteen ui --no-open  # 只起服务（SSH / 无桌面环境）
```

界面功能：
- **对话**：流式对话；回形针可附加文档（txt/md/csv/代码原生支持，docx/xlsx/pdf 走系统工具）；地球图标开启后消息里的 URL 会被抓取正文
- **部署**：搜索模型仓库 → 下载 → 面端部署 → 停止/删除；云端部署会先弹窗列出**算力规格、资源需求、预估时价与总花费**，勾选确认后才创建实例
- **设备**：CPU / 内存实时曲线、加速卡、磁盘剩余、每个推理进程的 RSS
- **设置**：调度策略、云端推理参数、云服务商开关（凭证只显示「是否已配置」，不回显原文）

> 前端是**内嵌在二进制里的单文件 HTML**，不引 CDN、不引框架、没有构建步骤，
> 所以离线与内网环境也能正常使用，发布件仍约 1.1 MB。

## 参考文档

详细参考文档请查看：
- @references/commands.md - 命令速查表
- @references/resource-estimation.md - 资源评估公式详解
- @references/cloud-providers.md - 云服务商配置指南
- @references/troubleshooting.md - 故障排查手册
- @references/faq.md - 常见问题 FAQ
- QUICKSTART.md - 5 分钟快速开始指南
- CHANGELOG.md - 版本历史

## 快速开始指南

在 WorkBuddy 中输入以下任一关键词即可触发 skill：

1. "部署 qwen2.5-7b 模型" → 自动选择最优节点并部署
2. "评估 llama3.3-70b 需要多大显存" → 评估 GPU 资源需求
3. "找一个 24GB 显存的最便宜的云端实例" → 智能选点
4. "启动模型推理服务" → 启动推理服务
5. "停止所有部署" → 停止所有部署
