# Mangosteen 使用指南

Mangosteen（山竹）是一款云端租赁部署大模型的客户端工具。本指南以 **qwen2.5-0.5b** 模型为例，详细介绍所有命令的使用方法。

---

## 目录

1. [快速开始](#1-快速开始)
2. [核心概念](#2-核心概念)
3. [部署模型](#3-部署模型)
4. [管理部署](#4-管理部署)
5. [查看状态](#5-查看状态)
6. [云端实例管理](#6-云端实例管理)
7. [云厂商密钥管理](#7-云厂商密钥管理)
8. [GPU 节点选择](#8-gpu-节点选择)
9. [自定义节点管理](#9-自定义节点管理)
10. [模型配置模板管理](#10-模型配置模板管理)
11. [配置管理](#11-配置管理)
12. [Web UI 控制台](#12-web-ui-控制台)
13. [环境自检](#13-环境自检)
14. [API 接口](#14-api-接口)
15. [常见问题](#15-常见问题)

---

## 1. 快速开始

### 1.1 安装与验证

```bash
# 查看版本
./mangosteen version

# 查看帮助
./mangosteen help
```

**执行结果：**

`mangosteen version` 输出：
```
Mangosteen 0.1.0 (macos / x86_64)
```

`mangosteen help` 输出：
```
Mangosteen—— 云端租赁部署大模型部署客户端 v0.1.0

用法
  mangosteen <命令> [选项]

命令
  deploy  部署模型到云端
  stop    停止部署并释放资源
  status  查看网关与部署状态
  cloud   云端算力管理
  device  自定义节点管理
  config  配置管理
  ui      启动 Web UI
  doctor  环境自检
  version 显示版本
  help    显示帮助

全局选项
  --config <path>     指定配置文件
  --json              以 JSON 输出（便于脚本处理）
  --quiet             抑制 banner 与进度提示
  --help, -h          显示帮助
  --version, -V       显示版本
```

### 1.2 三步部署模型

```bash
# 步骤 1: 查看可用 GPU 节点
./mangosteen cloud cheapest --min-vram 1

# 步骤 2: 部署模型（以 qwen2.5-0.5b 为例）
./mangosteen deploy qwen2.5-0.5b --gpu-model V100S --watch

# 步骤 3: 查看部署状态
./mangosteen status
```

**步骤 1 执行结果：**
```
最便宜候选（按价格升序）：

  服务商        GPU型号  数量  显存        价格        地域        
  ---------  -----  --  --------  --------  ---------- 
  compshare  V100S  1x  32.0 GiB  ¥1.340/h  cn-wlcb-01
```

---

## 2. 核心概念

### 2.1 部署状态

| 状态 | 说明 |
|------|------|
| `planned` | 部署计划已创建，等待执行 |
| `provisioning` | 正在创建云端实例、下载模型、启动服务 |
| `ready` | 部署成功，模型正在服务 |
| `stopped` | 已停止服务，实例保留（可复用） |
| `failed` | 部署失败 |
| `destroyed` | 实例已销毁，资源已释放 |

### 2.2 运行状态

| 状态 | 说明 |
|------|------|
| `active` | 服务正在运行，可接收请求 |
| `unactive` | 服务未运行 |

### 2.3 端口说明

| 端口 | 说明 |
|------|------|
| 本地端口（如 11435） | 本地访问端口，通过 SSH 隧道映射到远端推理服务 |
| 远端端口（如 8080） | 云端实例上 llama-server 的监听端口 |
| 网关端口（11516） | Mangosteen API 网关端口，提供 OpenAI 兼容接口 |

---

## 3. 部署模型

### 3.1 deploy 命令

将模型部署到云端 GPU 实例。

```bash
mangosteen deploy <模型> [选项]
```

#### 参数说明

| 参数 | 说明 | 示例 |
|------|------|------|
| `<模型>` | 模型 ID 或 GGUF 文件路径 | `qwen2.5-0.5b` |
| `--gpu-model <型号>` | 指定 GPU 型号 | `--gpu-model V100S` |
| `--device <节点ID>` | 指定自定义节点部署 | `--device dev-001` |
| `--template <槽位>` | 配置模板名称 | `--template default` |
| `--context <N>` | 上下文长度（默认 8192） | `--context 16384` |
| `--batch-size <N>` | 批处理大小（默认 512） | `--batch-size 256` |
| `--kv-cache <类型>` | KV 缓存精度 | `--kv-cache q8_0` |
| `--max-price <元/小时>` | 最高价格上限 | `--max-price 1.5` |
| `--watch` | 部署后轮询查看进度 | `--watch` |
| `--async` | 异步返回，不等待完成 | `--async` |
| `--local-only` | 仅使用本地算力 | `--local-only` |
| `--no-cpu-fallback` | 禁止 CPU 回退 | `--no-cpu-fallback` |
| `--require-full-gpu` | 要求完整 GPU 显存 | `--require-full-gpu` |
| `--json` | 以 JSON 格式输出 | `--json` |
| `--quiet` | 抑制进度提示 | `--quiet` |
| `--interactive` | 交互式确认部署方案 | `--interactive` |

#### 示例 1：基础部署（推荐新手）

```bash
./mangosteen deploy qwen2.5-0.5b --gpu-model V100S --watch
```

**执行结果（部署成功）：**
```
  ●  Mangosteen 0.1.0
  本地算力优先 · 云端算力弹性补位

· 配置模板：default（来源：设置页配置；可用 --template <槽位> 切换）
  ⠋ 正在部署 qwen2.5-0.5b 评估资源并调度…

[perf] ══ 部署开始 (dep-xxx-qwen2.5-0.5b-instruct) ══
[perf] ▶ 阶段 实例创建(等待Running+SSH确认)
[cloud] ✓ provision_resources 复用 GPU 型号匹配实例 compshare · uhost-xxx（状态 stopped）
[cloud] ✓ 实例 uhost-xxx 已进入 Running 状态
[cloud] ✓ 实例 uhost-xxx SSH 隧道已建立，本地端口 11436

[perf] ✓ 阶段 实例创建(等待Running+SSH确认) 耗时 99.3s
[perf] ▶ 阶段 节点配置(依赖安装+llama-server下载)
[perf] ✓ 阶段 节点配置(依赖安装+llama-server下载) 耗时 4.8s
[perf] ▶ 阶段 模型下载
[perf] ✓ 阶段 模型下载 耗时 9.3s

[perf] ═══ 部署耗时汇总 (dep-xxx) 成功 ═══
[perf]   实例创建(等待Running+SSH确认)     99.3s
[perf]   节点配置(依赖安装+llama-server下载)      4.8s
[perf]   模型下载            9.3s
[perf]   总计            113.4s
[perf] ════════════════════════════

✓ 部署完成：qwen2.5-0.5b-instruct (云端算力 · compshare)
  部署 ID      dep-xxx-qwen2.5-0.5b-instruct
  算力来源       云端算力 · compshare
  落位形态       纯 CPU 推理（无加速卡参与）
  上下文        8192
  KV 精度      f16
  量化         Q4_K_M
  后端端口       11435
  OpenAI 入口  http://127.0.0.1:11516/v1
  资源占用       权重 0.46 GiB + KV 0.09 GiB + 计算缓冲 0.19 GiB + 开销 0.06 GiB = 0.80 GiB
```

**执行结果（部署失败 - 厂商 API 频率限制 [226619]）：**

当短时间内频繁调用厂商 API 时，可能触发限流：

```
✗ 服务商 compshare 返回错误 [226619]：接口调用过于频繁，请稍后重试。
  建议： 服务商接口报错。核对密钥与账户余额，或换其它服务商
```

**解决建议：** 等待 5-10 分钟后重试，或使用 `--max-price` 放宽价格限制，尝试其他 GPU 型号。

#### 示例 2：指定 GPU 型号和价格上限

```bash
./mangosteen deploy qwen2.5-0.5b --gpu-model 3080TI --max-price 1.0 --watch
```

#### 示例 3：使用自定义节点

```bash
./mangosteen deploy qwen2.5-0.5b --device dev-001 --template default
```

#### 示例 4：调整上下文长度

```bash
./mangosteen deploy qwen2.5-0.5b --context 16384 --batch-size 256
```

#### 示例 5：异步部署（不等待完成）

```bash
./mangosteen deploy qwen2.5-0.5b --gpu-model V100S --async
```

**执行结果：**
```
部署 ID: dep-xxx-qwen2.5-0.5b-instruct
异步部署已启动，使用 `mangosteen status` 查看进度
```

---

## 4. 管理部署

### 4.1 stop 命令

停止部署并释放云端资源。

```bash
mangosteen stop <部署ID> | --all [选项]
```

#### 参数说明

| 参数 | 说明 |
|------|------|
| `<部署ID>` | 要停止的部署 ID（可通过 `mangosteen status` 查看） |
| `--all` | 停止全部活跃部署 |
| `--yes` | 免确认直接执行 |
| `--json` | 以 JSON 格式输出 |
| `--quiet` | 抑制进度提示 |

#### 示例 1：停止指定部署

```bash
./mangosteen stop dep-xxx-qwen2.5-0.5b-instruct --yes
```

**执行结果：**

如果部署状态为 `failed`（无法停止）：
```
✗ 请求非法：部署 `dep-xxx-qwen2.5-0.5b-instruct` 当前状态为 failed，不能 stop。只有 ready 状态的部署可停止
  建议： 执行 `mangosteen help <命令>` 查看正确用法
```

如果部署状态为 `ready`（成功停止）：
```
✓ 部署 dep-xxx-qwen2.5-0.5b-instruct 已停止，资源已释放
```

> **注意**：只有 `ready` 状态的部署才能执行 stop 命令。

#### 示例 2：停止全部部署

```bash
./mangosteen stop --all --yes
```

**执行结果：**
```
✓ 全部部署已停止
```

> **注意**：停止部署会停止 GPU 计费，但保留实例与磁盘，下次部署可复用。

---

## 5. 查看状态

### 5.1 status 命令

查看网关与部署状态。

```bash
mangosteen status [选项]
```

#### 参数说明

| 参数 | 说明 |
|------|------|
| `--json` | 以 JSON 格式输出 |
| `--quiet` | 抑制 banner |
| `--config <path>` | 指定配置文件 |

#### 示例 1：查看状态（表格形式）

```bash
./mangosteen status
```

**执行结果（无部署记录）：**
```
系统状态

  版本    0.1.0
  硬件    macos / x86_64 · 4 物理核 · 16.0 GiB 内存 · 无加速卡（CPU 推理）
  云服务商  compshare
  配置文件  用户配置：/Users/xxx/Library/Application Support/mangosteen/config.toml

全部部署

  当前没有部署记录
  执行 `mangosteen deploy <模型>` 开始部署

  推理入口：http://127.0.0.1:11516/v1
```

**执行结果（有活跃部署）：**
```
系统状态

  版本    0.1.0
  硬件    macos / x86_64 · 4 物理核 · 16.0 GiB 内存 · 无加速卡（CPU 推理）
  云服务商  compshare
  配置文件  用户配置：/Users/xxx/Library/Application Support/mangosteen/config.toml

全部部署

活跃部署（正在运行）：

  部署 ID                                     模型                            算力来源              状态     运行状态      本地端口   远端端口  健康
  ----------------------------------------  ----------------------------  ----------------  -----  --------  -----  ----  ---
  dep-xxx-qwen2.5-0.5b-instruct             qwen2.5-0.5b-instruct:q4_k_m   云端算力 · compshare  ready  active      11435  8080  就绪

  推理入口：http://127.0.0.1:11516/v1
```

#### 示例 2：JSON 格式输出

```bash
./mangosteen status --json
```

**执行结果：**
```json
{
  "version": "0.1.0",
  "hardware": "macos / x86_64 · 4 物理核 · 16.0 GiB 内存",
  "providers": ["compshare"],
  "active": [
    {
      "id": "dep-xxx-qwen2.5-0.5b-instruct",
      "model": "qwen2.5-0.5b-instruct:q4_k_m",
      "origin": "云端算力 · compshare",
      "status": "ready",
      "run_status": "active",
      "port": 11435,
      "remote_port": 8080,
      "ready": true
    }
  ]
}
```

---

## 6. 云端实例管理

### 6.1 cloud instances 命令

管理云厂商托管的实例。

```bash
mangosteen cloud instances <子命令> [选项]
```

#### 子命令

| 子命令 | 说明 |
|--------|------|
| `list` | 列出云端实例（含已关机） |
| `start` | 启动实例（开机） |
| `stop` | 关闭实例（停止 GPU 计费，保留磁盘） |
| `destroy` | 删除实例（销毁实例与磁盘，不可恢复） |

#### 示例 1：列出所有实例

```bash
./mangosteen cloud instances list
```

**执行结果：**
```
  实例ID                          GPU型号     GPU数量  显存     状态       价格        已部署模型
  ----------------------------  --------  -----  -----  -------  --------  -----
  compshare/uhost-1xlo8hdbjy34  RTX 3090  1      24 GB  stopped  ¥0.00/小时
  compshare/uhost-1xkzmcrg870e  V100S     1      32 GB  stopped  ¥0.00/小时
  compshare/uhost-1xg39npzxod7  P40       1      24 GB  stopped  ¥0.00/小时

  用 `mangosteen cloud instances start <厂商>/<实例ID>` 启动实例
  用 `mangosteen cloud instances stop <厂商>/<实例ID>` 关闭实例
  用 `mangosteen cloud instances destroy <厂商>/<实例ID>` 删除实例
```

#### 示例 2：启动实例

```bash
./mangosteen cloud instances start compshare/uhost-1xkzmcrg870e
```

**执行结果：**
```
✓ 实例 uhost-1xkzmcrg870e 已启动
```

#### 示例 3：关闭实例

```bash
./mangosteen cloud instances stop compshare/uhost-1xkzmcrg870e
```

**执行结果：**
```
✓ 实例 uhost-1xkzmcrg870e 已关机
```

#### 示例 4：删除实例（需确认）

```bash
./mangosteen cloud instances destroy compshare/uhost-1xkzmcrg870e --yes
```

**执行结果：**
```
✓ 实例 uhost-1xkzmcrg870e 已删除
```

---

## 7. 云厂商密钥管理

### 7.1 cloud key 命令

管理云厂商的 API 密钥。

> **厂商选择说明**：`compshare`（优云智算）是当前唯一已验证支持的云服务商（专用 RPC 驱动）。
> `ucloud`/`aliyun`/`autodl` 为占位服务商，无内置 profile，配置密钥后驱动加载仍会被跳过，
> 请勿向它们写入密钥。

```bash
mangosteen cloud key <厂商> [选项]
```

#### 参数说明

| 参数 | 说明 |
|------|------|
| `<厂商>` | 云服务商名称（**使用 compshare**；aliyun/autodl/ucloud 为占位） |
| `--secret <密钥>` | 更新厂商私钥（Secret Key） |
| `--public-key <公钥>` | 更新厂商公钥（AccessKey ID） |
| `--enable` | 启用该厂商 |
| `--disable` | 禁用该厂商 |

#### 示例 1：更新密钥

```bash
./mangosteen cloud key compshare --public-key YOUR_ACCESS_KEY --secret YOUR_SECRET_KEY
```

**执行结果：**
```
✓ 已更新 compshare 密钥
```

#### 示例 2：启用/禁用厂商

```bash
./mangosteen cloud key compshare --enable
./mangosteen cloud key aliyun --disable
```

**执行结果：**
```
✓ 已启用 compshare
✓ 已禁用 aliyun
```

### 7.2 cloud providers 命令

列出所有云服务商及其状态。

```bash
mangosteen cloud providers [选项]
```

#### 示例

```bash
./mangosteen cloud providers
```

**执行结果：**
```
  名称         显示名     驱动         状态  权重
  ---------  ------  ---------  --  ---
  aliyun     阿里云     aliyun     禁用  1.0
  autodl     AutoDL  autodl     禁用  1.0
  compshare  优云智算    compshare  启用  1.0

  用 `mangosteen cloud key <厂商> --enable|--disable` 启用/禁用服务商
  用 `mangosteen cloud key <厂商> --secret <密钥> --public-key <公钥>` 更新密钥
```

---

## 8. GPU 节点选择

### 8.1 cloud cheapest 命令

按价格从低到高列出可用的 GPU 节点。

```bash
mangosteen cloud cheapest [选项]
```

#### 参数说明

| 参数 | 说明 |
|------|------|
| `--min-vram <GB>` | 最小显存要求（GB） |
| `--require-stock` | 仅显示有库存的节点 |
| `--no-require-stock` | 显示所有节点（包括无库存） |
| `--json` | 以 JSON 格式输出 |

#### 示例 1：查找最便宜的 GPU

```bash
./mangosteen cloud cheapest --min-vram 1
```

**执行结果：**
```
最便宜候选（按价格升序）：

  服务商        GPU型号     数量  显存        价格        地域
  ---------  --------  --  --------  --------  ----------
  compshare  RTX 4090  1x  24.0 GiB  ¥2.050/h  cn-wlcb-01
  compshare  H20       1x  96.0 GiB  ¥7.120/h  cn-wlcb-01
```

#### 示例 2：JSON 格式输出

```bash
./mangosteen cloud cheapest --min-vram 8 --json
```

**执行结果：**
```json
{
  "candidates": [
    {
      "provider": "compshare",
      "gpu_model": "RTX 4090",
      "gpu_count": 1,
      "vram_bytes": 25769803776,
      "memory_bytes": 68719476736,
      "price_per_hour": 2.05,
      "region": "cn-wlcb-01",
      "spec_id": "4090/1x16c/64g"
    }
  ]
}
```

### 8.2 cloud pick 命令

智能选点（综合考虑价格、延迟、可用性、硬件）。

```bash
mangosteen cloud pick [选项]
```

#### 参数说明

| 参数 | 说明 |
|------|------|
| `--min-vram <GB>` | 最小显存要求（GB） |
| `--max-price <元/小时>` | 最高价格上限 |
| `--explain` | 显示打分明细 |
| `--json` | 以 JSON 格式输出 |

#### 示例

```bash
./mangosteen cloud pick --max-price 1.5 --explain
```

**执行结果：**
```
智能选点结果（综合评分）：

  服务商        GPU型号     价格        评分    说明
  ---------  --------  --------  ----  ----
  compshare  V100S     ¥1.340/h  85   价格适中，库存充足
  compshare  RTX 3090  ¥1.130/h  78   价格较低，显存充足
```

---

## 9. 自定义节点管理

### 9.1 device 命令

管理用户自己的 SSH 服务器节点。

```bash
mangosteen device <子命令> [选项]
```

#### 子命令

| 子命令 | 说明 |
|--------|------|
| `list` | 列出所有自定义节点 |
| `add` | 新增一个自定义节点 |
| `remove` | 删除一个自定义节点 |
| `test` | 测试节点 SSH 连通性 |

#### 示例 1：列出节点

```bash
./mangosteen device list
```

**执行结果（无节点）：**
```
（当前没有自定义节点）

  执行 `mangosteen device add <主机> <端口> --user <用户>` 新增一个节点
```

**执行结果（有节点）：**
```
  节点 ID              名称        主机            端口  用户    状态
  -----------------  --------  ------------  ---  ----  -----
  dev-192.168.1.100  我的节点    192.168.1.100  22   root  启用
```

#### 示例 2：添加节点

```bash
./mangosteen device add 192.168.1.100:22 --user root --password YOUR_PASSWORD --name 我的节点
```

**执行结果：**
```
✓ 已添加自定义节点 dev-192.168.1.100
```

#### 示例 3：测试节点连通性

```bash
./mangosteen device test dev-192.168.1.100
```

**执行结果：**
```
✓ SSH 连接成功
✓ 远端环境检查通过
```

#### 示例 4：删除节点

```bash
./mangosteen device remove dev-192.168.1.100
```

**执行结果：**
```
✓ 已删除节点 dev-192.168.1.100
```

---

## 10. 模型配置模板管理

### 10.1 models 命令

管理模型配置模板（设置页「模型配置」的 CLI 对应）。
模板是部署的唯一真源：下哪个仓库的哪个文件、用什么参数启动，全部由它决定。

```bash
mangosteen models <子命令> [选项]
```

#### 子命令

| 子命令 | 说明 |
|--------|------|
| `list` | 列出全部模型的配置模板 |
| `add` | 新增或更新一个配置模板 |
| `remove` | 删除一个自定义配置模板 |
| `show` | 显示指定模板的正文和解析摘要 |

#### 示例 1：列出全部模型配置模板

```bash
./mangosteen models list
```

**执行结果：**
```
模型配置模板

  qwen2.5-0.5b (default)
    default [内置] default

  qwen3.8-27b (default)
    default [内置] default
    uncensored [内置] Uncensored

用 `mangosteen models show <模型> --slot <槽位>` 查看模板详情
用 `mangosteen models add <模型> --slot <槽位> --file <文件>` 新增/更新模板
用 `mangosteen models remove <模型> --slot <槽位>` 删除自定义模板
```

#### 示例 2：查看模板详情

```bash
./mangosteen models show qwen3.8-27b --slot uncensored
```

**执行结果：**
```
模型：qwen3.8-27b (default)
槽位：uncensored [内置]

模板正文
template: Uncensored
model_id: qwen3.8-27b
quantization: 4-bit (Q4_K_M)
device: ssh
source: https://hf-mirror.com/
repository: JonathanColetti/Qwen3.8-27B-Uncensored-GGUF
framework: llama.cpp
binary: llama-server
model: Qwen3.8-27B-Uncensored-Q4_K_M.gguf
context: 65536
gpu_layers: 99
...

解析摘要
  模型文件      Qwen3.8-27B-Uncensored-Q4_K_M.gguf
  下载地址      https://hf-mirror.com/JonathanColetti/Qwen3.8-27B-Uncensored-GGUF/resolve/main/Qwen3.8-27B-Uncensored-Q4_K_M.gguf
  上下文        65536
  GPU 层数      99
```

#### 示例 3：新增自定义模板

```bash
./mangosteen models add qwen3.8-27b --slot custom --file ./my-template.yaml
```

**执行结果：**
```
✓ 模板已保存：qwen3.8-27b / custom

解析摘要
  模型文件      my-model-q4_k_m.gguf
  下载地址      https://hf-mirror.com/my-repo/my-model/resolve/main/my-model-q4_k_m.gguf
  上下文        8192
  GPU 层数      99
```

#### 示例 4：删除自定义模板

```bash
./mangosteen models remove qwen3.8-27b --slot custom
```

**执行结果：**
```
✓ 模板已删除：qwen3.8-27b / custom
```

> **注意**：内置模板（default、uncensored）不能删除，只能编辑内容（用 `--force` 强制覆盖）。

#### 内置模板说明

当前系统内置以下 3 个配置模板：

| 模型 | 槽位 | 说明 |
|------|------|------|
| `qwen2.5-0.5b` | `default` | Qwen2.5-0.5B 默认配置（Q4_K_M 量化） |
| `qwen3.8-27b` | `default` | Qwen3.8-27B 默认配置（Q4_K_M 量化） |
| `qwen3.8-27b` | `uncensored` | Qwen3.8-27B Uncensored 版本（Q4_K_M 量化，无审查） |

#### 使用场景

```bash
# 部署 qwen2.5-0.5b（default 模板）
./mangosteen deploy qwen2.5-0.5b --template default --gpu-model 3080TI --watch

# 部署 qwen3.8-27b（default 模板）
./mangosteen deploy qwen3.8-27b --template default --gpu-model 3080TI --watch

# 部署 qwen3.8-27b（uncensored 模板）
./mangosteen deploy qwen3.8-27b --template uncensored --gpu-model 3080TI --watch
```

---

## 11. 配置管理

### 11.1 config 命令

管理 Mangosteen 配置文件。

```bash
mangosteen config <子命令> [选项]
```

#### 子命令

| 子命令 | 说明 |
|--------|------|
| `show` | 显示当前完整配置 |
| `init` | 初始化默认配置文件 |
| `path` | 显示配置文件路径 |
| `validate` | 验证配置项合法性 |
| `reset-templates` | 重置内置部署模板 |

#### 示例 1：显示配置

```bash
./mangosteen config show
```

**执行结果：**
```
配置
  用户配置：/Users/zhuzhichao/Library/Application Support/mangosteen/config.toml

网关
  监听  127.0.0.1:11516
  鉴权  关闭

云端选择策略
  本地优先    是
  权重      价格 40 / 延迟 15 / 可用性 30 / 硬件 15
  降级重试上限  2
  价格上限    不限

模型配置
  推理引擎   自动在 PATH 中查找
  默认上下文  8192
  端口区间   11435-11999

云服务商
  名称         驱动         状态  凭证                                                                                          优先级
  ---------  ---------  --  ------------------------------------------------------------------------------------------  ---
  aliyun     aliyun     停用  ${ALIBABA_CLOUD_ACCESS_KEY_SECRET}                                                          1.0
  autodl     autodl     停用  ${AUTODL_API_TOKEN}                                                                         1.0
  compshare  compshare  启用  file:/Users/zhuzhichao/Library/Application Support/mangosteen/secrets/compshare_secret_key  1.0
```

#### 示例 2：查看配置路径

```bash
./mangosteen config path
```

**执行结果：**
```
配置与数据路径
  配置文件（写入位置）  /Users/zhuzhichao/Library/Application Support/mangosteen/config.toml
  配置文件（候选列表）  /Users/zhuzhichao/Library/Application Support/mangosteen/config.toml
                      /etc/mangosteen/config.toml
  日志          /Users/zhuzhichao/Library/Logs/mangosteen
  部署台账        /Users/zhuzhichao/Library/Application Support/mangosteen/state.json
  模型台账        /Users/zhuzhichao/Library/Application Support/mangosteen/models.json
```

#### 示例 3：验证配置

```bash
./mangosteen config validate
```

**执行结果：**
```
✓ 配置合法（用户配置：/Users/zhuzhichao/Library/Application Support/mangosteen/config.toml）
```

#### 示例 4：初始化配置

```bash
./mangosteen config init
```

**执行结果：**
```
✓ 已创建默认配置文件
```

---

## 12. Web UI 控制台

### 12.1 ui 命令

启动 Web UI 控制台，提供可视化管理界面。

```bash
mangosteen ui [start|stop|restart|status] [选项]
```

#### 子命令

| 子命令 | 说明 |
|--------|------|
| `start` | 后台启动 UI/API 网关 |
| `stop` | 停止 UI/API 网关 |
| `restart` | 重启 UI/API 网关 |
| `status` | 查看 UI/API 网关运行状态 |

#### 参数说明

| 参数 | 说明 |
|------|------|
| `--port <端口>` | 指定网关监听端口（默认 11516） |
| `--host <地址>` | 指定网关监听地址 |
| `--no-open` | 不自动打开浏览器 |
| `--foreground` | 前台运行（不守护进程） |

#### 示例 1：启动 Web UI

```bash
./mangosteen ui start
```

**执行结果：**
```
✓ UI/API 网关已启动
  访问地址：http://127.0.0.1:11516
```

#### 示例 2：停止 Web UI

```bash
./mangosteen ui stop
```

**执行结果：**
```
✓ UI/API 网关已关闭（不影响已部署的推理服务）
```

#### 示例 3：查看状态

```bash
./mangosteen ui status
```

**执行结果：**
```
✓ UI/API 网关运行中（PID 82854，端口 11516）
```

---

## 13. 环境自检

### 13.1 doctor 命令

检查系统环境是否满足运行要求。

```bash
mangosteen doctor [选项]
```

#### 检查项

- 硬件采集（CPU/内存/加速卡）
- 推理引擎（llama-server 是否就位）
- 配置文件（是否合法、密钥是否齐全）
- 云服务商驱动
- 目录权限
- 端口占用
- 磁盘空间

#### 示例

```bash
./mangosteen doctor
```

**执行结果：**
```
环境自检

  [OK  ] 硬件采集
         macos / x86_64 · 4 物理核 · 16.0 GiB 内存 · 无加速卡（CPU 推理）
  [WARN] 加速卡
         未检测到可用加速卡，只能 CPU 推理
         → CPU 推理速度通常 <5 tok/s。如需 GPU 加速，检查驱动/显存；或配置云服务商以降级
  [OK  ] 配置合法性
         用户配置：/Users/zhuzhichao/Library/Application Support/mangosteen/config.toml
  [WARN] 网关端口
         11516 已被占用
         → 已有 serve 在运行？否则改配置 gateway.port 或加 --port 启动
  [OK  ] 云服务商
         已就绪：compshare
         → 另有 2 个被跳过（用 `mangosteen cloud providers` 查看）

⚠ 2 项警告，不影响本地部署
```

---

## 14. API 接口

Mangosteen 网关提供 OpenAI 兼容的 API 接口，支持主流 AI 应用直接接入。

### 14.1 启动网关

```bash
./mangosteen ui start --no-open
```

### 14.2 API 端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `/v1/models` | GET | 获取模型列表 |
| `/v1/chat/completions` | POST | 对话补全 |
| `/v1/completions` | POST | 文本补全 |
| `/v1/embeddings` | POST | 向量化 |
| `/v1/messages` | POST | Anthropic 兼容接口 |
| `/health` | GET | 健康检查 |

### 14.3 使用示例

#### 获取模型列表

```bash
curl http://127.0.0.1:11516/v1/models
```

**执行结果：**
```json
{
    "object": "list",
    "data": [
        {
            "id": "qwen2.5-0.5b-instruct:q4_k_m",
            "object": "model",
            "created": 1794463744,
            "owned_by": "local"
        }
    ]
}
```

#### 健康检查

```bash
curl http://127.0.0.1:11516/health
```

**执行结果：**
```json
{
    "status": "ok",
    "version": "0.1.0",
    "backend_ready": false
}
```

> **注意**：`backend_ready: false` 表示当前没有活跃的推理后端。需要先部署模型并等待就绪后，`backend_ready` 才会变为 `true`。

#### 对话补全（OpenAI 格式）

```bash
curl -X POST http://127.0.0.1:11516/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-0.5b-instruct:q4_k_m",
    "messages": [{"role": "user", "content": "你好，请介绍一下自己"}],
    "max_tokens": 64
  }'
```

**执行结果：**
```json
{
  "id": "chatcmpl-xxx",
  "object": "chat.completion",
  "created": 1700000000,
  "model": "qwen2.5-0.5b-instruct:q4_k_m",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "你好！我是 Qwen，一个大型语言模型。"
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 20,
    "completion_tokens": 15,
    "total_tokens": 35
  }
}
```

#### 对话补全（Anthropic 格式）

```bash
curl -X POST http://127.0.0.1:11516/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-0.5b-instruct:q4_k_m",
    "max_tokens": 64,
    "messages": [{"role": "user", "content": "你好，请介绍一下自己"}]
  }'
```

**执行结果：**
```json
{
  "id": "msg-xxx",
  "type": "message",
  "role": "assistant",
  "content": [
    {
      "type": "text",
      "text": "你好！我是 Qwen，一个大型语言模型。"
    }
  ],
  "model": "qwen2.5-0.5b-instruct:q4_k_m",
  "stop_reason": "end_turn",
  "usage": {
    "input_tokens": 20,
    "output_tokens": 15
  }
}
```

### 14.4 在代码中使用

#### Python（使用 OpenAI SDK）

```python
from openai import OpenAI

client = OpenAI(
    api_key="not-needed",
    base_url="http://127.0.0.1:11516/v1"
)

response = client.chat.completions.create(
    model="qwen2.5-0.5b-instruct:q4_k_m",
    messages=[{"role": "user", "content": "你好"}]
)

print(response.choices[0].message.content)
```

#### Python（使用 Anthropic SDK）

```python
from anthropic import Anthropic

client = Anthropic(
    api_key="not-needed",
    base_url="http://127.0.0.1:11516"
)

response = client.messages.create(
    model="qwen2.5-0.5b-instruct:q4_k_m",
    max_tokens=64,
    messages=[{"role": "user", "content": "你好"}]
)

print(response.content[0].text)
```

---

## 15. 常见问题

### Q1: 部署失败，提示 "Permission denied"

**原因**：实例密码重置后 sshd 可能尚未生效。

**解决**：系统会自动重试，请稍后再试。如果持续失败，请检查云厂商控制台的实例密码。

### Q2: 部署失败，提示 "[226604] 该规格当前无库存"

**原因**：该 GPU 型号当前无库存。

**解决**：更换 GPU 型号或稍后重试。

```bash
# 查看其他可用 GPU
./mangosteen cloud cheapest --min-vram 1
```

### Q3: 部署失败，提示 "[226619] 接口调用过于频繁"

**原因**：触发厂商 API 限流。

**解决**：等待 5-10 分钟后重试。

### Q4: 无法访问推理服务

**原因**：SSH 隧道未建立或网关未启动。

**解决**：

```bash
# 1. 启动网关
./mangosteen ui start

# 2. 检查隧道端口
curl http://127.0.0.1:11435/health

# 3. 检查网关端口
curl http://127.0.0.1:11516/health
```

### Q5: 如何释放 GPU 资源？

**解决**：

```bash
# 停止部署（保留实例，可复用）
./mangosteen stop <部署ID> --yes

# 或停止全部部署
./mangosteen stop --all --yes

# 如需彻底删除实例
./mangosteen cloud instances destroy <厂商>/<实例ID> --yes
```

---

## 附录：配置文件说明

配置文件位置：`~/Library/Application Support/mangosteen/config.toml`

```toml
# 调度策略
[scheduler]
local_first = true           # 本地优先
strategy = "cheapest"        # 调度策略：cheapest / balanced

# 云服务商配置
[providers.compshare]
enabled = true
[providers.compshare.options]
public_key = "YOUR_ACCESS_KEY"
image_id = "compshareImage-xxx"

[providers.aliyun]
enabled = false
[providers.aliyun.options]
public_key = "YOUR_ACCESS_KEY"
```

---

**文档版本**：v1.0  
**更新日期**：2026-11-12  
**维护路径**：`/Users/zhuzhichao/Documents/mangosteen/docs/USAGE.md`