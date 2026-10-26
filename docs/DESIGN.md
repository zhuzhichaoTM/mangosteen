# Mangosteen 系统设计文档

> 本文是 Mangosteen（山竹）的技术基线。所有实现必须与此对齐；  
> 需要偏离时必须先改本文，再改代码。

## 目录

1. [设计目标与边界](#1-设计目标与边界)
2. [分层架构](#2-分层架构)
3. [核心数据结构](#3-核心数据结构)
4. [资源评估引擎](#4-资源评估引擎)
5. [调度决策器](#5-调度决策器)
6. [多云统一抽象](#6-多云统一抽象)
7. [后端节点选择器](#7-后端节点选择器)
8. [本地推理链路](#8-本地推理链路)
9. [云端网络工具集](#9-云端网络工具集)
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

| 目标          | 衡量方式                            |
| ----------- | ------------------------------- |
| 用户无需理解算力来源  | 上层只面对一个 OpenAI 兼容 base URL      |
| 本地能用就绝不上云   | 云端方案必带非空降级轨迹；本地可行时首选本地          |
| 本地与云端行为一致   | 同一套 llama.cpp 参数语义；同一份资源估算公式    |
| 新增云厂商不改上层代码 | 配置化驱动；`ProviderDriver` 接口稳定     |
| 三平台原生可用     | 所有平台差异收敛到 `mangosteen-platform` |

### 明确不做

- 集群、多租户、分布式推理
- Agent 框架

理由：这些能力会显著增加复杂度，却不服务于"个人开发者跑模型"这个单一场景。

### 已调整的边界（记录变更，避免照旧基线改回去）

| 原边界 | 现基线 | 调整原因 |
|--------|--------|----------|
| 不内置对话界面 | UI 提供完整对话窗口 | 用户要求「部署 + 使用一体」：部署完就在同一个客户端里对话，不用另开工具 |
| 推理引擎唯一 llama.cpp | 可配置多引擎，默认 llama.cpp | 用户已有不同栈（llama.cpp / Ollama / vLLM…），强制单一引擎会把人挡在门外 |
| 模型格式只用 GGUF | 下载不限格式，只标识类型 | 非 GGUF 是别的引擎要用的；替他决定「只能下 GGUF」是错的 |

三条调整都**不改变**本地优先与网关唯一收口这两条核心原则。

---

## 2. 分层架构

### 2.1 五层

严格对齐基线的自上而下链路：**用户会话管理 → API Server → 引擎管理 →
模型评估 → 节点选择 → 节点配置 → 模型下载 → 加载执行**。

```
┌─ 第一层  用户会话与交互层 ─────────────────────────────────┐
│  mangosteen-session   用户会话管理                          │
│      · 推理会话（对话上下文）                                │
│      · 部署任务会话（状态 + 操作记录）                       │
│  交互入口：mangosteen-cli（CLI/TUI）                         │
│            mangosteen-gateway::webui（内嵌 Web 控制台）      │
└──────────────────────┬────────────────────────────────────┘
                       │ 只与 API Server 通信
┌──────────────────────▼────────────────────────────────────┐
│ 第二层  API 网关层                                          │
│  mangosteen-gateway::server   唯一收口                      │
│      · [OI] 兼容接口（/v1/*）                               │
│      · 部署管理接口（/api/*）                               │
│      · 鉴权、协议解析、请求路由、状态回传                    │
└──────────────────────┬────────────────────────────────────┘
                       │
┌──────────────────────▼────────────────────────────────────┐
│ 第三层  编排调度核心层                                       │
│  ① 引擎管理    mangosteen-local::plugin   （插件式多引擎）   │
│  ② 模型评估    mangosteen-gateway::assess                   │
│  ③ 节点选择    mangosteen-gateway::scheduler（本地优先打分） │
│  ④ 节点配置    mangosteen-core::plan      （本地/云端两套）   │
│  ⑤ 多云适配    mangosteen-cloud + mangosteen-backend        │
└──────────────────────┬────────────────────────────────────┘
                       │
┌──────────────────────▼────────────────────────────────────┐
│ 第四层  资产与执行层                                         │
│  ① 模型下载    mangosteen-base::download                    │
│  ② 模型量化    mangosteen-base::quantize  （可选，非主链路）  │
│  ③ 加载执行    mangosteen-local::engine                     │
└──────────────────────┬────────────────────────────────────┘
                       │
┌──────────────────────▼────────────────────────────────────┐
│ 第五层  基础支撑层                                           │
│  core（领域类型 / 配置结构 / 错误）                          │
│  base（台账持久化 / 配置存储 / 文件访问控制）                 │
│  platform（平台适配：路径 / 进程 / 硬件 / 动态库）            │
└───────────────────────────────────────────────────────────┘
```

### 2.2 crate 与层的对应关系

层是**逻辑划分**，crate 是**编译单元**。两者不是一一对应，原因写在下面。

| 层 | crate | 说明 |
|----|-------|------|
| 一 | `mangosteen-session` | 会话领域模型 + 持久化 |
| 一 | `mangosteen-cli` | 交互入口（参数解析、TUI） |
| 一 | `mangosteen-gateway::webui` | 内嵌 Web 控制台（单文件 HTML） |
| 二 | `mangosteen-gateway::server` | HTTP 服务与路由 |
| 三 | `mangosteen-gateway::assess` | 模型评估 |
| 三 | `mangosteen-gateway::scheduler` | 节点选择 |
| 三 | `mangosteen-local::plugin` | 引擎管理（插件注册表） |
| 三 | `mangosteen-core::plan` | 节点配置（本地 / 云端） |
| 三 | `mangosteen-cloud` / `mangosteen-backend` | 多云厂商适配 |
| 四 | `mangosteen-base::download` / `quantize` | 资产 |
| 四 | `mangosteen-local::engine` | 执行 |
| 五 | `mangosteen-core` / `base` / `platform` | 支撑 |

**为什么 `gateway` 同时含第二层与第三层**：模型评估与节点选择是
「编排核心」而非「协议转换」，逻辑上属第三层。但它们的输入输出
（`Evaluated` / `Decision` / `DeploymentPreview`）与网关的编排入口
耦合极紧，拆成独立 crate 会产生大量双向类型搬运，收益不抵风险。
因此用**模块边界**区分层次，而不是物理拆分。

这不影响约束的可执行性：`layering.rs` 的 C3 按**文件**扫描主链路
（`gateway.rs` / `scheduler.rs` / `assess.rs`），与 crate 边界无关。

### 2.3 依赖方向

严格自上而下。`core` 是最底层，不依赖任何自家 crate。

```
core ←── platform ←── base ←── session
  ↑         ↑         ↑    ←── local
  │         │         │    ←── backend
  │         │         │    ←── cloud（可依赖 backend）
  └─────────┴─────────┴─────── gateway（+ session）
                              cli（+ gateway + session）
```

**为什么 `local` / `cloud` 不依赖 `base`**：它们只负责"把方案变成运行中的
服务"，不需要台账/配置能力。台账由网关层调用 `base` 完成。这样
`local` / `cloud` 可以脱离存储独立测试。

**为什么 `session` 只依赖 `core` + `platform`**：会话层是第一层的状态载体，
它**不感知算力来源**——不判断能不能部署、不启动引擎、不访问云厂商。
这条由 `c6_session_layer_does_not_know_about_compute` 守护。

**为什么 `backend` 只能被 `cloud` 依赖**：厂商驱动必须经驱动注册表统一加载，
否则会绕过"配置驱动 + 统一能力声明"的约定。网关与 CLI 直接依赖 `backend`
会被 `c5_backend_is_only_reachable_through_cloud_layer` 拦下。

### 2.4 各层职责与禁止事项

| 层 | crate | 可以做 | 不可以做 |
|----|-------|--------|----------|
| 一·会话 | `session` | 会话 CRUD、消息追加、操作记录、持久化 | 判断可部署性、启动引擎、访问云 API |
| 一·交互 | `cli` / `webui` | 解析参数、格式化输出、调用 `/api` 与 `/v1` | 直连引擎、直连云厂商 API |
| 二 | `gateway::server` | 鉴权、路由、协议转换、状态回传 | 写平台专属代码、自己实现推理 |
| 三·评估 | `gateway::assess` | 采集硬件、估算显存/内存、判定可行性 | 决定用本地还是云端 |
| 三·选点 | `gateway::scheduler` | 本地优先打分、云端候选排序 | 拉进程、调厂商 API |
| 三·配置 | `core::plan` | 生成本地参数 / 云端规格与隧道配置 | 执行部署 |
| 三·引擎 | `local::plugin` | 引擎插件注册与分发 | 决定是否该用本地 |
| 三·多云 | `cloud` / `backend` | 实例 CRUD、隧道、端口映射、存储映射、签名 | 决定选哪家、被 CLI 直连 |
| 四·资产 | `base::download` / `quantize` | 下载、校验、缓存、量化 | 决定部署策略 |
| 四·执行 | `local::engine` | 拉起进程、等待就绪、探活、终止 | 暴露 HTTP 给上层 |
| 五 | `core` / `base` / `platform` | 类型、纯算法、台账、配置、系统调用 | 含业务调度逻辑 |

---

## 3. 核心数据结构

### 3.1 模型规格 `ModelSpec`

资源评估的输入。**只描述模型自身，不含任何部署决策。**

```rust
pub struct ModelSpec {
    pub name: String,          // "qwen2.5-7b-instruct"
    pub params_b: f64,         // 7.615
    pub quant: Quantization,   // Q4KM
    pub n_layers: u32,         // 28
    pub n_heads: u32,          // 28
    pub n_kv_heads: u32,       // 4   ← GQA 的关键
    pub embedding_dim: u32,    // 3584
    pub vocab_size: u32,       // 152064
    pub native_context: u32,   // 32768
    pub file_size_bytes: u64,  // 4683073024
}
```

**为什么这些字段**：KV Cache 估算需要 `n_layers × n_kv_heads × head_dim`；  
`head_dim = embedding_dim / n_heads`。少了任何一个，估算误差会到一个数量级。

**来源优先级**：GGUF 文件头解析（首选）> 台账已有记录 > 用户显式指定。

### 3.2 硬件画像 `HardwareProfile`

```rust
pub struct HardwareProfile {
    pub os: String,                       // "macos"
    pub arch: String,                     // "aarch64"
    pub host: HostProfile,                // 内存 + CPU
    pub accelerators: Vec<Accelerator>,   // GPU（可多卡）
}

pub struct Accelerator {
    pub name: String,
    pub vendor: AcceleratorVendor,        // Nvidia / Apple / Amd / Intel / None
    pub vram_bytes: u64,                  // 独显显存
    pub unified_memory_bytes: u64,        // Apple Silicon 统一内存
    pub compute_capability: Option<String>, // "sm_89"
    pub fp16_tflops: Option<f32>,
    pub memory_bandwidth_gbps: Option<f32>,
}
```

**关键派生规则**：

| 量    | 计算                              | 依据                                   |
| ---- | ------------------------------- | ------------------------------------ |
| 可用显存 | 独显取 `vram_bytes`；统一内存取 `× 0.70` | macOS `recommendedMaxWorkingSetSize` |
| 可用内存 | `total - max(total×0.25, 1GiB)` | llama.cpp 内存不足会 swap，吞吐塌一个数量级        |
| 建议线程 | `physical_cores`（不是逻辑核）         | 超线程在 llama.cpp 线程池里争用，吞吐反而下降         |

### 3.3 部署方案 `DeploymentPlan`

网关内部的"唯一真相"，也是台账持久化实体。

```rust
pub struct DeploymentPlan {
    pub id: String,
    pub model: ModelSpec,
    pub origin: ComputeOrigin,           // Local | Cloud{provider, instance_id}
    pub local: Option<LocalPlan>,        // 本地方案
    pub cloud: Option<CloudPlan>,        // 云端方案
    pub assessment: Assessment,          // 评估快照（审计）
    pub created_at: i64,
    pub fallback_trail: Vec<String>,     // ["local:...", "cloud:..."]
}
```

**不变量**：`origin == Cloud` 时 `fallback_trail` 必须非空。  
由 `DeploymentPlan::respects_local_first()` 提供运行时检查，  
`assemble_plan()` 在轨迹为空时自动补 `"cloud:unspecified"` 兜底。

`LocalPlan` 字段与 llama.cpp 参数一一对应：

| 字段                  | llama-server 参数                     |
| ------------------- | ----------------------------------- |
| `model_path`        | `--model`                           |
| `context_size`      | `--ctx-size`                        |
| `threads`           | `--threads`                         |
| `batch_size`        | `--batch-size`                      |
| `gpu_layers`        | `--n-gpu-layers`                    |
| `kv_cache_type_k/v` | `--cache-type-k` / `--cache-type-v` |
| `flash_attention`   | `--flash-attn`                      |
| `use_mmap`          | `--mmap`                            |
| `use_mlock`         | `--mlock`                           |
| `host` / `port`     | `--host` / `--port`                 |

`CloudPlan.engine_args()` 强制 `host = 0.0.0.0`（实例内需监听网卡，由 SSH 隧道  
暴露到本机 `127.0.0.1`），其余参数语义与本地**完全相同**。

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

状态机迁移由 `InstanceState::can_transition_to()` 集中定义，  
**驱动实现不得自行发明状态**。刻意保守：`Created` 不能直接跳 `Running`  
（必须先经 `Starting`），保证"探测就绪"这一步不被跳过。

---

## 4. 资源评估引擎

### 4.1 估算公式

```
总需求 = weights + kv_cache + compute_buffer + runtime_overhead
```

| 项                | 公式                                                                       | 说明                            |
| ---------------- | ------------------------------------------------------------------------ | ----------------------------- |
| weights          | `file_size_bytes`（有实测）否则 `params × bpw / 8`                              | 实测优先，因为 GGUF 含元数据与对齐          |
| kv_cache         | `2 × n_layers × n_kv_heads × (embed_dim/n_heads) × ctx × kv_type_factor` | f16=1.0, q8_0≈0.53, q4_0≈0.28 |
| compute_buffer   | `batch × embed_dim × 8 × 2 / 4`，夹在 `[192MiB, 4GiB]`                      | 保守值（未启用 FlashAttention 的量级）   |
| runtime_overhead | NVIDIA/AMD 400MiB；Intel 320MiB；Apple 250MiB；CPU 64MiB                    | 实测经验值                         |

### 4.2 落位形态判定顺序

```
1. 全量 GPU 卸载（gpu_layers = n_layers + 1）    ← 最优
2. 部分卸载（gpu_layers 由预算反算）             ← 吞吐下降 40%~70%
3. 纯 CPU（allow_cpu_fallback 时可接受）        ← 极慢
4. Impossible                                   ← 本地不可行 → 云端
```

**「尽可能大上下文」模式**（请求未指定 `context_len`）：  
先在 `native_context` 下按 2 的幂收敛找最大可用 ctx，  
逐级尝试 KV 精度 f16 → q8_0 → q4_0；仍放不下则回退到 4K 并尝试部分卸载。

### 4.3 量化选档

`best_quant_within(spec, hw, req, budget)` 从 `F32` 向 `IQ1_S` 遍历  
`Quantization::ALL_DESCENDING`，返回第一个能塞进预算的档位。  
用于回答"我这个显卡跑 32B 该用什么量化"。

### 4.4 告警而非失败

评估过程产生的非致命问题记为 `Warning`，不中断流程：

| Warning                   | 触发条件               |
| ------------------------- | ------------------ |
| `QuantMismatch`           | 声明位宽与实测偏差 > 15%    |
| `ContextClamped`          | 请求 ctx 超过模型原生上限    |
| `KvCacheDowngraded`       | 因显存紧张降了 KV 精度      |
| `PartialOffloadPenalty`   | 只能部分卸载             |
| `CpuFallback`             | 无加速卡               |
| `UnifiedMemoryContention` | Apple Silicon 统一内存 |

---

## 5. 调度决策器

### 5.1 决策流程

```
                  ┌──────────────────────┐
  部署请求 ───────▶│ ① 评估本地可行性      │
                  └──────────┬───────────┘
                             │
               ┌─────────────┴─────────────┐
          本地可行                      本地不可行
               │                            │
               ▼                            │
     ┌──────────────────┐                   │
     │ ② 下发本地部署    │                   │
     └────────┬─────────┘                   │
              │                             │
         ┌────┴────┐                        │
       成功      失败                       │
         │        └──────────┬──────────────┘
         ▼                   ▼
     返回方案        ┌─────────────────────┐
                    │ ③ 云端调度           │
                    │  NodeSelector 打分   │
                    └─────────┬───────────┘
                              │
                     成功 / 重试耗尽 / 禁止云端
```

### 5.2 四条不变量

| 编号 | 内容                              | 守护方式                                  |
| -- | ------------------------------- | ------------------------------------- |
| I1 | `allow_cloud = false` 时绝不产生云端方案 | `scheduler::tests::invariant_i1_*`    |
| I2 | 云端方案必然带非空 `fallback_trail`      | `invariant_i2_*` + `assemble_plan` 兜底 |
| I3 | 本地可行时首选本地（CPU 可跑也算可行）           | `invariant_i3_*`                      |
| I4 | 降级重试次数 ≤ `max_fallbacks`        | `invariant_i4_*`                      |

**I3 的深意**：CPU 推理很慢（<5 tok/s），但它**确实能跑**。  
按"本地优先"原则，不该因慢而降级。是否接受 CPU 由请求的  
`allow_cpu_fallback` 决定（默认允许）。这条设计避免了"用户明明能在本地跑，  
却因为程序觉得太慢而偷偷花钱上云"。

### 5.3 端口分配

- 区间由 `local.port_range_start/end` 给出
- 网关构造时把台账里**活跃部署**的端口灌入 `reserved`
- 每次分配跳过 `reserved` 与不可绑定的端口，并立即记入 `reserved`
- 区间耗尽时返回起点（让失败发生在引擎启动阶段，而非分配阶段）

### 5.4 本地失败后的降级

```rust
degrade_after_local_failure(evaluated, req, failure, attempts)
```

与 `decide()` 的区别：已知本地试过且失败，轨迹里要记**具体失败原因**  
（如"端口 11435 被占用"），而不是"资源不足"这种预判。  
这个区别直接影响用户排障方向，因此错误信息必须准确。

---

## 6. 多云统一抽象

### 6.1 驱动接口

```rust
pub trait ProviderDriver: Send + Sync {
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
    fn probe_latency_ms(&self) -> Option<u32> { None }
}
```

约束：

1. 禁止 `unwrap` 网络结果，厂商错误码必须归一化为 `MnError::Provider`
2. 除 `create` 外全部幂等（调度器会重试）
3. 不暴露任何厂商私有字段（放 `extra` / 驱动内部）
4. 对象安全（以 `dyn` 使用）

### 6.2 两条新增服务商路径

**配置路径（首选）**——写一段 TOML，无需改代码、无需重编译：

```toml
[providers.autodl.profile]
name = "autodl"
display_name = "AutoDL"
[providers.autodl.profile.auth]
type = "bearer"
[providers.autodl.profile.endpoints]
base_url = "https://api.autodl.com"
list_specs = "/api/v1/machine/region/available"
create = "/api/v1/instance?snapshot_id={spec_id}"
start = "/api/v1/instance/{id}/power_on"
stop = "/api/v1/instance/{id}/power_off"
destroy = "/api/v1/instance/{id}"
describe = "/api/v1/instance/{id}"
list = "/api/v1/instance"
status_field = "code"
success_code = "Success"
error_field = "msg"
[providers.autodl.profile.fields]
list_root = "data.list"
item_root = "data"
id = "instance_id"
state = "status"
gpu_model = "gpu_name"
gpu_count = "gpu_num"
vram = "gpu_memory"
vram_unit = "gib"
# ...
[providers.autodl.profile.states.mapping]
running = "running"
shutdown = "stopped"
creating = "creating"
starting = "starting"
```


**代码路径（特殊场景）**——厂商需要签名算法（阿里云 RPC 签名）、  
分页聚合、异步任务轮询时，新写一个 `impl ProviderDriver` 并在  
`registry::build_registry` 注册。

两条路径对上层完全等价。

#### 6.2.1 第三条路径：`mangosteen-backend`（协议形态完全不同的厂商）

`profile` 能表达"路径 + 字段 + 状态"的差异，但假设了 REST 形态
（每动作一个 URL、Token 放请求头、JSON body）。**优云智算用的是 UCloud RPC 形态**，
三者全部不成立：

| 维度 | REST profile 假设 | 优云智算实际 |
|------|-------------------|--------------|
| 端点 | 每个动作一个路径 | 全部 `POST https://api.compshare.cn` |
| 动作区分 | HTTP 方法 + 路径 | 表单参数 `Action=CreateCompShareInstance` |
| 认证 | 请求头 Token | 参数 `PublicKey` + `Signature`（SHA1 签名） |
| 编码 | JSON body | 扁平表单（`Disks.0.Size=100`） |
| 成功判定 | HTTP 2xx | HTTP 2xx **且** `RetCode == 0` |

签名无法用字段映射表达，所以这类厂商单独放进 `mangosteen-backend`：

```
cloud/registry.rs ── handles(driver)? ──▶ backend::build_from_options
                                              │ sign / params / client / driver
                                              ▼
                                       https://api.compshare.cn
```

`backend` 内部的分层（每层只做一件事，便于对着文档逐条核对）：

| 模块 | 职责 |
|------|------|
| `sign` / `sha1` | UCloud V1 签名（工作区只依赖 sha2，SHA-1 自实现并用官方向量校验） |
| `params` | 参数扁平化；**签名用原值、传输用 URL 转义**（混用必然签名不匹配） |
| `model` | 厂商响应结构（字段名与文档逐字一致，全部 `default`） |
| `mapping` | 唯一换算点：单位（MB/GB）、状态、SSH 端口、价格折算 |
| `retcode` | 返回码语义归类（容量不足→换节点 / 欠费→停止重试） |
| `client` | 各 Action 的具体调用 |
| `driver` | `ProviderDriver` 实现 + 业务补偿 |

**驱动层承担的三个业务补偿**（上层因此不必认识厂商限制）：

1. **创建前强制查库存**：厂商文档要求判库存必须用 `CheckCompShareResourceCapacity`，
   不能用 Describe 类接口代替。无库存直接返回可降级错误，不提交创建请求。
2. **销毁前先关机**：`TerminateCompShareInstance` 仅对 `Stopped` 实例有效。
   `destroy()` 内部完成"关机 → 等 Stopped → 删除"，否则运行中的实例永远删不掉。
3. **创建后补详情可失败但 ID 必须保留**：创建接口只返回短 ID，
   补查 `Describe` 失败时返回带 ID 的占位实例——否则编排器拿不到 ID，回滚无从谈起。

**单位陷阱**（写进代码注释并有单测钉死）：创建/价格接口的 `Memory` 是 **MB**，
机型表的 `Memory` 与库存接口的 `Mem` 是 **GB**。混用会让显存/内存评估差 1024 倍，
直接把"本地优先"的降级判断带偏。

### 6.3 驱动注册表的容错

```rust
build_registry_with(config, make_transport) -> RegistryBuild {
    registry,   // 成功加载的驱动
    skipped,    // (服务商名, 跳过原因)
}
```

**单个服务商配置错误不中断其它服务商**。理由：某个密钥没配好，  
不该让整个客户端不可用。这与"部署失败自动降级重试"是同一容错哲学。

### 6.4 传输层抽象

驱动只依赖 `Transport` trait：

```rust
pub trait Transport: Send + Sync {
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse>;
}
```

- 生产：`CurlTransport`（系统 curl，自带 TLS，无新增依赖）
- 测试：`MockTransport`（预设响应 + 记录请求）

**为什么用 curl 而不是 HTTP 客户端库**：云端 API 是低频操作（分钟级），  
进程启动开销（~10ms）可忽略；而 curl 三平台自带、自带 TLS、  
不引入供应链风险。认证头通过 `--config` 从 0600 临时文件读取，  
避免出现在进程参数列表。

`mangosteen-backend` 出于同样的理由用 curl，但**认证参数在 body 里**
（UCloud 签名的一部分），所以改用 `--data-binary @-` 从 stdin 喂入请求体：
既不进进程参数列表，也不需要临时文件（少了权限与残留清理两类问题）。

**URL 只能出现一次**。云端 `CurlTransport` 曾把 URL 同时写进 `--config`
文件与 argv，curl 于是把同一请求发了两次，响应体变成
`{json}\n200{json}\n200`，调用方解析报 `trailing characters at line 2`。
对 GET 只是「看起来怪」，但对 POST 创建实例意味着可能开出两台并持续计费。
由 `curl::tests::url_must_appear_exactly_once` 守护。

### 6.5 云端实例监控

设备页在「使用云端算力」时需要显示云端实例的 CPU / GPU / 显存 / 内存 / 磁盘
使用率。这条链路的三个关键决定：

```text
后台线程（每 5 秒）──持锁读台账──▶ provider + instance_id
        │
        └──释放锁──▶ 厂商监控接口（约 0.5 秒）──▶ 共享快照
                                                     │
HTTP /api/sysinfo ◀── 只读内存（微秒级）──────────────┘
```

1. **绝不在请求路径里调厂商接口**。设备页每 2 秒轮询；同步调用会卡页面、
   持网关锁拖慢其它 API，还会触发厂商限流。
2. **绝不在网关锁内做网络往返**（与「27 秒引擎探测」同一教训）。
3. **单位归一化放在驱动内**：厂商返回 0~100 百分数，本项目本地监控用 0~1
   比例。混用会出现「CPU 3500%」，因此 `percent_to_ratio` 是唯一换算点。

空态必须**三态可分**，否则用户无法判断该找谁：

| 状态 | 含义 | 界面 |
|------|------|------|
| `supported: false` | 该服务商没有监控接口 | 「未提供实例监控接口」 |
| `has_any: false` | 实例存在但厂商暂无数据（关机/刚开机） | 「实例可能已关机…」 |
| `error` | 调用失败（鉴权/网络） | 「查询失败：…」并保留上次快照 |

兜底：监控接口的 `Results` 为空时，用 `DescribeCompShareInstance` 的
`MonitorMessages`（当前值快照）补 CPU/内存，避免一片空白。

---

### 6.6 优云智算 GPU 接口覆盖矩阵

厂商 OpenAPI 共 **71 个接口**，分 4 类。Mangosteen 只适配**与「部署并运行推理」
直接相关**的部分，其余明确不做——列出来是为了让后来者知道「不是漏了，是想过」。

#### instance 分类（28 个）：已全部适配

| 接口 | 用途 | 关键约束 |
|------|------|----------|
| `DescribeCompShareSupportZone` | 可用区 + 镜像兼容性 | `UnsupportedImageTypes` 提前拦 226603 |
| `DescribeAvailableCompShareInstanceTypes` | 可售机型与合法组合 | `Memory` 是 **GB** |
| `CheckCompShareResourceCapacity` | 库存（创建前必调） | `Region`/`Zone` 必填；`Mem` 是 **GB** |
| `GetCompShareInstancePrice` | 单模式价格 | `Memory` 是 **MB** |
| `GetCompShareInstanceUserPrice` | 全模式价格（时/日/月/Spot） | 便于对比「按量 vs 包月」 |
| `GetCompShareInstanceUpgradePrice` | 升降配差价 | 单位是**元**（差价），不是时价 |
| `GetCompShareRefundPrice` | 删除可退金额 | `UHostIds` 走 **JSON 数组**；按 `Code==0` 才可退 |
| `CreateCompShareInstance` | 创建 | `Memory` 是 **MB**；`Disks` 扁平化 |
| `DescribeCompShareInstance` | 查询（含 `MonitorMessages`） | 不传 Region 可查全部地域 |
| `StartCompShareInstance` | 开机 | 仅 `Stopped` |
| `StopCompShareInstance` | 关机 | 仅 `Running` |
| `RebootCompShareInstance` | 重启 | 仅 `Running` |
| `TerminateCompShareInstance` | 删除 | **仅 `Stopped`**，`destroy()` 自动先关机 |
| `ModifyCompShareInstanceName` | 改名 | ≤63 字符 |
| `ResetCompShareInstancePassword` | 重置密码 | `Password` 需 **Base64**；虚机仅 `Stopped`，容器可运行中 |
| `ReinstallCompShareInstance` | 重装系统 | 仅 `Stopped`；系统盘清空 |
| `ResizeCompShareInstance` | 升降配 / 扩盘 | 仅 `Stopped`；**计算与磁盘二选一** |
| `SwitchChargeType` | 按量 ⇄ 包月 | 余额不足可能回 `520` |
| `UpdateCompShareInstancePorts` | 容器对外端口 | **仅容器区**；**全量替换**（未传的会被删） |
| `UpdateCompShareStopScheduler` | 定时关机 | 需 `ProjectId`；目标时间 ≥ now+300s |
| `DeleteCompShareStopScheduler` | 取消定时关机 | 需 `ProjectId` |
| `SwitchCompShareEIPShareBandwidth` | 带宽归属切换 | `UHostIds` 走 **JSON 数组**；**顶层成功≠每个成功** |
| `GetCompShareInstanceMonitor` | 监控指标 | **必须 JSON 请求体**；数组签名按元素拼接 |
| `DescribeCompShareSoftwarePort` | 软件端口 | 实测**不含 SSH**（SSH 走 `TcpForwards`） |
| `GetSoftwareURL` | 软件访问地址 | Action 是全大写 `URL` |
| `DescribeCompShareMachineTypeFamilies` | 机型族目录 | 全量机型（含未上架） |
| `CheckCompShareNetOptimizer` | 网络加速状态 | 影响海外模型下载速度 |
| `GetProjectList` | 项目列表 | 定时关机的前置依赖（取默认 `ProjectId`） |

#### 明确不适配的 43 个

| 分类 | 数量 | 为什么不做 |
|------|------|-----------|
| `image/*`（自制/社区/收藏镜像） | 17 | 属于**镜像制作与分享**，不是部署链路。部署只用系统镜像；自制镜像由控制台管理 |
| `data/*`（云盘挂载/扩容/快照） | 9 | 属于**存储管理**。实例自带系统盘足够放 GGUF，不需要独立云盘编排 |
| `team/*`（团队/成员/订单/日志） | 17 | 属于**账号与计费体系**。Mangosteen 是个人级客户端，不做多租户（产品边界） |

> 如果将来要支持「团队共享 GPU 池」，`team/*` 才需要重新评估。
> 现在实现它们等于给一个不存在的功能写代码。

#### 三条跨接口的适配经验

1. **带数组参数就必须走 JSON 请求体**。表单编码（`UHostIds.0`）对
   `GetCompShareInstanceMonitor`、`GetCompShareRefundPrice`、
   `SwitchCompShareEIPShareBandwidth`、`UpdateCompShareInstancePorts`
   一律失败；而 `DescribeCompShareInstance` 用扁平索引键是正常的。
   两种形态并存，不能一刀切。
2. **JSON 通道的签名口径不同**：数组元素**直接首尾相接**
   （`UHostIdsu-1u-2`），逗号分隔或 JSON 字面量都会被判签名错误。
3. **`Memory` 单位随接口变化**：创建/价格/升降配是 **MB**，
   机型表/库存是 **GB**。差 1024 倍，且不会报错——只会算错钱或选错规格。

#### 状态前置校验统一放在驱动层

升降配、重装、重置密码（虚机）都要求实例 `Stopped`。厂商对状态不符只回
笼统的参数错误，用户无从判断该做什么。因此驱动层先查实例状态，
再给出**可操作**的提示（「请先执行 `mangosteen cloud stop <id>`」），
而不是把厂商原文透传上去。

---

## 7. 后端节点选择器

### 7.1 两阶段

**阶段一：硬性过滤**（`SelectionConstraints::admits`）

| 约束     | 默认       |
| ------ | -------- |
| 最低显存   | 模型所需     |
| 最低内存   | 模型所需     |
| 最低磁盘   | 模型大小 × 2 |
| 价格上限   | 配置或请求    |
| 服务商白名单 | 空（全部）    |
| 地域白名单  | 空（全部）    |
| 要求有库存  | 是        |

**阶段二：四维加权打分**

```
score = w_price·S_price + w_latency·S_latency + w_availability·S_avail + w_hardware·S_hw
```

归一化基准是**候选集内**的最优值（相对比较），不是绝对尺度。

### 7.2 权重语义

配置里写的是**相对重要性**（如 price=40, latency=15...），不必加和为 1。  
打分时统一归一化一次。这样用户改单个权重不会意外改变其它维度的相对占比。

默认权重偏向"性价比 + 稳定"（价格 40 / 可用性 30 / 延迟 15 / 硬件 15），  
因为个人用户对延迟不敏感、对价格敏感。

### 7.3 排序的确定性

```
总分降序 → 价格升序 → provider 字典序 → spec_id 字典序
```

同分同价同 provider 的候选按 `spec_id` 排序，保证结果可复现  
（快照测试与日志对比需要）。

### 7.4 「就要最便宜」：纯价格序（`cheapest_first`）

打分策略是**配置项**（`scheduler.strategy`）。用户临时说「给我最便宜的」时
不能指望配置恰好是 `cheapest`——默认 `balanced` 下四维打分会让延迟/库存分
翻盘（实测出现过 ¥2.15/h 的 4090 排在 ¥1.34/h 的 V100S 前面）。

因此单开一条**不受配置影响**的价格序：

```
价格升序（未知价排最后）→ 库存多优先 → provider → spec_id
```

排序口径抽成 `policy::cheapest_cmp`，由 `SelectionStrategy::Cheapest` 与
`SelectionConstraints::cheapest_first` **共用**。两处各写一遍必然漂移，
表现是「`cloud pick` 说 A 最便宜、`cloud cheapest` 说 B」——用户只会觉得
工具在乱说。

**未知价必须排最后**，不能当 0 元：厂商对个别规格返回 `f64::MAX` 当无报价
哨兵，上层落成 `0.0` 后按升序会排到第一，于是「最便宜」先去开最贵的卡。

### 7.5 「可用」由创建结果定义

厂商库存接口不可信：优云是**模拟调度**（对能开出来的机器也返回 false），
AutoDL 只覆盖部分机型。因此 `cloud cheapest --create` **不拿 `stock` 当准入
条件**，而是按价格顺序逐个真开：

```
P40  ¥0.380  → 226604 无库存  ✗
2080 ¥0.390  → 226604 无库存  ✗
3080Ti ¥0.700 → 226604 无库存 ✗
3090 ¥1.130  → 226604 无库存  ✗
V100S ¥1.340 → 创建成功       ✓   ← 「当前可用的最低价」就是它
```

（2026-10-26 真实调用记录。）

这条路径与 `provision` 共用 `create_first_running`，因此继承了同一条不变量：
**创建成功但起不来的实例，必须先销毁（带重试）才换下一个候选**——销毁失败
就立刻返回错误并带上残留实例 ID，绝不继续建（会同时留两台计费）。

### 7.6 裸实例必须能被清理

`cloud cheapest --create` 建的是**裸实例**：不建隧道、不传模型、不写部署台账。
代价是它不会被 `stop` / `abort` 自动回收，也不会出现在「模型实例」列表。

因此配套提供 `cloud destroy --instance <服务商>/<实例ID>`：没有这条路径，
用户想清理只能去厂商控制台，而「忘了去控制台」正是持续计费最常见的成因。
创建成功后 CLI 会直接打印这条可复制执行的命令。

---

## 8. 本地推理链路

### 8.1 生命周期

```
locate()   → 找 llama-server（显式路径 → 额外目录 → PATH）
launch()   → 校验模型存在 → 校验端口可用 → 拉进程 → 等 /health
status()   → 进程存活 + HTTP 探活
shutdown() → SIGTERM → 8s → SIGKILL
```

### 8.2 关键设计决策

**启动前校验模型文件存在**：省掉一次无意义的进程拉起与 20 行无用的 stderr。

**端口被占用时明确报错**：而不是让 llama-server 自己失败（它的错误信息很差）。

**输出重定向到日志文件**，不用管道：管道模式下如果父进程不持续读取，  
子进程写满 64KB 缓冲会阻塞，表现为"引擎启动后卡死"。

**进程提前退出时立即报错并带 stderr 尾部**：这是最常见的失败模式  
（模型损坏、显存不足、动态库缺失），必须把原因直接透给用户。

**`EngineHandle` 的 `Drop` 不杀进程**：用户可能想让服务继续跑  
（例如 CLI 退出但希望推理服务在线）。必须显式 `shutdown()`。  
这是有意为之——隐式杀进程会让 `mangosteen deploy` 之后无法继续用服务。

### 8.3 跨进程可见性

引擎进程由 `deploy` 启动，但 `serve` 是另一个进程。  
后者没有 `EngineHandle`，因此 `refresh()` 必须回退到 **HTTP 探活**  
（`probe_local_health`，2s 超时）。若探不通且台账说 Ready，则修正台账为 Failed。

停止时同理：无句柄则按台账记录的 PID 调用 `terminate_pid`。

---

## 9. 云端网络工具集

### 9.1 SSH 隧道

```
ssh -N -T \
    -o ExitOnForwardFailure=yes \      ← 端口占用时立即失败，避免"假连通"
    -o ServerAliveInterval=15 \        ← 保活，防 NAT 静默断开
    -o ServerAliveCountMax=3 \
    -o StrictHostKeyChecking=accept-new \
    -o ControlMaster=auto \            ← 连接复用，降低重连延迟
    -o ControlPersist=60 \
    -p <ssh_port> \
    -L 127.0.0.1:<local>:127.0.0.1:<remote> \
    <user>@<host>
```

建立后的就绪判据：**本地端口可连通**（不是"进程活着"——  
SSH 可能活着但转发已断）。

### 9.2 健康检查与自动重建

`ensure_alive()`：检查 → 断了就关旧进程、释放端口、重建。  
这是"云端实例像本地服务一样无感访问"的关键——网络抖动时上层不必感知。

### 9.3 端口分配器

`PortAllocator` 抽成独立结构以便单测（真实探测需要绑定 socket，  
而"跳过已占用端口"的逻辑与 OS 无关）。

### 9.4 存储映射

三种方式：`Mount`（SSHFS/SMB）、`Sync`（rsync）、`None`（云端自行下载）。

rsync 参数固定包含 `--partial`（断点续传，大模型必开）与  
`--inplace`（避免临时文件占双份磁盘）。

---

## 10. 统一协议适配层

### 10.1 接口清单

| 方法     | 路径                      | 说明          |
| ------ | ----------------------- | ----------- |
| GET    | `/health`               | 网关存活 + 后端就绪 |
| GET    | `/v1/models`            | [OI] 模型列表   |
| POST   | `/v1/chat/completions`  | [OI] 对话     |
| POST   | `/v1/completions`       | [OI] 文本补全   |
| POST   | `/v1/embeddings`        | [OI] 向量化    |
| GET    | `/api/status`           | 网关状态        |
| POST   | `/api/deploy`           | 发起部署        |
| POST   | `/api/assess`           | 仅评估         |
| GET    | `/api/deployments`      | 部署列表        |
| GET    | `/api/deployments/{id}` | 单个部署        |
| DELETE | `/api/deployments/{id}` | 停止部署        |

[OI] = OpenAI 兼容。

### 10.2 请求规范化

**只改 `model` 字段**，其余原样保留：

| 输入               | 处理                                  |
| ---------------- | ----------------------------------- |
| `model = "auto"` | 替换为当前部署的模型 ID                       |
| `model = ""`     | 同 `auto`                            |
| `model = "具体名"`  | 原样透传                                |
| 未知字段             | 原样透传（OpenAI 兼容性的关键）                 |
| 采样参数缺失           | 不补默认值（交给 llama.cpp 用它自己的默认值，避免双重默认） |
| 采样参数越界           | 400 + 指明参数名                         |

### 10.3 错误归一化

上游（llama.cpp）的错误体统一转换成 Mangosteen 格式，  
上层应用只需处理一种错误结构：

```json
{
  "error": {
    "message": "...",
    "type": "invalid_request_error",
    "code": "bad_request"
  }
}
```

HTTP 状态码映射：

| 错误                                                | 状态码     |
| ------------------------------------------------- | ------- |
| `BadRequest` / `ModelNotFound`                    | 400     |
| `AccessDenied`                                    | 401/403 |
| `InsufficientLocalResource` / `NoViableCandidate` | 503     |
| 其它                                                | 500     |

### 10.4 为什么网关不自己实现推理

网关只做协议转换与路由。这样：

- 本地与云端走同一份转发代码，行为一致
- 上游升级 llama.cpp 时网关无需改动
- 网关不可用时用户仍可直连后端排障

### 10.5 HTTP 服务的实现取舍

用标准库手写 HTTP/1.1（每连接一线程），不引入 axum/hyper。

| 需求       | 是否满足           |
| -------- | -------------- |
| 本机回环监听   | 是              |
| 11 个固定路由 | 是              |
| JSON 收发  | 是              |
| 并发       | 线程池，个人级（<64）足够 |

**代价**：不支持 HTTP/2、不支持 TLS。两者对"仅监听 127.0.0.1 的个人客户端"都不必要。

---

## 11. 会话管理

### 11.1 定位

基线把「用户会话管理」放在**第一层**，职责是「管理用户会话、部署任务会话、
推理会话生命周期；维护任务状态、上下文、操作记录，作为最上层交互入口」。

关键边界：**它不感知算力来源**。会话层不判断能不能部署、不启动引擎、
不访问云厂商——那些都是下层的事。它只记录「谁、在什么时候、做了什么、
结果如何」。

### 11.2 两类会话

| 种类 | 内容 | 生命周期 |
|------|------|----------|
| `Chat`（推理会话） | `messages: [{role, content, at_ms}]` | 新建 → 追加消息 → 删除 |
| `Deploy`（部署任务会话） | `operations: [{action, detail, ok, at_ms}]` | 新建 → 记录操作 → 删除 |

**为什么部署也要一个「会话」**：一次部署不是单点动作，而是
「评估 → 选点 → 下载 → 加载 → 就绪」的过程，中途可能降级、重试、失败。
把这些记在同一条时间线上，事后才能回答「上次为什么上了云」。
只记最终状态等于把排查线索丢掉。

### 11.3 持久化

单文件 JSON（`sessions.json`）+ 原子写，与部署台账、模型台账同目录。

**为什么不用数据库**：三个台账加起来不过几十 KB，引入 SQLite 会多一个
跨平台编译与迁移负担。JSON 直读也便于用户自己看、自己备份。

**为什么必须有 `prune`**：会话文件随对话增长，长期使用后会到几 MB。
每次读写都解析整份 JSON，UI 会肉眼可见地变慢。`prune(keep)` 保留最近的
若干个，是必要的上限保护。

### 11.4 接口

| 方法 | 路径 | 说明 |
|------|------|------|
| `GET` | `/api/sessions?kind=chat\|deploy` | 列表（摘要，不含消息体） |
| `POST` | `/api/sessions` | 新建（`kind` / `title` / `model` 均可省略） |
| `GET` | `/api/sessions/{id}` | 详情（含消息与操作记录） |
| `DELETE` | `/api/sessions/{id}` | 删除（幂等） |
| `POST` | `/api/sessions/{id}/messages` | 追加消息 |
| `POST` | `/api/sessions/{id}/operations` | 记录操作 |

列表只返回摘要：几十轮对话的会话带完整消息体会让列表接口膨胀到几百 KB，
而列表页根本不需要消息内容。

`title` 留空时由**首条用户消息**自动生成（按字符截断到 24 字 + 省略号，
不是按字节——按字节会切坏中文）。

### 11.5 与推理链路的关系

会话层**不参与**推理转发。`POST /v1/chat/completions` 仍是网关直接转发给
引擎（基线：网关不自己实现推理）。消息落库由调用方（UI / CLI）在拿到
完整回复后另行调用会话接口。

这样做的原因：流式响应下「什么时候算一条完整回复」只有调用方知道。
塞进网关的转发路径会让协议层多出一个状态机，而它对转发本身没有价值。

---

## 12. 基础能力层

### 11.1 模型台账

JSON 存储（不是数据库）。理由：个人场景模型数量在几十量级；  
用户可以直接手改排障（符合"配置驱动"调性）；写操作走原子替换不会写坏。

`register_local()` 流程：访问控制校验 → 解析 GGUF → 可选哈希 → 写入台账。  
重复注册同一 `name+quant` 会**更新**（幂等），便于"同一模型换了位置"。

**哈希默认在登记时不算**：7B 模型 4.5GiB，机械盘上要数十秒，  
会让 `models add` 体验极差。改为 `models verify` 时按需计算  
（首次计算并记录，后续比对）。

### 11.2 部署台账

**每次状态变更立即落盘**（不是退出时保存），原子替换。

理由：一次云端部署可能持续数小时并产生真实计费。进程重启后若丢失台账：  
用户看不到在跑什么实例 → 忘记关机 → 持续扣费；无法清理已创建实例 →  
云端资源泄漏。

状态机：`Planned → Provisioning → Ready → Stopped → Destroyed`  
（另有 `Failed`）。非法迁移（如 `Destroyed → Ready`）被拒绝。

### 11.3 文件访问控制

威胁模型：

1. 误把系统文件当模型读（`/etc/shadow`）→ 信息泄露
2. 授权目录内的符号链接指向外部 → 前缀判断被绕过
3. `../../` 路径穿越
4. 读取超大文件耗尽内存/磁盘

防御：

| 威胁 | 措施                                |
| -- | --------------------------------- |
| 1  | 白名单前缀 + 必须存在且为普通文件                |
| 2  | `fs::canonicalize` 后再比较（符号链接无处遁形） |
| 3  | 词法归约兜底（目标不存在时仍能拦住 `..`）           |
| 4  | 单文件体积上限                           |

授权范围 = `models_dir` ∪ `cache_dir` ∪ `allowed_read_roots`。

---

## 13. 平台适配层

### 12.1 路径规范

| 用途 | macOS                                       | Linux                              | Windows                            |
| -- | ------------------------------------------- | ---------------------------------- | ---------------------------------- |
| 配置 | `~/Library/Application Support/mangosteen`  | `$XDG_CONFIG_HOME/mangosteen`      | `%APPDATA%\mangosteen`             |
| 模型 | `.../Application Support/mangosteen/models` | `$XDG_DATA_HOME/mangosteen/models` | `%LOCALAPPDATA%\mangosteen\models` |
| 缓存 | `~/Library/Caches/mangosteen`               | `$XDG_CACHE_HOME/mangosteen`       | `%LOCALAPPDATA%\mangosteen\cache`  |
| 日志 | `~/Library/Logs/mangosteen`                 | `$XDG_STATE_HOME/mangosteen/log`   | `%LOCALAPPDATA%\mangosteen\logs`   |

每个都支持 `MANGOSTEEN_*` 环境变量覆盖。

**为什么不用 `dirs` crate**：规则只有四类、覆盖三平台，手写不足 200 行  
且可完全无副作用单测（注入环境变量读取器）；引入第三方反而增加审计面。

### 12.2 硬件采集

| 平台      | GPU                                                            | 内存                              | CPU                                            |
| ------- | -------------------------------------------------------------- | ------------------------------- | ---------------------------------------------- |
| macOS   | `system_profiler SPDisplaysDataType -json`；Apple Silicon 用统一内存 | `sysctl hw.memsize` + `vm_stat` | `sysctl hw.physicalcpu/logicalcpu`             |
| Linux   | `nvidia-smi --query-gpu=...`；AMD 读 `rocm-smi`                  | `/proc/meminfo`                 | 解析 `/proc/cpuinfo`（去重 `physical id`+`core id`） |
| Windows | `nvidia-smi`；否则 PowerShell WMI                                 | `Win32_OperatingSystem`         | `Win32_Processor`                              |

**降级策略**：采集失败**绝不**导致进程退出。每项失败退回保守默认值  
并记入 `warnings`，在 `mangosteen doctor` 里可见。

### 12.3 进程管理

| 差异点  | Unix                  | Windows                    |
| ---- | --------------------- | -------------------------- |
| 温和终止 | `SIGTERM`             | `taskkill /PID`            |
| 强杀   | `SIGKILL`             | `taskkill /PID /F`         |
| 进程组  | `process_group(0)`    | `CREATE_NEW_PROCESS_GROUP` |
| 僵尸判定 | `ps -o stat=` 首字母 `Z` | 无此概念                       |

**SIGKILL 后的判定逻辑**：SIGKILL 不可捕获，发出后必然终止。  
若 `kill(pid,0)` 仍成功，只可能是僵尸态或内核尚未调度终止。  
此时"倾向于认为已终止"比"报错"更安全——误报失败会让调用方重复清理甚至放弃清理，  
而资源实际上已经释放了。

---

## 14. 配置体系

### 13.1 搜索顺序

1. `--config <path>` 显式指定（**必须存在**，否则报错）
2. `MANGOSTEEN_CONFIG` 环境变量（同样必须存在）
3. 用户配置目录
4. 系统级目录（`/etc/mangosteen`，Windows 无）
5. 全部缺失 → 内置默认（首次运行场景）

**显式路径缺失为什么报错而非回退**：那是用户明确要求的，  
静默回退到默认值会让人误以为配置生效了。

### 13.2 密钥表达

三种来源，`config show` 只输出脱敏形式：

```toml
secret = { env = "AUTODL_API_TOKEN" }    # 显示为 ${AUTODL_API_TOKEN}
secret = { file = "/run/secrets/tok" }   # 显示为 file:/run/secrets/tok
secret = { inline = "xxx" }              # 显示为 <inline:***redacted***>
```

配置文件以 `0600` 权限创建（Unix）。

### 13.3 向前兼容

所有字段都有 `serde(default)`。旧版配置在新版本下可正常加载，  
缺失项取默认值。

---

## 15. 错误处理与降级

### 14.1 错误分类与可重试性

| 变体                                              | 可重试 | 理由               |
| ----------------------------------------------- | --- | ---------------- |
| `InsufficientLocalResource`                     | 是   | 本地链路的典型降级触发点     |
| `EngineLaunch` / `EngineExited`                 | 是   | 换云端再试            |
| `Provider` / `NoViableCandidate` / `Network`    | 是   | 换节点再试            |
| `Config` / `ConfigIo` / `BadRequest`            | 否   | 与算力来源无关，换链路也不会成功 |
| `ModelNotFound` / `GgufFormat` / `HashMismatch` | 否   | 文件问题，换算力来源无用     |
| `InvalidTransition`                             | 否   | 状态机错误，重试只会重复失败   |

`MnError::is_retryable()` 是调度决策器判断是否降级的依据。

### 14.2 云端编排的资源回滚

**核心不变量**：`provision()` 返回 `Err` 时，云端不应残留任何由本次调用创建的资源。

任何一步失败（创建失败、启动超时、缺 SSH 信息、隧道失败）都要销毁已创建的实例。  
若回滚本身也失败，必须把**残留实例 ID** 与**手工清理命令**写进错误消息：

```
⚠ 自动销毁实例 i-abc123 失败：...
请立即手工处理，避免持续计费：
  mangosteen cloud destroy i-abc123 --provider autodl
```

否则用户会被静默扣费。

### 14.3 错误消息的三段式

```
✗ 无可用云端算力节点：本地部署失败（推理引擎进程 46873 被信号终止：...）；
  但未配置任何可用的云服务商，无法降级。请检查 config.providers ...
  建议： 放宽约束（提高价格上限 / 降低显存要求），或启用更多服务商
```

1. **现象**：发生了什么
2. **上下文**：为什么会这样（本地失败？资源不足？）
3. **建议**：下一步该做什么

`output::suggest()` 为每种错误类型提供具体建议，不留"未知错误"。

---

## 16. 安全设计

| 面      | 措施                                         |
| ------ | ------------------------------------------ |
| 密钥存储   | 支持 env/file 引用；配置文件 0600；`config show` 脱敏  |
| 认证头传递  | 通过 0600 临时配置文件传给 curl，不放在进程参数里             |
| 推理服务暴露 | 本地绑 `127.0.0.1`；云端绑 `0.0.0.0` 但只经 SSH 隧道访问 |
| 网关鉴权   | 可选 Bearer / `X-API-Key`；开启时无密钥直接 401       |
| 文件访问   | 白名单 + 真实路径解析 + 体积上限 + 扩展名限制                |
| 路径穿越   | 词法归约 + 符号链接解析（防 TOCTOU）                    |
| 云端实例清理 | 失败必回滚；回滚失败必须暴露残留 ID                        |
| 网络重定向  | curl `--max-redirs 0`（防止认证头被带到别的域）         |

### 危险操作的确认机制

删除模型文件、销毁云端实例、批量停止——这些**不可逆**操作要求 `--yes`：

```
⚠ 此操作非常危险，可能导致不可逆的数据丢失！
  将删除：/path/to/model.gguf

  确认执行请加 --yes：mangosteen models remove x --delete-file --yes
```

未加 `--yes` 时返回退出码 2（可被脚本区分）。

---

## 17. 测试策略

### 16.1 分层测试

| 层          | 测试类型                | 数量      |
| ---------- | ------------------- | ------- |
| `core`     | 纯单元测试（无 IO）         | 142     |
| `platform` | 单元测试（含真实 syscall）   | 36      |
| `base`     | 单元测试（临时目录）          | 47      |
| `local`    | 单元测试（假引擎脚本）         | 12      |
| `cloud`    | 单元测试（MockTransport） | 78      |
| `gateway`  | 单元测试 + **端到端集成**    | 71 + 11 |
| `cli`      | 单元测试 + **架构约束**     | 72 + 18 |

### 16.2 关键测试设计

**资源估算的边界测试**：

- 7B Q4 适配 8GB / 24GB 卡的不同落位形态
- 32B 在 8GB 卡上必然部分卸载
- KV 量化降档能否救活"差一点"的场景
- 无 GPU 时 CPU 回退 vs 禁止回退

**状态机的完备性测试**：

- 合法路径全通
- 终态不可复活
- 不能跳过 `Starting` 直接 `Running`

**降级链的不变量测试**：

- I1~I4 四条不变量各有用例
- 本地失败时错误信息必须含"本地部署失败"而非"资源不足"
- 回滚失败必须暴露残留 ID 与清理命令

**端到端测试**（`gateway/tests/e2e.rs`，11 项）：  
用真实 GGUF 文件 + 真实的假 llama-server 子进程 + 真实 TCP 连接，验证：

- 登记 → 评估 → 部署 → 就绪 → 停止 全链路
- OpenAI 兼容接口（`/v1/models`、`/v1/chat/completions`、`/v1/embeddings`）
- `model:"auto"` 被替换为真实模型 ID
- API Key 鉴权（401 / 200）
- 本地失败降级且错误信息准确
- 跨进程引擎可见性（`deploy` 后另起 `serve` 能识别）
- 台账跨进程持久化

### 16.3 为什么端到端测试用假引擎

`llama-server` 需要真实模型文件（GB 级）与 GPU，CI 环境没有。  
假引擎用 Python 实现 `/health` 与 `/v1/chat/completions`，  
能验证协议层全链路，只是不产生真实推理结果。

这让"网关 → 引擎进程 → HTTP 转发 → 响应归一化"这条最易出错的路径  
在任何机器上都能被验证。

---

## 18. 架构约束的自动化守护

`cli/tests/layering.rs` 把提示词里的架构铁律变成可执行断言。  
任何违反都会让 `cargo test` 失败。

| 编号 | 约束       | 校验方式                                                                                                  |
| -- | -------- | ----------------------------------------------------------------------------------------------------- |
| C1 | 网关唯一收口   | CLI 的 Cargo.toml 不得依赖 local/cloud；源码不得 `use mangosteen_local::`；不得出现厂商 API 地址；除 `serve` 外不得自建 TCP 连接  |
| C2 | 平台无关     | core/gateway/base/local/cloud/backend 源码不得出现 `cfg(target_os)`；不得直接调 `libc::` / `windows_sys::`                            |
| C3 | 推理引擎唯一   | 全仓不得出现 vLLM / TensorRT / SGLang 等引擎名                                                                  |
| C4 | 本地优先     | `scheduler.rs` 中 `is_local_viable` 判断必须在 `build_cloud_plan` 之前；`Config::sample` 里 `local_first: true` |
| C5 | 依赖方向自上而下 | 逐 crate 检查 Cargo.toml 的依赖声明                                                                           |
| C5b | 厂商驱动统一入口 | `gateway` / `cli` / `base` 不得依赖或引用 `mangosteen_backend`，必须经 `cloud` 的注册表加载                          |

C2 的落地：`mangosteen-base` 的富文档提取原本内联了 `cfg(target_os)`（选择
textutil / pdftotext / Word COM），违反「平台差异收敛到 platform」。现已下沉为
`mangosteen-platform::docconv`，base 只调用不判断平台。

**技术要点**：扫描前必须剥离注释与字符串字面量。  
本仓库大量注释在**解释**为什么不用某个技术栈（例如"禁止引入 vLLM"），  
若直接扫原文会把自己写死。`strip_comments_and_strings()` 处理这一点，  
并有元测试验证剥离器本身可靠。

此外还检查：

- 生产代码中 `unwrap()` 密度 < 3/kloc（防止 panic 蔓延）
- 只支持 GGUF（不得出现 safetensors 等加载逻辑）

---

## 附：关键设计决策速查

| 决策              | 选择           | 理由                                    |
| --------------- | ------------ | ------------------------------------- |
| CPU 可跑是否算"本地可行" | 算            | 本地优先原则不因慢而变；由 `allow_cpu_fallback` 控制 |
| 推理引擎            | 只用 llama.cpp | 本地/云端参数语义必须一致                         |
| 模型格式            | 只用 GGUF      | 与 llama.cpp 生态对齐，不引入转换链               |
| 云端 HTTP 客户端     | 系统 curl      | 低频操作；无新增供应链；自带 TLS                    |
| 网关 HTTP 服务      | 标准库手写        | 11 个固定路由；HTTP/2 与 TLS 对回环监听不必要        |
| 台账存储            | JSON + 原子写   | 几十条记录；用户可手改排障                         |
| 登记时算哈希          | 默认不算         | 4.5GiB 文件数十秒；改为 verify 时按需            |
| 引擎退出时杀进程        | 不杀           | 用户可能要用服务；必须显式 stop                    |
| 并发模型            | 线程而非 async   | 推理是一次性生命周期管理，无高并发需求                   |
| 周期轮询 vs 事件      | 轮询           | 云厂商普遍无 webhook；轮询间隔可配                 |
| 云端监控采集          | 后台线程 + 5 秒缓存 | 设备页 2 秒轮询；每次同步打厂商接口会卡页面并触发限流      |
| 云端监控空态          | 三态可区分        | 「不支持 / 无数据 / 失败」混为一谈会让用户找错排查方向        |
