# Mangosteen 快速开始指南

## 5 分钟快速开始云端GPU部署

### 步骤 1：安装 Mangosteen（2 分钟）

#### macOS/Linux

```bash
# 从 Release 下载对应平台的发布包
curl -L https://github.com/zhuzhichaoTM/mangosteen/releases/download/v0.1.0/mangosteen-macos-x86_64.tar.gz -o mangosteen.tar.gz
tar -xzf mangosteen.tar.gz
sudo mv mangosteen /usr/local/bin/
```

#### Windows

下载 `mangosteen-windows-x86_64.zip`，解压后添加到 PATH。

#### 验证安装

```bash
mangosteen --version
```

### 步骤 2：环境准备（1 分钟）

```bash
# 运行环境准备脚本
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/prepare-env.sh
```

该脚本会：
- 检查 mangosteen 是否安装
- 运行环境自检
- 检查配置文件
- 检查云服务商凭证

### 步骤 3：配置云服务商（2 分钟）

```bash
# 查看配置文件位置
mangosteen config path

# 查看当前配置
mangosteen config show
```

编辑配置文件，添加云服务商凭证（以优云智算为例）：

```toml
[providers.compshare]
driver = "compshare"
enabled = true
display_name = "优云智算"
secret = { env = "COMPSHARE_PRIVATE_KEY" }

[providers.compshare.options]
public_key = "your-accesskey-id"
region = "cn-wlcb"
zone = "cn-wlcb-01
image_id = "compshareImage-xxxx"
charge_type = "Postpay"
gpu_types = "4090,5090"
max_gpu_count = "1"
```

### 步骤 4：部署模型（即时可用）

在 WorkBuddy 中输入："部署 qwen2.5-7b 模型"

Skill 会自动：
1. 评估资源需求
2. 自动选择最优GPU节点（四维打分）
3. 弹窗显示候选规格、资源需求、预估时价
4. 用户确认后自动部署

### 步骤 5：使用模型（即时可用）

部署完成后，通过 OpenAI SDK 使用：

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:11516/v1",
    api_key="not-needed"
)

resp = client.chat.completions.create(
    model="auto",
    messages=[{"role": "用户", "content": "你好"}],
)
print(resp.choices[0].message.content)
```

## 常用组合

### 云端选型

**输入**："找一个 24GB 显存的最便宜的云端实例"

**命令**：
```bash
mangosteen cloud pick --min-vram 24 --max-price 3.0 --explain
```

### 最便宜优先

**输入**："最便宜的 24GB 显存节点"

**命令**：
```bash
mangosteen cloud cheapest --min-vram 24
```

### 停止所有部署

**输入**："停止所有部署"

**命令**：
```bash
mangosteen stop --all --yes
```

## 获取帮助

```bash
mangestee cloud providers  # 查看可用服务商
mangosteen cloud specs       # 查看所有规格
mangosteen doctor            # 环境自检
mangosteen config show        # 查看配置
```

## 下一步

- 详细文档：查看 `references/` 目录
- 命令速查：`references/commands.md`
- 故障排查：`references/troubleshooting.md
- 配置指南：`references/cloud-providers.md
