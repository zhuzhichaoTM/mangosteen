---
name: mangosteen-deploy
description: Deploy large language models (qwen3.8-27b standard or uncensored) to the cheapest cloud GPU node (under CNY 1/hour, retry every 10 minutes when none available), and guide users through installing Mangosteen, registering Compshare, generating and injecting AccessKey, doctor self-check, and first top-up. Use when the user wants to deploy an LLM to a cloud GPU, install Mangosteen, obtain cloud credentials, or stop all models and nodes after use. Stop all models and nodes when the task ends.
description_zh: 将大模型（qwen3.8-27b 标准版/去审查版）部署到云端最便宜 GPU 节点（单价 < 1 元/小时，无满足节点时每 10 分钟自动重试选点）；分步指导用户下载安装 Mangosteen、注册优云智算、生成并注入 AccessKey、doctor 环境自检、首次充值 5-10 元；部署后验证推理，任务结束必须停止所有大模型与所有节点（停止计费）。
description_en: Deploy LLMs to the cheapest cloud GPU node (under CNY 1/hour, auto-retry every 10 minutes), guide through Mangosteen install, Compshare registration, AccessKey injection, doctor check, and first top-up. Stop all models and nodes when done.
version: 1.2.0
agent_created: true
tags: [llm, deployment, cloud-gpu, compshare, qwen3.8-27b]
---

# Mangosteen 大模型部署助手

## 用途

使用 `mangosteen` CLI 把大模型部署到优云智算（compshare）的最便宜 GPU 节点，验证推理，任务结束时停止全部模型与节点。**仅支持 qwen3.8-27b 标准版与去审查版两个模型**，不支持其它模型、引擎、云厂商；范围外请求要明确说明。

## 前置条件（首次使用逐步执行；已完成步骤可跳过）

### 步骤 1：下载安装 Mangosteen（macOS / Windows / Linux）

发布地址：https://github.com/zhuzhichaoTM/mangosteen/releases

| 平台 | 下载文件 |
|------|---------|
| macOS Intel | `mangosteen-macos-x86_64.tar.gz` |
| macOS Apple Silicon（M1-M4） | `mangosteen-macos-aarch64.tar.gz` |
| Windows x86_64 | `mangosteen-windows-x86_64.zip` |
| Linux x86_64 | `mangosteen-linux-x86_64.tar.gz` |

- macOS：解压后 `sudo cp mangosteen /usr/local/bin/`（或加入 PATH）
- Windows：解压后将目录加入系统 PATH（`设置 → 系统 → 系统信息 → 高级系统设置 → 环境变量`），重开终端验证
- GitHub 直连慢时可加代理前缀 `https://gh-proxy.com/`（实测最快，约 50KB/s）

一键检测平台与安装指引：`bash scripts/setup.sh`（自动识别 macOS/Windows/Linux 并给出对应步骤）。

### 步骤 2：确认软件可用

```bash
mangosteen version        # 应输出 Mangosteen 0.1.0 (平台 / 架构)
mangosteen config init    # 初始化配置（已初始化会提示）
```

### 步骤 3：注册优云智算并获取 AccessKey

1. 通过专属注册链接注册（新用户注册即送试用额度）：
   > https://passport.compshare.cn/register?referral_code=D7ulbLWjdVHCJ0gNwdFxoa
2. 登录控制台 →「访问管理 / API 密钥」创建密钥，获取 **AccessKey ID**（公钥）与 **AccessKey Secret**（私钥，只显示一次，务必保存）
3. 首次充值 5~10 元即可（按量计费下 qwen3.8-27b 一天仅几毛钱）

### 步骤 4：注入访问密钥

```bash
mangosteen cloud key compshare --secret           # 按提示粘贴私钥（不回显）
mangosteen cloud key compshare --public-key <AccessKeyID>
```

密钥只写用户本机配置；绝不回显、绝不写入本 skill。

### 步骤 5：环境自检

```bash
mangosteen doctor
```

期望「✓ 全部检查通过」。云服务商项 WARN「缺少密钥」→ 回步骤 4。

### 步骤 6：首次充值确认

控制台余额 ≥ 5 元。节点时价参考：RTX 3090 / V100S / RTX 4090（24GB 显存）¥1.13~2.05/h；**P40 ¥0.38/h 最便宜但实测输出乱码，仅作最后备选**。

### 步骤 7：选点 → 部署 → 验证 → 停止

**选点（最便宜 < 1 元/小时）**：

```bash
mangosteen cloud cheapest --min-vram 24 --json
```

- 选 `price_per_hour < 1.0` 的最小者；节点偏好 3090 / V100S / 4090
- **无 < 1 元节点时（必须重试循环，不许放弃）**：告知用户「每 10 分钟自动重试选点」→ 循环 `sleep 600` → 重查并报当前最便宜价格 → 用户喊停才退出

**部署（两模型二选一）**：

```bash
mangosteen deploy qwen3.8-27b --template uncensored --gpu-model <型号> --watch   # 去审查版
mangosteen deploy qwen3.8-27b --template default --gpu-model <型号> --watch     # 标准版
```

- `--watch` 每 5 秒刷新进度；模型 15.7GB，通常 5-15 分钟
- 幂等：同模型 active → 自动跳过；旧失败台账 → 自动清理重建

**验证**：

```bash
mangosteen status
curl http://127.0.0.1:<本地端口>/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen3.8-27b","messages":[{"role":"user","content":"你好"}],"max_tokens":64}'
```

> Qwen3.8 是推理模型：思维链在 `reasoning_content`、答案在 `content`；`max_tokens` ≥ 64，否则像「空回复」。

**步骤 7 红线——任务结束必须停止**：

```bash
mangosteen stop --all --yes                    # 停所有大模型
mangosteen cloud instances list                # 查所有实例
mangosteen cloud instances stop compshare/<实例ID>   # 对任何 running 实例执行
```

报告清理结果并确认计费已停；用户明确说「不清理」时仍要提醒实例在计费。

## 故障速查

| 现象 | 处置 |
|------|------|
| 「接口调用过于频繁 [226619]」 | 厂商限流，等 5-10 分钟重试 |
| 「无库存 [226604]」 | 换型号或等下一轮选点 |
| P40 输出乱码 | Pascal 老卡兼容问题；换 3090 / V100S / 4090 |
| 推理 content 为空 | max_tokens 太小只出了思维链；加大到 ≥64 |
| 下载卡在 15.66GiB | CLI 自动断点续传；无进展可重跑 deploy |
| ready 但 unactive | `mangosteen ui restart` 或重新 deploy |
| Windows 下命令找不到 | 确认解压目录已加入 PATH 并重开终端 |

## 安全红线

1. 绝不把任何 AccessKey / 密钥写入本 skill 文件、仓库或日志——密钥只进用户本机配置（`cloud key` 命令）
2. 不回显密钥原文（`config show` 输出本身已脱敏）
3. 测试完毕必须停模型 + 停节点