# Mangosteen 系统设计文档

> 本文是 Mangosteen（山竹）的技术基线，与代码仓当前实现严格对齐。  
> 所有实现必须与本文一致；需要偏离时先改本文、再改代码。

## 目录

1. [设计目标与边界](#1-设计目标与边界)
2. [分层架构](#2-分层架构)
3. [核心数据结构](#3-核心数据结构)
4. [资源评估引擎](#4-资源评估引擎)
5. [调度决策器](#5-调度决策器)
6. [多云统一抽象](#6-多云统一抽象)
7. [后端节点选择器](#7-后端节点选择器)
8. [云端网络工具集](#8-云端网络工具集)
9. [部署编排与生命周期](#9-部署编排与生命周期)
10. [统一协议适配层](#10-统一协议适配层)
11. [会话管理](#11-会话管理)
12. [基础能力层](#12-基础能力层)
13. [平台适配层](#13-平台适配层)
14. [配置体系](#14-配置体系)
15. [错误处理与降级](#15-错误处理与降级)
16. [安全设计](#16-安全设计)
17. [测试策略](#17-测试策略)
18. [架构约束的自动化守护](#18-架构约束的自动化守护)

---

## 1. 设计目标与边界

### 目标

| 目标 | 衡量方式 |
|------|---------|
| 用户无需理解算力来源 | 上层只面对一个 OpenAI 兼容 base URL |
| 自动选择最优 GPU 节点 | 纯价格序或四维打分；云端方案必带非空降级轨迹 |
| 云端行为一致 | 同一套 llama.cpp 参数语义；同一份资源估算公式 |
| 新增云厂商不改上层代码 | 配置化驱动；`ProviderDriver` 接口稳定 |
| 三平台原生可用 | 所有平台差异收敛到 `mangosteen-platform`（本地支持 macOS / Windows / Linux） |

### 明确不做

- 集群、多租户、分布式推理
- Agent 框架
- 本地模型推理部署（`target=local` 被调度器明确拒绝，本地推理交由 Ollama 承担）
- 远端节点非 Linux 系统（镜像选择强制 Linux，见 §6.6）

理由：这些能力会显著增加复杂度，却不服务于「个人开发者把大模型部署到云端 GPU」这个单一场景。

### 已调整的边界（记录变更，避免照旧基线改回去）

| 原边界 | 现基线 | 调整原因 |
|--------|--------|----------|
| 不内置对话界面 | UI 提供完整对话窗口 | 部署 + 使用一体：部署完就在同一个客户端里对话 |
| 推理引擎唯一 llama.cpp | 引擎插件化（llama.cpp + Ollama 已实现） | 用户已有不同栈；差异收敛在 `EnginePlugin`，新增引擎不改调度主链路 |
| 模型格式只用 GGUF | 下载不限格式，只标识类型 | 非 GGUF 是别的引擎要用的；替他决定「只能下 GGUF」是错的 |

---

## 2. 分层架构

### 2.1 五层

自上而下：**用户会话与交互 → API 网关 → 编排调度核心 → 设备执行与节点供应 → 基础支撑与平台适配**。

```
┌─ 第一层  用户会话与交互层 ─────────────────────────────────┐
│  mangosteen-session   会话领域模型 + 持久化                  │
│  交互入口：mangosteen-cli（命令行 + ui 启动 Web 控制台）      │
│            mangosteen-webui（单文件 Web 控制台静态资源）      │
└──────────────────────┬────────────────────────────────────┘
                       │ 只与 API 网关通信
┌──────────────────────▼────────────────────────────────────┐
│ 第二层  API 网关层（mangosteen-gateway）                    │
│  唯一功能收口：HTTP 服务（标准库手写，8 worker 线程池）        │
│  · 统一协议适配层（/v1/*：OpenAI + Anthropic 兼容）           │
│  · 部署管理接口（/api/*，54 条路由）                          │
│  · 鉴权、协议解析、请求路由、状态回传                         │
│  · 四个后台线程：runprobe / sshmon / cloudmon / pull_worker   │
└──────────────────────┬────────────────────────────────────┘
                       │
┌──────────────────────▼────────────────────────────────────┐
│ 第三层  编排调度核心层（mangosteen-orchestration）            │
│  ① 模型评估    assess（Assessor，纯估算 + 文件头解析）         │
│  ② 模式决策    scheduler::decide（四条不变量 I1–I4）           │
│  ③ 部署编排    deploy / provision（阶段划分 + perf 计时）      │
│  ④ 运行生命周期 lifecycle（stop / stop_service / unload /      │
│     恢复 / 中止回滚；关实例判据 = 引擎端口占用制）              │
│  ⑤ 节点选择    cloud / 复用（按 GPU 型号复用实例）              │
└──────────────────────┬────────────────────────────────────┘
                       │
┌──────────────────────▼────────────────────────────────────┐
│ 第四层  设备执行与节点供应                                    │
│  mangosteen-device（本地/云端共用）：引擎插件、本地/远端拉起、  │
│    探活、停止、远端模型下载（aria2c/curl）                     │
│  mangosteen-provider（仅节点供应）：GPU 实例 CRUD、厂商驱动、   │
│    SSH 隧道、远端执行（不运行模型）                             │
│  mangosteen-base：模型/部署台账、配置存储、下载、文件访问控制    │
└──────────────────────┬────────────────────────────────────┘
                       │
┌──────────────────────▼────────────────────────────────────┐
│ 第五层  基础支撑层                                           │
│  mangosteen-core（领域类型 / 配置 / 错误 / SPI / 纯算法）      │
│  mangosteen-platform（路径 / 进程 / 硬件 / 信号 / docconv）    │
└───────────────────────────────────────────────────────────┘
```

### 2.2 crate 与层的对应关系

层是**逻辑划分**，crate 是**编译单元**。共 10 个 crate，约 6.3 万行（src）。

| 层 | crate | 规模 | 说明 |
|----|-------|------|------|
| 一 | `mangosteen-session` | ~560 行 | 会话领域模型 + 持久化 |
| 一 | `mangosteen-cli` | ~5,300 行 | 命令行入口（含 layering 架构守护测试 1,031 行） |
| 一 | `mangosteen-webui` | 单文件 | 内嵌 Web 控制台（index.html ~2,600 行，无 CDN） |
| 二 | `mangosteen-gateway` | ~8,100 行 | HTTP 服务、路由、协议、唯一收口 |
| 三 | `mangosteen-orchestration` | ~8,500 行 | 评估、决策、部署编排、生命周期 |
| 四 | `mangosteen-device` | ~3,700 行 | 引擎插件、进程运行时、远端执行 |
| 四 | `mangosteen-provider` | ~17,500 行 | 云驱动（compshare 专用 + REST 通用）、隧道 |
| 四 | `mangosteen-base` | ~5,400 行 | 配置、台账、下载、量化、提取、联网抓取 |
| 五 | `mangosteen-core` | ~10,800 行 | 领域契约与纯算法 |
| 五 | `mangosteen-platform` | ~3,500 行 | 跨平台唯一落地点 |

`Gateway` 是 `Orchestrator` 的类型别名（gateway.rs），网关不持有执行状态——编排实体在第三层，网关只做转发与收口。

### 2.3 依赖方向

严格自上而下，`core` 不依赖任何自家 crate。

```
core ←── platform ←── base ←── session
  ↑         ↑         ↑    ←── device
  ↑         ↑         └────── provider（同层组合，复用 device 远端运行时）
  ↑         └──────────────── orchestration（评估/调度/部署编排/生命周期）
  └─────────────────────────── gateway（唯一 HTTP 收口）
                              cli（core/platform/base/gateway）
```

**为什么 device / provider 不依赖 base**：它们只负责「把方案变成运行中的服务」与「把节点准备好」，不需要台账/配置能力。台账由编排/网关调用 `base` 完成，执行层可脱离存储独立测试。

**为什么 provider 可以依赖 device**：云端部署同样需要下载模型、安装并启动推理引擎；执行语义由 device 统一持有，provider 只负责创建 GPU 节点与建立连接。

**为什么 session 只依赖 core + platform**：会话层不感知算力来源——不判断能不能部署、不启动引擎、不访问云厂商。由 `c6_session_layer_does_not_know_about_compute` 守护。

### 2.4 各层职责与禁止事项

| 层 | crate | 可以做 | 不可以做 |
|----|-------|--------|----------|
| 一·会话 | `session` | 会话 CRUD、消息追加、操作记录 | 判断可部署性、启动引擎、访问云 API |
| 一·交互 | `cli` / `webui` | 解析参数、格式化输出、调用 `/api` 与 `/v1` | 直连引擎、直连云厂商 API、写平台分支 |
| 二 | `gateway` | 鉴权、路由、协议转换、状态回传 | 写平台专属代码、自己实现推理、在锁内做网络调用 |
| 三·评估 | `assess` | 采集硬件、估算显存/内存、判定可行性 | 决定用本地还是云端 |
| 三·选点 | `scheduler` | 云端候选打分与排序 | 调厂商 API |
| 三·编排 | `deploy` / `provision` | 部署编排、生命周期、阶段推进 | 解析 HTTP、做协议转换 |
| 三·配置 | `core::plan` | 生成云端规格与 `EngineLaunchConfig` | 执行部署 |
| 三·引擎 | `device::plugin` | 引擎插件注册与分发 | 决定是否该用本地 |
| 四·节点 | `provider` | 实例 CRUD、隧道、端口映射、厂商签名 | 下载模型、启停推理服务、决定选哪家 |
| 四·执行 | `device::remote` | 远端拉起进程、探活、终止、模型下载 | 暴露 HTTP 给上层 |
| 五 | `core` / `platform` | 类型、纯算法、平台能力 | 含业务调度逻辑 |

---

## 3. 核心数据结构

### 3.1 模型规格 `ModelSpec`

资源评估的输入。**只描述模型自身，不含任何部署决策。**

```rust
pub struct ModelSpec {
    pub name: String,          // "qwen2.5-0.5b-instruct"
    pub alias: Option<String>, // API 模型别名（如 qwen2.5-0.5b）
    pub params_b: f64,         // 7.615
    pub quant: Quantization,   // Q4_K_M
    pub n_layers: u32,
    pub n_heads: u32,
    pub n_kv_heads: u32,       // GQA 的关键
    pub embedding_dim: u32,
    pub vocab_size: u32,
    pub native_context: u32,
    pub file_size_bytes: u64,  // 实测优先
}
```

来源优先级：GGUF 文件头解析（首选）> 台账已有记录 > 用户显式指定。

### 3.2 硬件画像 `HardwareProfile`

```rust
pub struct HardwareProfile {
    pub os: String,                     // "macos" / "linux" / "windows"
    pub arch: String,
    pub host: HostProfile,              // 内存 + CPU
    pub accelerators: Vec<Accelerator>, // GPU（可多卡）
}
```

| 量 | 计算 | 依据 |
|----|------|------|
| 可用显存 | 独显取 `vram_bytes`；Apple 统一内存取 `× 0.70` | 统一内存与系统共享 |
| 可用内存 | `total − max(total×0.25, 1GiB)` | llama.cpp 内存不足会 swap，吞吐塌一个数量级 |
| 建议线程 | `physical_cores`（不是逻辑核） | 超线程在 llama.cpp 线程池里争用 |

### 3.3 部署方案 `DeploymentPlan`

网关内部「唯一真相」，也是台账持久化实体。

```rust
pub struct DeploymentPlan {
    pub id: String,
    pub model: ModelSpec,
    pub origin: ComputeOrigin,           // 当前唯一变体：Cloud{provider, instance_id}
    pub cloud: Option<CloudPlan>,
    pub assessment: Assessment,          // 评估快照（审计）
    pub created_at: i64,
    pub fallback_trail: Vec<String>,     // 必非空（I2）
}
```

**不变量**：`origin == Cloud` 时 `fallback_trail` 必须非空（I2）；空轨迹由 `assemble_plan()` 补 `"cloud:unspecified"`。

`LocalPlan`（结构保留）字段与 llama.cpp 参数一一对应：`model_path → --model`、`context_size → --ctx-size`、`threads → --threads`、`batch_size → --batch-size`、`gpu_layers → --n-gpu-layers`、`kv_cache_type_k/v → --cache-type-k/v`、`flash_attention → --flash-attn`、`use_mlock → --mlock`、`host/port → --host/--port`。

`CloudPlan` 云端方案：`engine_port`（远端监听端口，默认 8080，多模型节点分配 8080–8191）、`model_path`（远端模型文件路径）、`download_url/download_bytes`（远端直连下载）、`node_ip/node_ssh_port/node_ssh_user`（部署成功后回填，供 UI 展示）、`local_port`（隧道建立后本地映射端口）、`engine: LocalPlan`（云端参数语义与本地完全一致）。

### 3.4 云实例 `Instance`

归一化后的唯一形态。上层不会看到任何厂商私有字段。

```rust
pub struct Instance {
    pub id: String,
    pub provider: String,
    pub state: InstanceState,     // Pending/Creating/Created/Starting/Running/Stopping/Stopped/Destroyed/Failed
    pub spec: InstanceSpec,       // 归一化规格
    pub network: NetworkEndpoint, // SSH 与推理端口
    pub created_at: i64,
    pub price: Option<PriceQuote>,
    pub display_name: Option<String>,
}
```

状态机迁移由 `InstanceState::can_transition_to()` 集中定义（21 条合法迁移），**驱动不得自行发明状态**。`is_billing_compute()` = `Starting|Running|Stopping|Creating`。

---

## 4. 资源评估引擎

### 4.1 估算公式

```
总需求 = weights + kv_cache + compute_buffer + runtime_overhead
```

| 项 | 公式 | 说明 |
|----|------|------|
| weights | `file_size_bytes`（实测优先）否则 `params × bpw / 8` | GGUF 含元数据与对齐 |
| kv_cache | `2 × n_layers × n_kv_heads × head_dim × ctx × kv_type_factor` | f16=1.0, q8_0≈0.53, q4_0≈0.28 |
| compute_buffer | `batch × embed_dim × 8 × 2 / 4`，夹在 `[192MiB, 4GiB]` | 保守值 |
| runtime_overhead | NVIDIA/AMD 400MiB；Intel 320MiB；Apple 250MiB；CPU 64MiB | 经验值 |

### 4.2 落位形态判定顺序

```
1. 全量 GPU 卸载（gpu_layers = n_layers + 1）   ← 最优
2. 部分卸载（gpu_layers 由预算反算）            ← 吞吐下降 40%~70%
3. 纯 CPU（allow_cpu_fallback 时可接受）        ← 极慢
4. Impossible                                  ← 本地不可行 → 云端
```

「尽可能大上下文」模式（请求未指定 `context_len`）：先在 `native_context` 下按 2 的幂收敛找最大可用 ctx（对齐 256 整数倍），逐级尝试 KV 精度 F16 → Q8_0 → Q4_0（`DOWNGRADE_CHAIN`）；仍放不下则回退到 4K 并尝试部分卸载。

### 4.3 量化选档

`best_quant_within()` 从 `F32` 向 `IQ1_S` 遍历 `Quantization::ALL_DESCENDING`，返回第一个能塞进预算的档位。

### 4.4 告警而非失败

| Warning | 触发条件 |
|---------|---------|
| `QuantMismatch` | 声明位宽与实测偏差 > 15% |
| `ContextClamped` | 请求 ctx 超过模型原生上限 |
| `KvCacheDowngraded` | 因显存紧张降了 KV 精度 |
| `PartialOffloadPenalty` | 只能部分卸载 |
| `CpuFallback` | 无加速卡 |
| `UnifiedMemoryContention` | Apple Silicon 统一内存 |

### 4.5 粗估兜底

无 GGUF 元数据时（如仅给下载地址），`evaluate_estimated` 按「32 层 / 32 头 / 8 KV 头 / 4096 维 / 151936 词表 / 32768 原生上下文」的 7B 级典型值估算（误差 2 倍内），下载完成后用真实文件重估。

---

## 5. 调度决策器

### 5.1 决策流程（纯云端）

```
部署请求 ──▶ Scheduler::decide
               │
               ├─ forces_local 或 allow_cloud=false ──▶ Reject（明确拒绝）
               │
               └─ 其余 ──▶ build_cloud_plan
                              │
                              ▼
                   云端候选打分 / 价格序 → provision
```

**产品的定位是「只部署到远端 GPU 节点」**：`target=local` 必须被**明确拒绝**（I3），绝不静默上云产生计费实例；本地推理交由 Ollama 承担。本地失败后的降级路径已废弃（仅保留错误消息生成）。

### 5.2 四条不变量

| 编号 | 内容 | 守护方式 |
|------|------|---------|
| I1 | `allow_cloud = false` 时绝不产生云端方案 | `scheduler::tests::invariant_i1_*` |
| I2 | 云端方案必然带非空 `fallback_trail` | `invariant_i2_*` + `assemble_plan` 兜底 |
| I3 | `target = local` 必然被拒绝（不会自动改用云端） | `invariant_i3_*` |
| I4 | 降级重试次数 ≤ `max_fallbacks`（默认 2，上限 10） | `invariant_i4_*` |

### 5.3 端口分配

- 本地端口区间 `local.port_range_start/end`（默认 11435–11999），由 `Scheduler` 内嵌分配器管理；网关构造时把台账活跃部署端口灌入 `reserved`；每次分配跳过 `reserved` 与不可绑定端口；拒绝 <1024 特权端口
- 远端引擎端口 `allocate_remote_engine_port`：范围 8080–8191，只看**活跃**部署（stopped/failed 不参与，否则首台机器第一个模型会从 8081 起）
- 分配器抽成独立结构 `PortAllocator`（core::net）以便单测

---

## 6. 多云统一抽象

### 6.1 驱动接口

```rust
pub trait ProviderDriver: Send + Sync {
    // 10 个必选
    fn name(&self) -> &str;
    fn display_name(&self) -> &str;
    fn capabilities(&self) -> DriverCapabilities;
    fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>>;
    fn create(&self, spec_id: &str, name: Option<&str>) -> MnResult<Instance>;
    fn start(&self, instance_id: &str) -> MnResult<Instance>;
    fn stop(&self, instance_id: &str) -> MnResult<Instance>;
    fn destroy(&self, instance_id: &str) -> MnResult<()>;
    fn describe(&self, instance_id: &str) -> MnResult<Instance>;
    fn list(&self) -> MnResult<Vec<Instance>>;
    // 3 个带默认实现
    fn probe_latency_ms(&self) -> Option<u32> { None }
    fn set_password(&self, ...) -> MnResult<()> { /* password_reset_unsupported */ }
    fn instance_metrics(&self, ...) -> MnResult<Option<InstanceMetrics>> { Ok(None) }
}
```

约束：禁止 `unwrap` 网络结果或透传厂商错误码（必须归一化为 `MnError::Provider`）；除 `create` 外全部幂等；不暴露厂商私有字段。

### 6.2 两条新增服务商路径

**配置路径（首选）**——`RestDriver` + `RestDriverProfile`（认证头/URL 路径/字段映射/状态枚举全部可配），写一段 TOML 即可接入 REST 形态厂商，无需改代码。

**代码路径（特殊场景）**——厂商需要签名算法、分页聚合、异步任务轮询时，新写一个 `impl ProviderDriver` 并在 `registry::build_registry` 注册。

### 6.2.1 compshare 专用驱动（协议形态完全不同的厂商）

优云智算用 UCloud RPC 形态：全部 `POST https://api.compshare.cn`、表单参数 `Action=...` 区分动作、`PublicKey + Signature`（SHA1）认证、扁平表单编码、成功判定 = HTTP 2xx **且** `RetCode == 0`。签名无法用字段映射表达，所以放进 `mangosteen-provider::compshare`：

| 模块 | 职责 |
|------|------|
| `sign` / `sha1` | UCloud V1 签名（`concat(key+value)` 字典序 + 私钥，SHA1 小写 hex；数组元素直接首尾相接；官方向量钉死） |
| `params` | 参数扁平化；**签名用原值、传输用 URL 转义** |
| `model` | 厂商响应结构（字段与文档逐字一致，全部 default） |
| `mapping` | 唯一换算点：单位（MB/GB）、状态、SSH 端口、价格折算；Linux 镜像强制（`pick_linux_image` 刻意不回退 Windows 镜像） |
| `retcode` | 返回码语义归类（226604 容量不足→换节点 / 226601 欠费→停止重试 / 226605 镜像任务冲突 / 8903 异步任务中→重试） |
| `client` | 各 Action 的具体调用（29 个方法，见 §6.6） |
| `driver` | `ProviderDriver` 实现 + 业务补偿 |

**驱动层承担的业务补偿**（上层不必认识厂商限制）：
1. **创建前查库存**：用 `CheckCompShareResourceCapacity`（文档要求，不能用 Describe 代替）；无库存返回可降级错误
2. **销毁前先关机**：`TerminateCompShareInstance` 仅对 `Stopped` 有效；`destroy()` 内部完成「关机 → 等 Stopped → 删除」
5. **创建后补详情可失败但 ID 必须保留**：否则编排器拿不到 ID，回滚无从谈起
6. **统一固定密码**：所有云端实例强制 `1qaz@WSX`（`restore_login_credential` 异步生效后验证；失败重启实例再验证）
7. **伪 Running 防线**：厂商状态滞后时 `force_reboot_if_unreachable` 唯一有效处置是 stop→等真实 Stopped→start

**单位陷阱**：创建/价格接口的 `Memory` 是 **MB**，机型表/库存接口是 **GB**。混用差 1024 倍且不报错。

### 6.3 驱动注册表的容错

`build_registry_with` 返回 `(registry, skipped)`：**单个服务商配置错误不中断其它服务商**——某个密钥没配好，不该让整个客户端不可用。`builtin_driver_names()` 当前返回空：只有 compshare 经过真实验证，其余厂商只能通过配置 `profile` 段接入（REST 映射未核对）。

### 6.4 传输层抽象

驱动只依赖 `Transport` trait（`execute(&self, req) -> MnResult<HttpResponse>`）。

- 生产：`CurlTransport`（系统 curl，自带 TLS，三平台自带，无新增供应链）
- 测试：`MockTransport`（预设响应 + 记录请求）

**为什么用 curl 而不是 HTTP 客户端库**：云端 API 低频（分钟级），进程启动开销 ~10ms 可忽略；认证头通过 `--config` 从 0600 临时文件读取，避免出现在进程参数列表；`--noproxy '*'` 防止继承消亡代理；**URL 只能出现一次**（曾把 URL 同时写进 config 与 argv，curl 发两次请求，POST 创建意味着可能开两台计费实例，由 `url_must_appear_exactly_once` 守护）。

### 6.5 云端实例监控

```
后台线程（每 5 秒）──持锁读台账──▶ provider + instance_id
        └──释放锁──▶ 厂商监控接口（~0.5s）──▶ 共享快照
HTTP /api/sysinfo ◀── 只读内存（微秒级）
```

1. 绝不在请求路径里调厂商接口（设备页 2 秒轮询会卡页面、持网关锁、触发限流）
2. 绝不在网关锁内做网络往返
3. 单位归一化放驱动内（厂商 0~100 vs 本地 0~1）

空态三态可分：`supported:false`（无监控接口）/ `has_any:false`（暂无数据）/ `error`（失败保留上次快照）。

### 6.6 compshare GPU 接口覆盖矩阵

厂商 OpenAPI 共 71 个接口。`client.rs` 已实现 29 个 Action（覆盖部署全链路）：

| 分类 | 已适配 | 关键约束 |
|------|--------|---------|
| 可用区/机型 | `DescribeCompShareSupportZone`、`DescribeAvailableCompShareInstanceTypes`、`DescribeCompShareMachineTypeFamilies` | 可用区三级回退：虚机+镜像兼容 → 任意虚机 → 任意可用区 |
| 库存/价格 | `CheckCompShareResourceCapacity`、`GetCompShareInstancePrice`、`GetCompShareInstanceUserPrice`、`GetCompShareInstanceUpgradePrice`、`GetCompShareRefundPrice` | 库存必查；退款/监控数组走 JSON 通道 |
| 实例 CRUD | `CreateCompShareInstance`、`DescribeCompShareInstance`、`StartCompShareInstance`（仅 Stopped）、`StopCompShareInstance`（仅 Running）、`RebootCompShareInstance`、`TerminateCompShareInstance`（仅 Stopped）、`ModifyCompShareInstanceName`、`ResetCompShareInstancePassword`（Base64）、`ReinstallCompShareInstance`、`ResizeCompShareInstance`（计算/磁盘二选一）、`SwitchChargeType` | 状态前置校验统一放驱动层，给可操作提示 |
| 端口/网络 | `UpdateCompShareInstancePorts`（仅容器区、全量替换）、`SwitchCompShareEIPShareBandwidth`（顶层成功≠每个成功）、`CheckCompShareNetOptimizer`、`DescribeCompShareSoftwarePort`、`GetSoftwareURL` | 带数组参数必须走 JSON 请求体 |
| 其他 | `UpdateCompShareStopScheduler`、`DeleteCompShareStopScheduler`、`GetProjectList`、`GetCompShareInstanceMonitor`（必须 JSON 体） | 定时关机需 ProjectId；监控数组签名按元素拼接 |

明确不适配的 42 个：`image/*`（自制/社区镜像，17 个）、`data/*`（云盘管理，9 个）、`team/*`（团队/计费，16 个）——属于镜像制作、存储管理、账号体系，不是部署链路。

**三条跨接口经验**：① 带数组参数必须走 JSON 请求体（`UHostIds.0` 表单对部分接口全失败）；② JSON 通道签名口径不同（元素直接首尾相接）；③ `Memory` 单位随接口变化（MB/GB 混用差 1024 倍且不报错）。

---

## 7. 后端节点选择器

### 7.1 两阶段

**阶段一：硬性过滤**（`SelectionConstraints::admits`）——服务商/地域白名单、显存/内存/磁盘下限、价格上限、`require_stock` 时要求有库存。

**阶段二：排序**

- `SelectionStrategy::Cheapest`（默认）：满足约束的候选里价格升序取第一个（`cheapest_first` 纯价格序，不受配置权重影响）
- `SelectionStrategy::Balanced`：四维加权打分 `w_price·S_price + w_latency·S_latency + w_availability·S_avail + w_hardware·S_hw`，归一化基准为候选集内最优值

### 7.2 权重语义

配置写的是相对重要性（默认 price 40 / availability 30 / latency 15 / hardware 15），不必加和为 1；打分时统一归一化。GPU 型号偏好表（`default_gpu_preference`）：RTX 4090=1.20、RTX 5090=1.25、RTX 4080=1.05、RTX 3090=0.95、RTX 3080=0.85、A100=0.80、H100=0.75。

### 7.3 排序的确定性

`总分降序 → 价格升序 → provider 字典序 → spec_id 字典序`，同分同价保证结果可复现。

### 7.4 「就要最便宜」：纯价格序（`cheapest_first`）

`cloud cheapest` 与部署选点共用 `cheapest_cmp`（价格升序 → 库存多优先 → provider → spec_id），两处各写一遍必然漂移。**未知价必须排最后**，不能当 0 元：厂商对个别规格返回 `f64::MAX` 当无报价哨兵，落成 0.0 会排到第一，先去开最贵的卡。

### 7.5 「可用」由创建结果定义

厂商库存接口不可信（优云是模拟调度）。`create_first_running` 按价格序逐个真开，**只在「该规格无库存」这类可重试错误上换下一个候选**（`is_retryable_create_error`：226604/售罄/sold out/no stock）；鉴权、参数、余额类错误立刻中止。创建成功但起不来的实例必须先销毁（带重试）才换下一个，销毁失败立即返回错误并带残留实例 ID。

### 7.6 裸实例必须能被清理

`cloud cheapest --create` 建的是裸实例（不建隧道、不传模型、不写部署台账），不会被 stop/abort 自动回收；配套 `cloud destroy --instance` 清理路径，创建成功后 CLI 直接打印可复制执行的命令。

---

## 8. 云端网络工具集

### 8.1 SSH 隧道

```
ssh -N -T \
    -o ExitOnForwardFailure=yes \      ← 端口占用时立即失败，避免「假连通」
    -o ServerAliveInterval=15 \        ← 保活，防 NAT 静默断开
    -o ServerAliveCountMax=3 \
    -o ConnectTimeout=5 \              ← 不可达快速失败
    -o ConnectionAttempts=2 \
    -o StrictHostKeyChecking=accept-new \
    -p <ssh_port> \
    -L 127.0.0.1:<local>:127.0.0.1:<remote> \
    <user>@<host>
```

- **不再传 ControlMaster/ControlPersist**：厂商网关按 host:port 区分实例，控制套接字会被互不相干的会话争用（历史事故：`Connection closed by remote host` 连续刷屏）
- 就绪判据：**本地端口可连通**（不是「进程活着」——SSH 可能活着但转发已断）
- `TunnelManager::establish` 6 次重试、退避 3/6/9/12/15s，跨厂商网关冷启动窗口（30s+）
- 瞬时断链白名单（`is_transient_connect_failure`）：`Connection closed by` / `closed by remote host`（OpenSSH 9.x 带 IP） / `Connection reset by peer` / `Connection refused` / `Connection timed out` / `Broken pipe` / `ssh_dispatch_run_fatal`；认证失败、密钥缺失、主机不可达不重试

### 8.2 SSH 快速失败策略（远端执行层）

`ssh_exec` 失败两分类（`SshFailure`）：

| 类别 | 判定 | 行为 |
|------|------|------|
| Transient（可重试） | 退出码 255 且 stderr 未命中确定性关键词 | 重试，最多 2 次（总尝试 2） |
| Fatal（快速失败） | 认证/配置类（Permission denied、host key 变更、DNS 失败）+ TCP 不可达类（connect to host 阶段的 timeout/refused/unreachable/no route） | 立即报错 |

- **为什么不可达不重试**：每次烧满 ConnectTimeout（5s × ConnectionAttempts 2），5 次重试把「实例已关机」拖成数分钟假死（实测：手动关闭节点后 stop/deploy 卡住）
- **banner exchange 超时仍可重试**（TCP 已建立，只是握手慢）
- askpass 免密：密码落 0600 sidecar 文件，脚本只负责打印（Unix `#!/bin/sh + exec cat`、Windows `.cmd + type`）；成功后公钥进 `authorized_keys`（幂等 `grep -qxF`），主路径切换公钥认证

### 8.3 健康检查与自动重建

`ensure_alive()`：检查 → 断了就关旧进程、释放端口、重建；`heal(remote_port)` 按远端端口精确定位（同节点多模型）。runprobe 二道防线：连续 3 轮 `/health` 失败且过 60s 冷却 → `restore_deployment_tunnel` 重建。

### 8.4 存储映射

三种方式：`Mount`（SSHFS/SMB）、`Sync`（rsync，`--partial --inplace --info=progress2`）、`None`（云端自行下载）。

---

## 9. 部署编排与生命周期

### 9.1 部署阶段划分

```
① 模型评估 → ② 节点选择（实例创建/复用）→ ③ 节点配置（依赖 + llama-server）
→ ④ 模型下载（主模型 + 辅助模型）→ ⑤ 启动模型（拉引擎 + 健康检查）→ ⑥ 就绪
```

`DeployStage`（Assessing/Selecting/Configuring/Downloading/Launching/Tunneling/Ready/Failed）驱动前端步骤条；`perf.rs` 的 `StageTimer` 在每阶段进入/退出时输出 `[perf] ▶/✓/✗` 耗时日志，`DeployPerfTrace` 在结束时输出全程汇总（最长阶段占 40 字符柱状条）。

### 9.2 部署主流程

CLI `deploy`（`crates/cli/src/commands/deploy.rs`）：
1. 解析参数 → 查模板正文（设置页配置 → 内置默认模板）
2. **幂等部署检查**：同模型 `ready+active` 部署存在 → 打印「正在运行，跳过部署」返回 0；否则删除该模型全部旧台账后重新部署
2. 三条路径：模型在台账 → `gw.deploy`；不在台账但有模板 → 估算评估 → `prepare_with_progress` → `commit_cloud`；无模板 → 返回原始错误
3. `--watch` 每 5 秒轮询进度直至终态；成功后确保网关持有隧道（已运行 → `POST /api/deployments/{id}/restore-tunnel`；未运行 → 拉起守护网关 `ui --foreground --no-open`）

`prepare_with_progress`：先 `prune_inactive` 清非活跃记录（避免复用不可达旧实例）→ `evaluate` → `prepare_inner`（唯一性按模型 ID、引擎可用性检查）→ `prepare_vendor_node`（构造 SelectionConstraints：`min_memory ≥ 8GiB`、`min_disk ≥ 50GiB`）→ `configure_node` → `download_all_node_models`。

**实例复用**：`find_reusable_instance` 按 GPU 型号匹配（大小写不敏感 contains），只接受可进入 Running 的状态（Created/Stopped/Starting/Running）；列实例失败退避重试 3 次、全失败报错终止部署（宁可让用户稍后重试，不能静默多开机器）。

### 9.3 模型下载

- 两条节点路径共用 `download_all_node_models`（下载清单只定义一次）；主模型在前、辅助模型（mmproj/MTP）按模板顺序
- **aria2c 优先**（-x16 -s16 多连接多分片续传），失败回退 `curl -fL -C - --retry 5`
- **`.download.ok` 权威标记**：只有下载器 0 退出才写入（内容 = 真实文件大小）；期望字节数只是参考值（计划元数据与实际模板文件可能不是同一个）
- 完整性三道证据：`.download.ok` + 文件非空 + GGUF 魔数；终检只看远端真实文件（任一缺失/为空禁止进入加载）
- 进度上报：每 5% bucket 写一次阶段输出（trail）；无总大小时按 64 MiB 粒度报增量
- 重试模型：单轮 5 次 × 3 轮 + 停滞上限 3 次（跨续传比较无增长立即失败，不再烧完轮次预算）；总 deadline 3600s；源站轮询 3 轮（hf-mirror ↔ huggingface 镜像替换）
- 远端下载元数据预取：HTTP Range 取文件头 8MiB 解析 GGUF（截断错误翻倍重试至 64MiB）

### 9.4 停止与生命周期

**`stop`（销毁台账 + 释放实例）**：

```
stop 部署
  ├─ 只能关闭 Ready 状态部署
  ├─ 台账检查 + 远端端口探测（engine_ports_busy 8080/8081，ss → netstat → /dev/tcp 三级）
  │    ├─ 端口全空闲 → teardown（关隧道 + 关机保留，磁盘/模型文件保留）
  │    └─ 任一端口被占 → 只按模型名停本部署进程，实例保留（note 注明）
  └─ 台账置 stopped
```

- **关实例判据 = 引擎端口占用制**（用户策略）：8080/8081 都空闲即无人推理，直接关机；台账其他部署记录仅提示不阻止（其进程没跑，模型文件随磁盘保留，下次 deploy 自动开机复用）
- **按模型名停止**（`stop_llama_server_by_model`）：`--model <path>` 是进程命令行最稳定锚点（端口会漂移、pid 文件会陈旧）；流程 = TERM（等 5s 优雅退出）→ KILL → **等 1s** → 按模型名复查 → 未删净重试，共 2 轮；两轮仍残留报错附 PID 列表（绝不假报成功）
- 实例未运行 → SSH 探测快速返回（无进程可查），不做无谓连接

**`stop_service`（停服务保留实例）**：停进程 + 关机 + 清台账 node_ip（防开机漂移残留）；自建设备只停进程绝不关机。

**`unload`（卸载只停不删）**：先置台账 stopped（消除 runprobe 秒级竞态误降 failed）→ 按模型名停进程 → 云厂商等待端口空闲确认（最多 10s）→ 共享节点只关自己隧道 → nvidia-smi 检查其他 GPU 进程无则关机（查询失败保守不关）。

**`restore_deployment_tunnels`（网关启动自恢复）**：厂商 stopped + 台账 ready/failed → 删台账；厂商 running + 台账 ready/failed → 恢复隧道 + 起进程 + 等 /health → 置 ready；不修改台账 local_port（CLI 分配的固定端口）。

**`abort_deployment`（取消）**：实例只停不删；显式释放端口预留（否则反复「部署→停止」吃光端口区间）。

### 9.5 状态机

- `DeploymentStatus`：Planned → Provisioning → Ready → Stopped → Destroyed（+ Failed）；`transition` 白名单迁移，非法迁移报 `InvalidTransition`；**Failed → Ready 专门放行**（探测自愈：瞬时 /health 503 翻的 Failed 在端口恢复后必须能翻回）
- `RunStatus`：Active/Unactive，只由后台探测线程更新（`update_run_status`），**不修改 DeploymentStatus**（避免后台线程误降级）
- 三个监控线程（gateway crate，任何网络调用不得在网关锁内执行）：
  - **runprobe**：5 秒一轮探测本地回环 /health（400ms 超时），更新 run_status；连续 3 轮失败 + 60s 冷却 → 隧道自愈重建
  - **sshmon**：20 秒一轮刷新 SSH 登录信息缓存（持锁只读台账 → 释放锁调厂商）
  - **cloudmon**：5 秒一轮采集实例监控指标（失败保留上次快照但暴露 error）

### 9.6 失败清理

`park_failed_instance`：
- **连接类失败（非认证）** → 清理本部署隧道 + **删台账**（用户重新 deploy 从零开始；远端已无现场可复用）
- **认证失败（Permission denied）** → 保留台账（机器活着，修好密钥即可重试）
- 其余失败 → 停进程 + 关隧道 + 关机保留（不销毁）

云资源回滚不变量（provider 层）：`provision` 返回 Err 时云端不应残留本次创建的实例；回滚失败必须把残留实例 ID 写进错误消息。

---

## 10. 统一协议适配层

### 10.1 HTTP 服务的实现取舍

用标准库手写 HTTP/1.1（不引 axum/hyper）。**8 个固定 worker 线程池**（非每连接一线程——监控页 1.5s 轮询会积累上百线程拖垮进程）；每连接读超时 120s / 写超时 300s；`Connection: close` 短连接；`MAX_BODY = 32MiB`；内置 base64 解码（服务浏览器附件上传）；路径 percent-decode（模型 ID 的 `:` 被浏览器编成 `%3A`）。

代价：不支持 HTTP/2、TLS——对「仅监听 127.0.0.1 的个人客户端」都不必要。

### 10.2 路由清单（54 条，按类别）

| 类别 | 路由 |
|------|------|
| 内置页面 | `GET /`、`GET /index.html`（内嵌 WebUI，同源可调 /api 与 /v1，无需 CORS） |
| OpenAI 兼容 | `GET /health`、`GET /v1/models`、`POST /v1/chat/completions`、`POST /v1/completions`、`POST /v1/embeddings` |
| Anthropic 兼容 | `POST /v1/messages`（系统提示插入 messages 开头；流式 chunk → content_block_delta / message_delta / message_stop 事件） |
| 部署管理 | `GET /api/status`、`POST /api/deploy`、`GET /api/deployments`、`POST /api/assess`、`POST /api/preview`、`GET /api/sysinfo`、`GET/PUT /api/settings` |
| 模型模板 | `GET /api/model-templates`、`PUT /api/model-templates`、`DELETE /api/model-templates/{model}/{slot}` |
| 设备 | `GET/POST /api/devices`、`POST /api/devices/{id}/test`、`DELETE /api/devices/{id}` |
| 云实例 | `GET /api/cloud/instances`、`POST .../power-off`、`POST .../power-on`、`DELETE .../{provider}/{id}` |
| 模型下载任务 | `POST /api/models/pull`、`GET /api/models`、`GET /api/models/jobs`、`POST /api/models/jobs/{id}/cancel`、`DELETE /api/models/{id}` 等 |
| 会话 | `GET/POST /api/sessions`、`POST /api/sessions/{id}/messages`、`POST /api/sessions/{id}/operations`、`DELETE/GET /api/sessions/{id}` |
| 部署生命周期 | `POST /api/deployments/{id}/stop-service|start-service|unload|restore-tunnel`、`DELETE /api/deployments/{id}`、`GET /api/deployments/{id}/ssh-info` |
| 附件/联网 | `POST /api/extract`、`POST /api/fetch-url` |

`/api/` 或 `/v1/` 前缀下方法不匹配 → 405；其余 404。

### 10.3 请求规范化

**只改 `model` 字段**，其余原样保留（OpenAI 兼容性的关键）：`model="auto"`/空 → 当前部署模型 ID；具体名 → 原样透传；未知字段 → 透传；采样参数越界 → 400 指明参数名。

**响应改写**：上游 llama.cpp 填绝对路径，必须改写为 Mangosteen 模型 ID（防泄漏本机路径）；只改 `model` 一个键、保留 `timings`/`system_fingerprint` 等非标准字段。

### 10.4 错误归一化

上游错误统一转 Mangosteen 格式 `{error: {message, type, code}}`：

| 错误 | 状态码 |
|------|--------|
| `BadRequest` / `ModelNotFound` | 400（model_not_found → 404） |
| `AccessDenied` | 403 |
| `InsufficientLocalResource` / `NoViableCandidate` | 503 |
| 其它 | 500 |

### 10.5 流式 / 非流式分路径

流式（POST + 推理路由 + `stream:true`）走 `relay_stream` 直通：系统 `curl --no-buffer` 上游、先探第一段区分正常 SSE 与上游报错 JSON（不能一上来写 200，否则错误被当流解析、界面空白）、SSE 帧逐行改写 model 后 `write_chunk` 立即 flush、`TCP_NODELAY` 防 Nagle 攒包、客户端断开即 `child.kill()`。非流式走 `http_post_json` 裸 TcpStream（上游必为本机回环，无需 TLS）。

### 10.6 为什么网关不自己实现推理

网关只做协议转换与路由：本地与云端走同一份转发代码；上游升级 llama.cpp 时网关无需改动；网关不可用时用户仍可直连后端排障。

---

## 11. 会话管理

### 11.1 定位

第一层职责：「管理用户会话、部署任务会话、推理会话生命周期；维护任务状态、上下文、操作记录」。关键边界：**不感知算力来源**——不判断能不能部署、不启动引擎、不访问云厂商。

### 11.2 会话种类

| 种类 | 内容 | 生命周期 |
|------|------|----------|
| `Chat`（推理会话） | `messages: [{role, content, at_ms}]` | 新建 → 追加消息 → 删除 |
| `Deploy`（部署任务会话） | `operations: [{action, detail, ok, at_ms}]` | 新建 → 记录操作 → 删除 |

部署也要「会话」：一次部署是「评估 → 选点 → 下载 → 加载 → 就绪」的过程，记在同一条时间线上，事后才能回答「上次为什么上了云」。

### 11.3 持久化与接口

单文件 JSON（`sessions.json`，config_dir 下）+ 原子写，跨进程可见（CLI 与 `ui` 共享同一份）。`prune(keep)` 防文件无限增长。接口：`GET/POST /api/sessions`、`GET/DELETE /api/sessions/{id}`、`POST /api/sessions/{id}/messages`、`POST /api/sessions/{id}/operations`。

### 11.4 与推理链路的关系

会话层**不参与**推理转发（消息落库由调用方拿到完整回复后另行调用）——流式响应下「什么时候算一条完整回复」只有调用方知道。

---

## 12. 基础能力层

### 12.1 模型台账

JSON 存储。`register_local_with_source`：访问控制校验 → 解析 GGUF → 可选 SHA-256 → 写入台账；同 ID 重复注册**更新**（幂等），未提供来源不抹历史来源。`register_cloud`：不校验本地文件，location 记 `Cloud{instance_id, path}`。

**哈希默认在登记时不算**（7B 文件机械盘数十秒），改为 `verify` 时按需计算。`remove(id, delete_file)` 删磁盘文件前再走访问控制。

### 12.2 部署台账

`state.json`（config_dir），`DeploymentRecord{plan, status, run_status, pid, error, updated_at, config_template}`。**每次状态变更立即落盘**（原子替换）——一次云端部署可能持续数小时并产生真实计费，进程重启丢失台账 = 忘记关机持续扣费 + 资源泄漏。`upsert`（同 ID 覆盖）、`prune_destroyed/prune_terminal`（清 destroyed+failed，保留 stopped 复用依据）、`prune_inactive`（部署前清非活跃，避免复用不可达旧实例）。

### 12.3 文件访问控制（AccessGuard）

威胁模型：误读系统文件、符号链接逃逸、`../../` 穿越、超大文件。防御：`fs::canonicalize` 后前缀比较（符号链接无处遁形）+ 词法归约兜底（目标不存在时仍能拦住 `..`）+ 体积上限 + `.gguf` 扩展名白名单。授权范围 = `models_dir ∪ cache_dir ∪ allowed_read_roots`。

### 12.4 下载与格式

`download_model`：`.part` 断点续传（`--continue-at -`）、多源择优（hf-mirror/huggingface HEAD 测速排序）、大小/sha256 校验后原子 rename、进度轮询 250ms、取消经 `try_wait + kill`。`detect_format` 识别 gguf/safetensors/onnx/pytorch/tensorrt/config/ggml（**只标识不过滤**——下载不限格式）。

### 12.5 其他能力

`quantize`（可选工具，非部署主链路；输出不得覆盖源文件）、`extract`（文本直读 + 系统工具富文档，单文件 200k 字符上限）、`webfetch`（系统 curl + 状态机去标签，零密钥零依赖）。

---

## 13. 平台适配层（本地三平台：macOS / Windows / Linux）

### 13.1 路径规范

| 用途 | macOS | Linux | Windows |
|------|-------|-------|---------|
| 配置 | `~/Library/Application Support/mangosteen` | `$XDG_CONFIG_HOME/mangosteen` | `%APPDATA%\mangosteen` |
| 模型 | `.../mangosteen/models` | `$XDG_DATA_HOME/mangosteen/models` | `%LOCALAPPDATA%\mangosteen\models` |
| 缓存 | `~/Library/Caches/mangosteen` | `$XDG_CACHE_HOME/mangosteen` | `%LOCALAPPDATA%\mangosteen\cache` |
| 日志 | `~/Library/Logs/mangosteen` | `$XDG_STATE_HOME/mangosteen/log` | `%LOCALAPPDATA%\mangosteen\logs` |

每个都支持 `MANGOSTEEN_*` 环境变量覆盖（容器化与测试）。不用 `dirs` crate（手写 <200 行可完全单测）。

### 13.2 硬件采集

| 平台 | GPU | 内存 | CPU |
|------|-----|------|-----|
| macOS | `system_profiler SPDisplaysDataType -json`；Apple Silicon 统一内存 | `sysctl` + `vm_stat` | `sysctl hw.physicalcpu/logicalcpu` |
| Linux | `nvidia-smi`；AMD `rocm-smi` | `/proc/meminfo` | `/proc/cpuinfo`（按 physical id + core id 去重） |
| Windows | `nvidia-smi`；否则 PowerShell WMI | `Win32_OperatingSystem` | `Win32_Processor` |

降级策略：采集失败**绝不**退出，每项退回保守默认（8GiB/4 核/无 GPU）并记入 `warnings`。

### 13.3 进程管理

| 差异点 | Unix | Windows |
|--------|------|---------|
| 温和终止 | SIGTERM | `taskkill /PID`（不带 /F = WM_CLOSE） |
| 强杀 | SIGKILL | `taskkill /PID /F` |
| 进程组 | `process_group(0)` | `CREATE_NEW_PROCESS_GROUP \| CREATE_NO_WINDOW` |
| 僵尸判定 | `ps -o stat=` 首字母 Z | 无此概念 |

**SIGKILL 后的判定**：若 `kill(pid,0)` 仍成功，只可能是僵尸态或内核尚未调度终止——「倾向于认为已终止」比「报错」更安全（误报失败会让调用方重复清理甚至放弃清理）。日志重定向优先于管道（管道写满 64KB 会阻塞子进程——「引擎启动后卡死」根因）。

### 13.4 文档转换（docconv）

macOS `textutil` / Linux `pdftotext + pandoc` / Windows PowerShell + Word COM；缺工具时给可执行安装建议，不静默失败。

### 13.5 资源监控（resmon）

`CpuSampler` 有状态（存上次累计时间片算增量）；`process_rss_bytes` 供设备页每部署内存占用（查不到返回 None 不算错误）。

---

## 14. 配置体系

### 14.1 搜索顺序

1. `--config <path>`（**必须存在**，否则报错——静默回退会让人误以为配置生效）
2. `MANGOSTEEN_CONFIG` 环境变量
3. 用户配置目录 `config.toml`
4. 系统级目录（`/etc/mangosteen`，Windows 无）
5. 全部缺失 → `Config::sample()` 内置默认（首次运行场景）

TOML 语法损坏 → 重命名为 `config.toml.corrupt-<时间戳>` 并警告，回落内置样例继续；语义错误（如 max_model_bytes=0）原样上报。

### 14.2 密钥表达

`SecretRef` 四种来源：Inline / Env / File / None，按顺序解析；`config show` 只输出脱敏形式（`${ENV}` / `file:...` / `<inline:***redacted***>`）；配置文件 0600。

### 14.3 向前兼容

所有字段 `serde(default)`，旧版配置在新版本下可正常加载，缺失项取默认值。

### 14.4 配置模板与模型来源

- **配置模板**（`core::template`）：手写 YAML 子集解析（带行号错误）；`BUILTIN_TEMPLATES` 是首次启动灌入用户配置的唯一数据源（只补缺失不覆盖用户已改）；`resolve(model, text)` 未注册模型明确报错不猜默认值
- **模型来源**（`ModelSource`）：全字段可配（配置驱动）；内置三源 modelscope（默认）/ hf-mirror（国内直连可搜索）/ huggingface；`download_url` 按 kind 拼装，Custom 缺模板报 config 错误

---

## 15. 错误处理与降级

### 15.1 错误分类与可重试性

`MnError` 20 个变体；`is_retryable()` 是调度决策器判断是否降级的依据：

| 可重试 | 不可重试 |
|--------|---------|
| InsufficientLocalResource / EngineLaunch / EngineExited / Provider / NoViableCandidate / Network | Config / ConfigIo / ModelNotFound / GgufFormat / HashMismatch / DriverUnavailable / AccessDenied / BadRequest / Cancelled / CloudDownloadFailed / InvalidTransition / Io |

`code()` 提供稳定错误码（17 个）；`exit_code()` 用 BSD sysexits（64 用法错 / 66 无输入 / 77 无权限 / 78 配置错 / 69 不可用 / 70 软件错）。

### 15.2 云端编排的资源回滚

**核心不变量**：`provision()` 返回 `Err` 时，云端不应残留任何由本次调用创建的资源。失败处理原则：**停实例不销毁**（权重/引擎/环境是重新部署最贵的部分；关机已能停止计费）；回滚失败必须把**残留实例 ID + 手工清理命令**写进错误消息。

### 15.3 错误消息的三段式

```
✗ 现象（发生了什么）
  上下文（为什么：资源不足/网络/厂商限流）
  建议： 下一步该做什么（可执行命令）
```

`output::suggest()` 为每种错误类型提供具体建议，不留「未知错误」。

---

## 16. 安全设计

| 面 | 措施 |
|----|------|
| 密钥存储 | env/file/inline 引用；配置文件 0600；`config show` 脱敏 |
| 认证头传递 | 0600 临时配置文件传给 curl（`--config`），不进进程参数列表 |
| 推理服务暴露 | 本地绑 127.0.0.1；云端绑 0.0.0.0 但只经 SSH 隧道访问（浏览器直接开 `http://127.0.0.1:<本地端口>/v1`） |
| 网关鉴权 | 可选 Bearer / `X-API-Key`；开启时无密钥 401 |
| 文件访问 | 白名单 + 真实路径解析 + 体积上限 + 扩展名限制 |
| 路径穿越 | 词法归约 + 符号链接解析（防 TOCTOU） |
| 云端实例清理 | 失败必回滚；回滚失败必须暴露残留 ID |
| 网络重定向 | curl `--max-redirs 0`（防认证头被带到别的域） |
| 危险操作确认 | 删除模型/销毁实例/批量停止要求 `--yes`，未加返回退出码 2 |
| 隧道进程清理 | `TunnelManager::Drop` 兜底 `close_all()`，ssh 不成孤儿 |

---

## 17. 测试策略

### 17.1 分层测试（当前规模）

| crate | 单元测试 | 备注 |
|-------|---------|------|
| `core` | 185 | 纯算法（无 IO） |
| `platform` | 36 | 含真实 syscall |
| `base` | 47 | 临时目录 |
| `device` | 33 + 1 ignored | 假引擎脚本 |
| `provider` | 293 | MockTransport / FakeDriver |
| `orchestration` | 63 | 不变量 I1–I4 |
| `gateway` | 单测 + e2e 43 | 真 GGUF + 假 llama-server + 真 TCP |
| `cli` | 47 + layering 24 + release_assets 20 | 架构守护 + 交付件守护 |

### 17.2 关键测试设计

- **资源估算边界**：7B Q4 适配 8GB/24GB 卡；32B 在 8GB 卡必然部分卸载；KV 降档救「差一点」；无 GPU 时 CPU 回退 vs 禁止回退
- **状态机完备性**：合法路径全通；终态不可复活；不跳过 Starting 直接 Running；Failed → Ready 探测自愈
- **降级链不变量**：I1–I4 各有用例；本地失败错误信息含「本地部署失败」
- **端到端**（`gateway/tests/e2e.rs`，43 项）：真实 GGUF + 假 llama-server（Python 实现 /health 与 /v1/chat/completions）+ 真实 TCP——登记 → 评估 → 部署 → 就绪 → 停止全链路、OpenAI 兼容接口、`model:"auto"` 替换、API Key 鉴权、跨进程台账持久化
- **为什么用假引擎**：llama-server 需要 GB 级模型与 GPU，CI 没有；假引擎能验证「网关 → 引擎进程 → HTTP 转发 → 响应归一化」这条最易出错的路径

### 17.3 交付件守护（release_assets.rs，20 项）

DESIGN.md 必须覆盖 8 个 crate 名与 7 个主题词（资源评估引擎/调度决策器/多云统一抽象/后端节点选择器/统一协议适配层/文件访问控制/架构约束的自动化守护）；CI 必须跑架构约束测试；WebUI 保持自包含（禁止 CDN）。

---

## 18. 架构约束的自动化守护

`cli/tests/layering.rs`（1,031 行、24 个用例）把架构铁律变成可执行断言，扫描前用 `strip_comments_and_strings()` 剥离注释与字符串（本仓库大量注释在解释「为什么不用某技术栈」，直接扫原文会误伤），并有元测试验证剥离器可靠。

| 编号 | 约束 | 校验方式 |
|------|------|---------|
| C1 | 网关唯一收口 | CLI 的 Cargo.toml 不得依赖 device/provider；源码不得 `use mangosteen_device::/mangosteen_provider::`；不得出现厂商 API 地址；不得自建 TCP 连接 |
| C2 | 平台无关 | core/gateway/base 源码不得出现 `cfg(target_os)`；core 不得依赖 platform/libc/windows-sys；平台分支只允许在 platform 与 cli |
| C3 | 推理引擎插件化 | orchestration/gateway 源码不得出现 `InferenceEngine::`（settings handler 例外）；必须出现 `engines_registry.get(`；注册表恰好 2 个内置插件（llama.cpp + Ollama），未实现引擎刻意缺席 |
| C4 | 本地优先配置 | `decide()` 中本地拒绝判定必须早于云端方案构造；默认配置必须含 `local_first: true` |
| C5 | 依赖方向自上而下 | 逐 crate 比对 Cargo.toml 依赖白名单 |
| C6 | 五层架构完整性 | session/cli/webui、gateway http/*、device/plugin + orchestration 7 个执行文件 + provider/registry、base 下载/量化、core/platform 全部在位；session 不感知算力 |
| — | 其他 | 生产代码 unwrap 密度 < 3/kloc；多格式「识别不过滤」；模型登记仍要求 GGUF 元数据 |

接入点：`.github/workflows/release.yml` 在 `cargo test --workspace` 后单独再跑 layering；`scripts/refactor-check.sh` 五级校验（fmt → clippy → test → layering → e2e）。

---

## 附：关键设计决策速查

| 决策 | 选择 | 理由 |
|------|------|------|
| 推理引擎 | 插件化（llama.cpp + Ollama） | 本地/云端参数语义一致；新增引擎不改调度主链路 |
| 模型格式 | 下载不限格式，只标识 | 非 GGUF 是别的引擎要用的 |
| 本地推理部署 | 不支持（target=local 拒绝） | 本地推理交由 Ollama；静默上云会产生计费实例 |
| 云端 HTTP 客户端 | 系统 curl | 低频操作；无新增供应链；自带 TLS |
| 网关 HTTP 服务 | 标准库手写（8 worker 线程池） | 回环监听；HTTP/2 与 TLS 不必要；每连接一线程会被轮询拖垮 |
| 台账存储 | JSON + 原子写 | 几十条记录；用户可手改排障 |
| 登记时算哈希 | 默认不算 | GB 级文件耗时；verify 时按需 |
| 关实例判据 | 引擎端口占用制（8080/8081） | 端口空闲即无人推理；台账其他部署仅提示 |
| 停止进程判据 | 按模型名（非端口） | 端口漂移、pid 陈旧；模型文件名是命令行最稳定锚点 |
| SSH 重试 | 断链才重试，最多 2 次；不可达快速失败 | 不可达每次烧满 ConnectTimeout，把已关机节点拖成假死 |
| 失败清理 | 连接类失败删台账；其余停实例不销毁 | 远端无现场可复用；权重/环境是重部署最贵部分 |
| 并发模型 | 线程而非 async | 推理是一次性生命周期管理，无高并发需求 |
| 云端监控采集 | 后台线程 + 5 秒缓存 | 设备页 2 秒轮询；同步调厂商会卡页面并触发限流 |
| 远端节点 OS | 仅 Linux | 镜像强制 Linux；Windows 镜像既不能 SSH 也不能跑推理 |
| 实例密码 | 固定 `1qaz@WSX` | 公钥安装后的统一回退凭证；异步生效需验证重试 |
