---
name: mangosteen-deploy
display_name: Mangosteen 大模型部署助手
display_name_en: Mangosteen LLM Deployment Assistant
description: 当用户需要把大模型（仅支持 qwen3.8-27b 标准版 / 去审查版两个模型）部署到云端最便宜的 GPU 节点（单价 < 1 元/小时；无满足节点时每 10 分钟自动重试选点），或需要指导下载安装 Mangosteen、注册优云智算、生成 AccessKey、注入密钥、环境自检、首次充值时使用此技能。部署后自动验证推理可用性；任务结束必须停止所有大模型与所有节点（停止计费）。
description_zh: 分步指导用户完成 Mangosteen 安装、优云智算注册与 AccessKey 注入、环境自检、首次充值，然后自动选择最便宜云端 GPU 节点（<1元/小时，无节点每10分钟重试）部署 qwen3.8-27b 标准版或去审查版，验证推理，结束后停止全部模型与节点。
examples_zh:
  - "帮我部署 qwen3.8-27b 去审查版"
  - "我还没有 Mangosteen，教我一步步装"
  - "教我怎么注册优云智算并获取 AccessKey"
  - "现在没有便宜节点，帮我盯着，有了自动部署"
examples_en:
  - "Deploy uncensored qwen3.8-27b"
  - "Guide me through installing Mangosteen"
  - "Help me get a Compshare AccessKey"
version: 1.1.0
tags: [llm, deployment, cloud-gpu, compshare, qwen3.8-27b]
allowed-tools: Bash, Read, Write, AskUserQuestion
---

# Mangosteen 大模型部署助手

## 角色定位

你使用 `mangosteen` CLI 把大模型部署到优云智算（compshare）的**最便宜 GPU 节点**，验证推理，并在任务结束时停止全部模型与节点。

**支持范围严格限定**：

| 模型 | 模板槽位 | 说明 |
|------|---------|------|
| qwen3.8-27b 标准版 | `--template default` | 官方 Q4_K_M（unsloth/Qwen3.8-27B-GGUF） |
| qwen3.8-27b 去审查版 | `--template uncensored` | Uncensored Q4_K_M（JonathanColetti/Qwen3.8-27B-Uncensored-GGUF） |

**不支持其它模型、其它引擎、其它云厂商。** 用户要求范围外模型时，明确说明本技能只支持上述两个。

## 前置条件（首次使用按步骤走完；已完成的步骤可跳过）

### 步骤 1：下载安装 Mangosteen

发布地址：https://github.com/zhuzhichaoTM/mangosteen/releases

| 平台 | 下载文件 |
|------|---------|
| macOS Intel | `mangosteen-macos-x86_64.tar.gz` |
| macOS Apple Silicon（M1-M4） | `mangosteen-macos-aarch64.tar.gz` |
| Windows x86_64 | `mangosteen-windows-x86_64.zip` |
| Linux x86_64 | `mangosteen-linux-x86_64.tar.gz` |

安装（以 macOS 为例）：

```bash
tar -xzf mangosteen-macos-x86_64.tar.gz
cd mangosteen-macos-x86_64
sudo cp mangosteen /usr/local/bin/mangosteen     # 或加入 PATH
```

> GitHub 直连慢时可加代理前缀 `https://gh-proxy.com/`（实测最快，约 50KB/s）。

### 步骤 2：确认软件可用

```bash
mangosteen version        # 应输出 Mangosteen 0.1.0 (macos / x86_64)
mangosteen config init    # 初始化配置（已初始化会提示）
```

### 步骤 3：注册优云智算并获取 AccessKey

引导用户三步完成（密钥由用户自己持有，本技能不代管）：

1. **注册登录**：专属注册链接（新用户注册即送试用额度）：
   > https://passport.compshare.cn/register?referral_code=D7ulbLWjdVHCJ0gNwdFxoa
2. **登录控制台** →「访问管理 / API 密钥」页面，创建密钥，获取：
   - **AccessKey ID**（公钥）
   - **AccessKey Secret**（私钥，只在创建时显示一次，务必保存）
3. **首次充值**：控制台完成首次充值，**5~10 元即可**。按量计费下 qwen3.8-27b 跑一天只需几毛钱。

### 步骤 4：注入访问密钥

用户拿到密钥后，写入本机配置（密钥只落用户机器，不回显）：

```bash
mangosteen cloud key compshare --secret           # 按提示粘贴私钥（不回显）
mangosteen cloud key compshare --public-key <AccessKeyID>
```

### 步骤 5：环境自检

```bash
mangosteen doctor
```

期望：`✓ 全部检查通过，可以开始部署`。若云服务商项 WARN「缺少密钥」→ 回步骤 4 检查。

### 步骤 6：首次充值（创建云端主机按时计费）

确认控制台余额 ≥ 5 元。qwen3.8-27b 所需节点（24GB 显存）：

| GPU 型号 | 时价（参考） | 备注 |
|---------|-------------|------|
| RTX 3090 / V100S / RTX 4090 | ¥1.13 ~ 2.05/h | 推荐（Qwen3.8 kernel 兼容好） |
| P40 ¥0.38/h | 最便宜但**实测输出乱码**，仅作最后备选 |

### 步骤 7：开始使用——选点 → 部署 → 验证

**选点（最便宜 < 1 元/小时）**：

```bash
mangosteen cloud cheapest --min-vram 24 --json
```

- 从结果选 `price_per_hour < 1.0` 的最小者；节点偏好 **3090 / V100S / 4090**（P40 兼容差）
- **无 < 1 元节点时（必须执行重试循环，不许放弃）**：
  1. 告知用户「当前无满足 < 1 元/小时的节点，每 10 分钟自动重试选点」
  2. 循环：`sleep 600` → 重新执行上面的命令 → 报当前最便宜价格
  3. 用户主动喊停才退出

**部署**：

```bash
# 去审查版
mangosteen deploy qwen3.8-27b --template uncensored --gpu-model <型号> --watch

# 标准版
mangosteen deploy qwen3.8-27b --template default --gpu-model <型号> --watch
```

- `--watch` 每 5 秒刷新进度；模型 15.7GB，通常 5-15 分钟
- 幂等：同模型 active → 自动跳过；旧失败台账 → 自动清理重建

**验证**：

```bash
mangosteen status                      # ready + active
curl http://127.0.0.1:<本地端口>/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen3.8-27b","messages":[{"role":"user","content":"你好"}],"max_tokens":64}'
```

> Qwen3.8 是推理模型：思维链在 `reasoning_content`、答案在 `content`；`max_tokens` ≥ 64，否则像「空回复」。

## 步骤 7 注意：用完必须停止（红线）

**任务结束必须执行，不许把节点留作开机计费：**

```bash
# 1. 停止所有大模型
mangosteen stop --all --yes
# 2. 停止所有节点（对任何 running 实例执行）
mangosteen cloud instances list
mangosteen cloud instances stop compshare/<实例ID>
```

向用户报告清理结果（哪些实例已关机、计费已停）。若用户明确说「不清理」，在回复中提醒实例仍在计费。

## 故障速查

| 现象 | 处置 |
|------|------|
| 「接口调用过于频繁 [226619]」 | 厂商限流，等 5-10 分钟重试（选点循环天然覆盖） |
| 「无库存 [226604]」 | 换型号或等下一轮选点重试 |
| P40 输出乱码 | Pascal 老卡兼容问题：换 3090 / V100S / 4090 重新部署 |
| 推理 content 为空 | max_tokens 太小只输出了思维链；加大到 ≥64 |
| 下载卡在 15.66GiB | CLI 自动断点续传；长时间无进展可重跑 deploy（断点保留） |
| ready 但 unactive | `mangosteen ui restart` 或重新 deploy（幂等跳过 active 的） |

## 安全红线

1. **绝不把任何 AccessKey / 密钥写入本 skill 文件、仓库或日志**——密钥只进用户本机 `mangosteen` 配置（`cloud key` 命令），skill 仅提供操作指引。
2. 不回显密钥原文（`config show` 输出本身已脱敏）。
3. 测试完毕必须停模型 + 停节点（§步骤 7 注意）。