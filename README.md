# Mangosteen（山竹）

> 个人级大模型部署客户端 —— **本地算力优先 + 云端算力弹性补位**

Mangosteen 对标 Ollama 的使用体验，但多了一件事：**本地跑不动的模型，自动帮你上云跑**。
你只需要说"我要跑这个模型"，剩下的（显存够不够、要不要降量化、云端选哪个节点、实例怎么连）
由它自己决定。上层应用始终只面对一个 OpenAI 兼容接口。

```
$ mangosteen deploy qwen2.5-7b --context 8192
✓ 部署完成：qwen2.5-7b-instruct (本地算力)
  落位形态   全部层驻显存（GPU 全量卸载）
  上下文     8192
  资源占用   权重 4.36 GiB + KV 0.44 GiB + 计算缓冲 0.19 GiB + 开销 0.39 GiB = 5.38 GiB
  OpenAI 入口 http://127.0.0.1:11516/v1

$ mangosteen deploy llama3.3-70b --context 8192
✓ 部署完成：llama3.3-70b (云端算力 · AutoDL)
⚠ 发生算力降级：cloud:本地资源不足：需要 42.8 GiB，可用显存 24.0 GiB
  云上实例正在计费，用完请执行 `mangosteen stop <部署ID>`
```

---

## 快速开始

### 1. 安装

从 Release 下载对应平台压缩包解压，把 `mangosteen` 放到 `PATH` 即可。三个平台都支持：

| 平台 | 归档名 |
|------|--------|
| macOS (Intel) | `mangosteen-macos-x86_64.tar.gz` |
| macOS (Apple Silicon) | `mangosteen-macos-aarch64.tar.gz` |
| Linux (x86_64) | `mangosteen-linux-x86_64.tar.gz` |
| Linux (aarch64) | `mangosteen-linux-aarch64.tar.gz` |
| Windows (x86_64) | `mangosteen-windows-x86_64.zip` |

### 2. 推理引擎（按 Ollama 的方式集成 llama.cpp）

Mangosteen 以 **llama.cpp 的 `llama-server`** 为唯一推理引擎。集成方式与 Ollama
一致：**引擎和它的动态库作为一个整体，库搜索路径由 Mangosteen 自己注入**，
不依赖系统包管理器把 `LC_RPATH` / `RUNPATH` 写对。

```bash
# 方式一：复用系统已装的 llama.cpp
brew install llama.cpp                 # macOS / Linux

# 方式二：随包分发的运行时（完全自包含，推荐给不想动系统的用户）
#   把 llama-server 与它的 .dylib/.so 一起放到 mangosteen 同级目录：
#   <mangosteen 所在目录>/runtime/darwin-x86_64/llama-server
#   <mangosteen 所在目录>/runtime/darwin-x86_64/lib*.dylib
```

查找顺序：`local.engine_path` → 可执行文件同级 `runtime/<平台-架构>/` → `PATH`。

> **为什么需要这一步**：Homebrew 装的 `llama-server` 常常依赖
> `@rpath/libllama-server-impl.dylib`，但二进制里**没有写 `LC_RPATH`**。
> 直接执行会报 `Library not loaded ... no LC_RPATH's found`，而 `.dylib`
> 其实就在 `/usr/local/lib` 里。Mangosteen 启动引擎前会自动探测库目录并注入
> `DYLD_FALLBACK_LIBRARY_PATH`（Linux 为 `LD_LIBRARY_PATH`），所以**不需要你手工设环境变量**。
>
> 想确认它到底注入了什么：
>
> ```bash
> $ mangosteen engine env
>   二进制                       /usr/local/bin/llama-server
>   DYLD_FALLBACK_LIBRARY_PATH  /usr/local/bin:/usr/local/lib:/usr/lib
> ```
>
> `mangosteen doctor` 也会单独列出一项「引擎运行库路径」，把这件事说清楚。

### 3. 初始化并自检

```bash
mangosteen config init       # 生成配置模板
mangosteen doctor            # 环境自检（会告诉你还缺什么）
```

### 4. 获取模型

内置 ModelScope（国内快）与 HuggingFace 两个源，开箱即用：

```bash
# 看某个仓库里有哪些 GGUF 文件（含官方大小与 SHA-256）
mangosteen models search Qwen/Qwen2.5-0.5B-Instruct-GGUF

# 下载（省略源名时用 storage.default_source，默认 modelscope）
mangosteen models pull Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf

# 指定源：加前缀或 --source
mangosteen models pull hf:Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf
```

下载走的是 Ollama 式的 blob 流程，四个细节都替你处理了：

| 机制 | 效果 |
|------|------|
| 先落 `.part` 临时文件 | 中断不会留下「看起来能用但实际残缺」的 GGUF |
| 断点续传 | 网络抖动后重跑，从断点继续，不从头开始 |
| 边下边算 SHA-256 | 与源提供的官方哈希比对，不必下完再读一遍 4.5 GiB |
| 校验通过才改名 | 同一文件系统内 `rename` 是原子的，不会出现半个模型 |

下载完成后**自动登记台账**，不需要再 `models add`。重复执行同一条 `pull`
会直接命中已完成的文件（毫秒级返回），不会重复下载。

已有本地文件时仍可手工登记：

```bash
mangosteen models add ./qwen2.5-7b-instruct-q4_k_m.gguf
```

Mangosteen 会解析 GGUF 文件头，读出真实的层数、KV 头数、原生上下文——
这些是资源评估的依据，不需要你手动填。

模型源列表与自定义源：

```bash
mangosteen models sources        # 看当前有哪些源、哪个是默认
```

要接内网私有仓库，只需在配置里加一段，不用改代码：

```toml
[model_sources.myhub]
name = "myhub"
kind = "Custom"
base_url = "https://models.corp.internal"
default_revision = "main"
url_template = "{base}/repo/{repo}/raw/{revision}/{file}"
```

### 5. 部署

```bash
mangosteen deploy qwen2.5-7b-instruct --context 8192
```

然后就可以像用 OpenAI 一样用它：

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:11516/v1", api_key="not-needed")
resp = client.chat.completions.create(
    model="auto",                       # "auto" = 用当前部署的模型
    messages=[{"role": "user", "content": "你好"}],
)
print(resp.choices[0].message.content)
```

---

### 6. 图形界面

```bash
mangosteen ui          # 启动控制台并自动打开浏览器
mangosteen ui --no-open  # 只起服务（SSH / 无桌面环境）
```

界面与 `serve` 跑在**同一个网关端口**上，因此前端调 `/api/*` 与 `/v1/*` 天然同源，
不需要 CORS，也没有额外的静态服务进程。四个页签各自独立：

| 页签 | 用途 |
|------|------|
| **对话** | 流式对话；回形针可附加文档（txt/md/csv/代码原生支持，docx/xlsx/pdf 走系统工具）；地球图标开启后消息里的 URL 会被抓取正文 |
| **部署** | 搜索模型仓库 → 下载 → 部署 → 停止/删除；云端部署会先弹窗列出**算力规格、资源需求、预估时价与总花费**，勾选确认后才创建实例 |
| **设备** | CPU / 内存实时曲线、加速卡、磁盘剩余、每个推理进程的 RSS |
| **设置** | 调度策略、本地推理参数、存储与云服务商开关（凭证只显示「是否已配置」，不回显原文） |

> 前端是**内嵌在二进制里的单文件 HTML**，不引 CDN、不引框架、没有构建步骤，
> 所以离线与内网环境也能正常使用，发布件仍约 1.1 MB。


## 核心概念

### 本地优先，不可动摇

调度决策器严格按这个顺序判断：

```
本地资源满足 → 本地部署        ← 永远优先
本地资源不足 → 云端降级        ← 只在必要时
本地部署失败 → 云端降级 + 记录原因
```

这条原则在三个层面被强制：

* 配置层：`scheduler.local_first` 默认 `true`，关闭会在 `doctor` 里告警；
* 代码层：`Scheduler::decide()` 的本地判断在云端构造之前；
* 测试层：`tests/layering.rs` 里的 `c4_*` 用例会扫描源码验证顺序。

### 三个"唯一"

| 唯一 | 含义 | 为什么 |
|------|------|--------|
| 唯一收口 | 所有操作经 API 网关 | 上层不感知算力来源；CLI/TUI 无法绕过调度直连引擎 |
| 唯一引擎 | 本地与云端都用 llama.cpp | 同一个模型、同一套参数，推理行为完全一致 |
| 唯一格式 | 只支持 GGUF | 与 llama.cpp 生态完全对齐，不引入格式转换链 |

### 四层架构

```
┌─────────────────────────────────────────────────────┐
│ 用户交互层    mangosteen-cli                          │  CLI + 全屏 TUI
│               只与网关对话，禁止直连引擎/云厂商          │
└───────────────────────┬─────────────────────────────┘
                        │
┌───────────────────────▼─────────────────────────────┐
│ API 网关层    mangosteen-gateway                      │  系统唯一功能收口
│  ① 资源评估引擎  → 显存/内存/算力够不够               │
│  ② 调度决策器    → 本地优先，必要时降级               │
│  ③ 统一协议适配  → OpenAI 兼容接口                    │
└──────────┬────────────────────────┬─────────────────┘
           │                        │
┌──────────▼──────────┐  ┌──────────▼─────────────────┐
│ 设备执行（本地/云端共用）│  │ 节点供应                    │
│ mangosteen-device    │  │ mangosteen-provider           │
│  模型下载/引擎/运行时│  │  GPU 实例 CRUD / 节点连接    │
└──────────┬──────────┘  └──────────┬─────────────────┘
           │                        │
┌──────────▼────────────────────────▼─────────────────┐
│ 基础支撑      mangosteen-base                         │
│  模型管理 / 配置持久化 / 部署台账 / 文件访问控制        │
├─────────────────────────────────────────────────────┤
│ 平台适配层    mangosteen-platform                     │
│  硬件采集 / 路径规范 / 进程信号 / 原子写              │
├─────────────────────────────────────────────────────┤
│ 领域层        mangosteen-core / orchestration          │
│  纯类型与算法：估算公式 / 状态机 / 打分 / 协议结构      │
└─────────────────────────────────────────────────────┘
```

各层依赖只能自上而下，由 `cargo test -p mangosteen-cli --test layering` 强制校验。

---

## 命令一览

| 命令 | 作用 |
|------|------|
| `deploy <模型>` | 部署（本地优先，不足自动上云） |
| `run <模型>` | 部署并前台保持运行，Ctrl-C 优雅退出 |
| `stop <ID> \| --all` | 停止并释放资源 |
| `status` | 系统状态（硬件 / 引擎 / 配置 / 活跃部署） |
| `ps` | 列出全部部署记录 |
| `models add/list/show/verify/remove` | 模型管理 |
| `assess <模型>` | **只评估不部署**，看本地能不能跑 |
| `cloud providers/specs/pick/cheapest/instances/destroy` | 云端算力 |
| `config show/init/path/validate` | 配置管理 |
| `serve` | 只起 API 网关（多个终端协作时用） |
| `tui` | 全屏交互界面 |
| `doctor` | 环境自检 |

### 常用组合

```bash
# 花钱前先问清楚：本地能跑吗？跑不动需要多大云端显存？
mangosteen assess llama3.3-70b --context 8192

# 只允许本地，绝不花钱
mangosteen deploy qwen2.5-7b --local-only

# 云端选点，看清打分依据（不创建实例、不产生费用）
mangosteen cloud pick --min-vram 24 --max-price 3.0 --explain

# 就要最便宜的：纯价格序（不受打分策略影响，同样不创建实例）
mangosteen cloud cheapest --min-vram 24

# 一步拿到「当前可用的最低价」机器（省钱档：自动跳过无货的卡型）
mangosteen cloud cheapest --create --yes --name mn-dev
# 用完销毁（裸实例不在部署台账里，不会被 stop/abort 自动回收）
mangosteen cloud destroy --instance compshare/uhost-xxxx --yes

# 一个终端部署，另一个终端只提供 API
mangosteen deploy qwen2.5-7b &
mangosteen serve --port 8080
```

---

## 配置

配置文件位置（`mangosteen config path` 可查）：

| 平台 | 路径 |
|------|------|
| macOS | `~/Library/Application Support/mangosteen/config.toml` |
| Linux | `${XDG_CONFIG_HOME:-~/.config}/mangosteen/config.toml` |
| Windows | `%APPDATA%\mangosteen\config.toml` |

关键片段：

```toml
[gateway]
port = 11516                # 默认值；11434 留给 Ollama 守护进程
host = "127.0.0.1"
require_api_key = false

[scheduler]
local_first = true                    # 不要关掉
max_fallbacks = 2                     # 降级重试上限
max_price_per_hour = 5.0              # 云端价格上限（元/小时）

[scheduler.weights]
price = 40                            # 四维相对权重，不必加和为 1
latency = 15
availability = 30
hardware = 15

[local]
default_context = 8192
kv_cache = "f16"
allow_cpu_fallback = true
allow_partial_offload = true
port_range_start = 11435
port_range_end = 11999

[storage]
models_dir = "~/Library/Application Support/mangosteen/models"
cache_dir = "~/Library/Caches/mangosteen"
verify_hash = false                   # 登记时是否立即算 SHA-256（大文件很慢）
allowed_read_roots = []               # 额外授权访问的目录

# 当前唯一内置、已验证的 GPU 供应商：优云智算（UCloud RPC 形态）
# 其它厂商暂不内置（配置 profile 可动态接入，见下文「服务商 API 改版」）
# 凭证两段：公钥放 options，私钥放 secret
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

#### 为什么优云智算要单独一个 `driver`

它的开放接口是 **UCloud RPC 形态**，与其它服务商的 REST 形态有四处根本差异：

| 维度 | 常见 REST 服务商 | 优云智算 |
|------|------------------|----------|
| 端点 | 每个动作一个路径 | 全部 `POST https://api.compshare.cn` |
| 动作区分 | HTTP 方法 + 路径 | 表单参数 `Action=...` |
| 认证 | 请求头 Token | 参数 `PublicKey` + `Signature`（SHA1 签名） |
| 成功判定 | HTTP 2xx | HTTP 2xx **且** `RetCode == 0` |

签名无法用 profile 的字段映射表达，所以由 `mangosteen-provider/src/compshare` 提供专用驱动。
驱动替你处理了厂商的三个硬限制：**创建前强制查库存**、**销毁前必须先关机**、
**状态并发锁（10 分钟 TTL）**。上层调用方式与其他服务商完全一致。

### 服务商 API 改版怎么办

不用等新版本。在配置里覆盖 profile 即可：

```toml
[providers.mycloud]
driver = "mycloud"
enabled = true
secret = { env = "MYCLOUD_TOKEN" }

[providers.mycloud.profile]
name = "mycloud"
display_name = "我的算力平台"
[providers.mycloud.profile.endpoints]
base_url = "https://api.mycloud.example"
list_specs = "/v1/gpu/specs"          # ← 厂商改了就改这里
# ...
```

这条配置路径**不需要改代码、不需要重新编译**；核对完成的厂商才会进入
内置模板清单（当前为空，唯一受支持的 `compshare` 走专用驱动）。

---

## 资源评估是怎么算的

一次本地部署占用的显存/内存由四块构成：

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

---

## 云端是怎么选的

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
★ autodl/RTX 4090 [cn-west-1] ¥2.380/h · 42ms · 24.0GiB │ 价格 1.00 延迟 1.00 可用 0.92 硬件 0.94 → 总分 0.9654
  ucloud/RTX 4090 [cn-east-2] ¥2.650/h · 78ms · 24.0GiB │ 价格 0.90 延迟 0.54 可用 0.92 硬件 0.94 → 总分 0.8123
```

---

## 故障处理

### `mangosteen doctor` 说引擎有问题

Doctor 会真正执行 `--version` 验证，不只看文件是否存在。

**情况一：动态库找不到**（最常见，且通常**不需要**你动手）

```
[FAIL] 本地推理引擎
       /usr/local/bin/llama-server 存在但无法运行：Library not loaded:
       @rpath/libllama-server-impl.dylib ... no LC_RPATH's found
```

Mangosteen 已经自动注入库搜索路径，如果仍然失败，说明库不在常见目录。按顺序查：

```bash
mangosteen engine env      # 看它注入了什么、从哪些目录找
mangosteen engine locate   # 看搜索过哪些目录
```

确认库的真实位置后，在配置里手工指定：

```toml
[local.engine_env]
DYLD_FALLBACK_LIBRARY_PATH = "/opt/my-llama/lib"
```

**情况二：架构不匹配**

```
Bad CPU type in executable
```

Homebrew 在 Apple Silicon 上装到了 `/usr/local`（Intel 路径），或反之。
用 `mangosteen engine locate` 看实际路径，必要时用 `local.engine_path` 指向正确的那份。

**情况三：引擎根本没装**

```
[FAIL] 本地推理引擎
       未找到 llama-server。已搜索：/usr/local/bin: /usr/bin: ...
```

Doctor 会把**搜索过的目录**全列出来——这比让你猜 PATH 有没有配更直接。

### 部署失败了，怎么知道原因

每次部署都会落台账（`state.json`），失败原因在里面：

```bash
mangosteen ps                          # 看状态
mangosteen status                      # 看活跃部署 + 引擎路径
```

引擎日志在 `<日志目录>/engine-<端口>.log`，错误消息里会给出完整路径。

### 云端实例忘了关

```bash
mangosteen ps                    # 找出来
mangosteen stop <部署ID>          # 停掉
mangosteen stop --all --yes      # 或者全停
mangosteen cloud instances       # 只看云端实例
```

台账是**每次状态变更立即落盘**的，进程重启也不会丢——这是为了避免忘记关机导致持续扣费。

### 端口被占用

```bash
mangosteen stop --all            # 清理残留进程
```

或改配置 `local.port_range_start` / `port_range_end`。

---

## 开发

```bash
cargo build --workspace            # 构建
cargo test --workspace             # 全部测试（含 e2e 与架构约束校验）
cargo test -p mangosteen-cli --test layering   # 只跑架构约束
```

### 代码结构

```
crates/
├── mangosteen-core        领域层：类型 + 纯算法（无平台代码、无 IO）
│   ├── model.rs           GGUF 规格、量化格式
│   ├── hardware.rs        硬件画像
│   ├── estimate.rs        资源估算（纯函数，100% 可单测）
│   ├── plan.rs            部署方案
│   ├── cloud.rs           多云统一抽象 + 驱动接口
│   ├── policy.rs          四维打分
│   ├── config.rs          配置模型
│   ├── gguf.rs            GGUF 解析
│   ├── openai.rs          OpenAI 协议结构
│   └── net.rs             隧道/端口映射模型
├── mangosteen-platform    平台适配层：所有 cfg(target_os) 都在这里
│   ├── hardware.rs        nvidia-smi / system_profiler / /proc / WMI
│   ├── paths.rs           XDG / Library / AppData
│   ├── process.rs         子进程 + 信号 + 僵尸回收
│   ├── docconv.rs         富文档转换工具选择（textutil / pdftotext / Word COM）
│   └── fs.rs              原子写 + 权限 + 符号链接防护
├── mangosteen-base        基础能力层
│   ├── models.rs          模型台账
│   ├── config.rs          配置发现与持久化
│   ├── state.rs           部署台账
│   └── guard.rs           文件访问控制
├── mangosteen-device       本地链路：llama.cpp 引擎管理
├── mangosteen-backend     厂商协议适配：优云智算（UCloud RPC 形态）
│   ├── sign.rs / sha1.rs  UCloud V1 签名（SHA1 + 参数拼接）
│   ├── params.rs          参数扁平化（签名用原值、传输用转义）
│   ├── client.rs          各 Action 调用（创建/查询/启停/删除/价格/镜像/可用区）
│   ├── mapping.rs         单位/状态/SSH 端口/价格的唯一换算点
│   ├── retcode.rs         返回码语义归类（容量不足→换节点 / 欠费→停止重试）
│   └── driver.rs          ProviderDriver + 业务补偿（先查库存、先关机再删）
├── mangosteen-provider       云端链路：驱动 + 编排 + SSH 隧道
├── mangosteen-gateway     网关层：评估/调度/协议/HTTP
│   ├── assess.rs          资源评估引擎
│   ├── scheduler.rs       调度决策器（本地优先）
│   ├── protocol.rs        统一协议适配
│   ├── gateway.rs         门面
│   └── server.rs          HTTP 服务
└── mangosteen-cli         交互层：CLI + TUI
```

### 设计文档

* [`docs/DESIGN.md`](docs/DESIGN.md) —— 系统架构、模块接口、数据结构、决策流程
* [`docs/VERSUS-OLLAMA.md`](docs/VERSUS-OLLAMA.md) —— 与 Ollama 的差异化分析

---

## 产品边界

Mangosteen 是**个人级桌面客户端**，明确不做：

* 集群管理、多租户、分布式推理（企业级能力）
* 内置对话界面、Agent 框架（只提供 API，上层应用自带）
* 第二个推理引擎（vLLM / TensorRT-LLM 等）

云端优化目标为 RTX 4090 / 5090 等消费级显卡实例；其它硬件仅做扩展兼容。

---

## 许可

Apache-2.0
