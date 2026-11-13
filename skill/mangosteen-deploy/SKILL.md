---
name: mangosteen-deploy
display_name: Mangosteen 大模型部署助手
display_name_en: Mangosteen LLM Deployment Assistant
description: 当用户需要把大模型（仅支持 qwen3.8-27b 标准版 / 去审查版两个模型）部署到云端最便宜的 GPU 节点（单价 < 1 元/小时；无满足节点时每 10 分钟自动重试选点），或需要指导注册优云智算、首次充值、生成 AccessKey 时使用此技能。部署后自动验证推理可用性；任务结束必须删除模型部署并关闭所有云端实例（停止计费）。
description_zh: 自动选择最便宜的云端 GPU 节点（<1元/小时，无节点每10分钟重试），部署 qwen3.8-27b 标准版或去审查版，指导用户完成优云智算注册/充值/AccessKey 生成，部署后验证推理并在结束时清理全部云端资源。
examples_zh:
  - "部署 qwen3.8-27b 标准版到最便宜的节点"
  - "帮我部署去审查版 qwen3.8-27b"
  - "教我怎么获取优云智算 AccessKey"
  - "现在没有便宜节点，帮我盯着，有了自动部署"
examples_en:
  - "Deploy qwen3.8-27b standard to the cheapest node"
  - "Deploy uncensored qwen3.8-27b"
  - "Guide me to get a Compshare AccessKey"
version: 1.0.0
tags: [llm, deployment, cloud-gpu, compshare, qwen3.8-27b]
allowed-tools: Bash, Read, Write, AskUserQuestion
---

# Mangosteen 大模型部署助手

## 角色定位

你使用 `mangosteen` CLI 把大模型部署到优云智算（compshare）的**最便宜 GPU 节点**，验证推理，并在任务结束时清理全部云端资源。**支持范围严格限定**：

| 模型 | 模板槽位 | 说明 |
|------|---------|------|
| qwen3.8-27b 标准版 | `--template default` | 官方 Q4_K_M（unsloth/Qwen3.8-27B-GGUF） |
| qwen3.8-27b 去审查版 | `--template uncensored` | Uncensored Q4_K_M（JonathanColetti/Qwen3.8-27B-Uncensored-GGUF） |

**不支持其它模型、其它引擎、其它云厂商。** 用户要求范围外模型时，明确说明本技能只支持上述两个。

## 安全红线（必须遵守）

1. **绝不把任何 AccessKey / 密钥写入本 skill 文件、仓库或日志**。密钥只允许写进用户本机 `mangosteen` 配置（`cloud key` 命令），skill 仅提供操作指引。
2. 用户密钥只在当次会话中使用；不得回显密钥原文（`config show` 输出本身已脱敏）。
3. 测试完毕**必须**删除模型部署并关闭所有实例（见 §7 清理），不得把节点留作开机计费。

## 前置条件

mangosteen 已安装（`mangosteen version` 可执行）。未安装时引导用户从 GitHub Releases 下载：
- 地址：https://github.com/zhuzhichaoTM/mangosteen/releases
- macOS Intel：`mangosteen-macos-x86_64.tar.gz`；Apple Silicon：`mangosteen-macos-aarch64.tar.gz`
- 解压后 `sudo cp mangosteen /usr/local/bin/` 或加入 PATH

## 工作流

### §1 环境检查

```bash
mangosteen version                 # 确认已安装
mangosteen config init             # 初始化配置（已初始化会提示）
mangosteen cloud providers         # 确认 compshare 行显示「启用」
```

`config.toml` 位置：`~/Library/Application Support/mangosteen/config.toml`（macOS）/ `%APPDATA%\mangosteen\config.toml`（Windows）/ `~/.config/mangosteen/config.toml`（Linux）。

### §2 凭证指导（用户尚无 AccessKey 时）

引导用户三步完成：

1. **注册并登录**：通过专属注册链接注册优云智算账号（新用户注册即送试用额度）：
   > https://passport.compshare.cn/register?referral_code=D7ulbLWjdVHCJ0gNwdFxoa
2. **首次充值**：登录控制台后完成首次充值，**5~10 元即可**。按量计费下 qwen3.8-27b 跑一天只需几毛钱，5~10 元足够试用。
3. **创建 API 密钥**：控制台 →「访问管理 / API 密钥」→ 获取：
   - AccessKey ID（公钥）
   - AccessKey Secret（私钥，只显示一次，务必保存）

用户拿到密钥后，指导其在本机写入（**你在会话中代为执行也可以，但密钥值由用户提供、只写本机配置**）：

```bash
mangosteen cloud key compshare --secret        # 按提示粘贴私钥（不回显）
mangosteen cloud key compshare --public-key <AccessKeyID>
```

写入后 `mangosteen doctor` 应显示「云服务商 [OK] compshare 已配置成功」。

### §3 选点：最便宜且单价 < 1 元/小时

```bash
mangosteen cloud cheapest --min-vram 24 --json
```

- qwen3.8-27b Q4_K_M 需要 **≥ 20GB 显存**（KV cache 后建议 24GB 卡）；`--min-vram 24` 是推荐值（V100S 32G / 3090 24G / 4090 24G 均可）
- 从 JSON 结果里选 **price_per_hour < 1.0 的最小者**（如 P40 ¥0.38、3080TI ¥0.7 之类）
- **节点偏好**（按可用性排序）：3090 / V100S / 4090（较新架构，Qwen3.8 kernel 兼容好）> P40（Pascal 老卡，¥0.38 最便宜但**实测输出乱码**，仅作备选且必须验证输出）
- **无 < 1 元节点时的重试策略（必须执行，不许放弃）**：
  1. 告知用户「当前无满足 < 1 元/小时的节点，每 10 分钟自动重试选点」
  2. 循环：`sleep 600` 后重新执行 `mangosteen cloud cheapest --min-vram 24 --json`，直到出现 < 1 元/小时的节点
  3. 每次重试向用户报一次当前最便宜价格与库存情况
  4. 用户主动喊停才退出循环
- 选中节点后记下 GPU 型号（如 `V100S`），供部署命令的 `--gpu-model` 使用

### §4 部署

```bash
# 标准版
mangosteen deploy qwen3.8-27b --template default --gpu-model <型号> --watch

# 去审查版
mangosteen deploy qwen3.8-27b --template uncensored --gpu-model <型号> --watch
```

说明：
- `--watch` 每 5 秒刷新进度直至完成；模型下载 15.7GB，hf-mirror 源通常 5-15 分钟
- **幂等**：同模型 ready+active 时 deploy 会自动跳过；旧的非运行台账会自动清理重建
- 失败速查见 §8

### §5 验证推理

```bash
mangosteen status                  # 状态 ready、运行状态 active
# 从 status 或部署输出拿本地端口（如 11435），验证推理：
curl http://127.0.0.1:<本地端口>/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen3.8-27b","messages":[{"role":"user","content":"你好"}],"max_tokens":32}'
```

返回 `choices[0].message.content` 非空即成功。也可以直接打开 `http://127.0.0.1:11516` 的 Web 控制台对话。

### §6 模型切换

- 标准版 ↔ 去审查版：先 `stop` 旧部署再 deploy 新模板（同一实例可复用，模型文件不同需重新下载）
- `stop` 后引擎端口（8080/8081）全部空闲时实例自动关机停止计费；任一端口被占则实例保留（有其他模型在跑）

### §7 清理（任务结束时必须执行）

**测试完毕必须删除模型部署并关闭所有实例：**

```bash
# 1. 删除模型部署（引擎端口空闲时自动关机；仍有共享部署时实例保留）
mangosteen stop <部署ID>            # 或 mangosteen stop --all --yes
# 2. 确认所有实例已关闭（防止任何遗漏节点持续计费）
mangosteen cloud instances list
mangosteen cloud instances stop compshare/<实例ID>    # 对任何 running 的实例执行
```

向用户报告清理结果（哪些实例已关机）。若用户希望保留环境，需用户明确说明「不清理」，并在回复中提醒实例在计费。

### §8 故障速查

| 现象 | 处置 |
|------|------|
| 「接口调用过于频繁 [226619]」 | 厂商限流，等 5-10 分钟重试（选点循环天然覆盖） |
| 「无库存 [226604]」 | 换型号（`--gpu-model 3090` 等）或等下一轮选点重试 |
| 「实例保留（未关机）」 | 引擎端口仍被占：先停掉占用端口的部署，再 stop；或 `cloud instances stop` 整机关停 |
| 下载卡在 15.66GiB 不动 | 部署 CLI 会自动断点续传重试；长时间无进展可重新执行 deploy（断点保留） |
| SSH 连接中断 | 连接层瞬断自动重试 2 次；实例已关机属正常（先 `cloud instances start`） |
| ready 但 unactive | 隧道未激活：执行 `mangosteen ui restart` 或重新 deploy（幂等跳过已 active 的） |
| **P40 输出乱码/空内容** | P40（Pascal 架构）对 Qwen3.8 新 kernel 兼容有问题（2026-11-13 实测：推理 200 但输出乱码）。选点时**优先 3090/V100S/4090 等较新卡**；P40 仅作最后备选，部署后必须验证输出可读 |
| 推理 content 为空但 reasoning_content 有内容 | Qwen3.8 是推理模型：思维链走 `reasoning_content`，`max_tokens` 太小会只输出思维链。验证时 max_tokens ≥ 64 |

## 输出要求

- 每完成一个阶段（选点 / 部署 / 验证 / 清理）向用户简报一次结果
- 涉及费用时给出实算：实例时价 × 预计时长；清理后确认「计费已停止」
- 全程使用中文与用户交流