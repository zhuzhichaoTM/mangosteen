# 指定模型云端部署 10 秒优化与性能探测设计

## 1. 目标与边界

以 **Qwen2.5-0.5B-Instruct（Q4_K_M GGUF）+ 优云智算 P40 等低价节点**为基准，建立可重复、可归因、可回归的部署性能体系，并把优化后的部署耗时压到 10 秒级。

部署链路保持现有六阶段语义：

> 模型评估 → GPU 节点选择 → 节点配置 → 模型下载 → 模型加载 → 隧道建立

本方案的预热前提为：

- 引擎可执行文件、CUDA/系统依赖已安装；
- 模型权重已在目标节点本地磁盘，下载阶段应为 `Skipped/CacheHit`，不重复传输；
- 不做量化，不上传本地权重；
- 仍执行真实节点获取/启动、引擎进程启动、健康检查和本地隧道建立，不能用“只写台账 Ready”冒充部署完成。

### 1.1 10 秒 SLO 的严格定义

主指标：

```text
T_deploy = T_api_accept → T_ready
```

- `T_api_accept`：网关接受部署请求、完成任务参数校验并创建 job 的时间；包含后续任务排队时间。
- `T_ready`：远端引擎健康检查通过、本地隧道可用、部署台账提交为 `Ready` 的时间。
- `T_first_token` 单独统计，不混入 10 秒部署 SLO，用于观察模型运行时表现。

验收建议：同一预热场景连续 30 次，`p50 ≤ 8s`、`p95 ≤ 10s`、成功率 `≥ 99%`；任何失败和中止单独计数，不能从分位统计中删除。

### 1.2 场景必须分开承诺

| 场景 | 说明 | 10 秒目标 |
|---|---|---|
| A. 运行中节点复用 | 节点 Running，权重和引擎已缓存 | 主目标，必须达到 |
| B. 停机节点复用 | 保留磁盘，执行开机后启动引擎 | 争取达到，取决于厂商开机耗时 |
| C. 全新节点创建 | 创建云实例、初始化、启动引擎 | 单独 SLO；不承诺普遍 10 秒 |

全新实例的创建与操作系统启动通常由云厂商控制，不能通过本地代码稳定压缩到 10 秒。优化报告若混合 A/B/C，会得到失真的平均值，因此每条 trace 必须带 `node_mode=reuse_running|reuse_stopped|create_new`。

## 2. 总体思路

先做测量，再做优化；先保证归因完整，再调整并发与轮询。

```mermaid
flowchart LR
    A[部署请求] --> B[Trace 开始]
    B --> C[六阶段主 Span]
    C --> D[云 API/SSH/引擎/隧道子 Span]
    D --> E[内存 Trace]
    E --> F[后台 JSONL 持久化]
    F --> G[p50/p95/失败归因报表]
    G --> H[预算超支定位与优化]
    H --> I[同场景基准回归]
```

设计原则：

1. **进度与性能分离**：现有 `ProgressSink/PullJobs::set_stage` 继续服务 UI；新增 `DeploymentTrace` 专门计时。
2. **单调时钟计时**：耗时使用 `Instant`，不使用可被系统校时影响的 wall clock；wall clock 只用于展示。
3. **锁内只写内存**：遵守现有约束，进度 sink 和 trace span 在网关锁内不做网络/磁盘 IO。
4. **记录嵌套与并行**：阶段耗时不直接相加，保留父子关系与并行重叠，另外计算总耗时和关键路径。
5. **默认低开销**：每次部署仅几十条事件；内存有界，JSONL 在 worker 释放网关锁后落盘。
6. **敏感信息零记录**：不记录 AccessKey、SSH 密码、完整命令行、带凭证 URL；仅记录错误码和脱敏摘要。

## 3. 性能探测工具设计

### 3.1 模块与接入位置

新增 `crates/orchestration/src/trace.rs`，核心类型建议：

```rust
pub struct DeploymentTrace {
    pub trace_id: String,          // 复用 deployment_id + attempt，不引入 UUID
    pub job_id: String,
    pub deployment_id: Option<String>,
    pub model_id: String,
    pub provider: String,
    pub gpu_model: String,
    pub node_mode: NodeMode,
    pub started_at_unix_ms: u64,
    pub total_us: Option<u64>,
    pub outcome: TraceOutcome,     // Ready/Failed/Cancelled
    pub failure_stage: Option<String>,
    pub failure_code: Option<String>,
    pub spans: Vec<StageSpan>,
}

pub struct StageSpan {
    pub name: String,              // 稳定英文枚举名，展示文案另行映射
    pub parent: Option<usize>,
    pub start_us: u64,             // 相对 trace 起点的单调时钟偏移
    pub duration_us: Option<u64>,
    pub outcome: StageOutcome,
    pub cache: CacheState,         // Hit/Miss/Bypass/Unknown
    pub attempts: u32,
    pub attrs: BTreeMap<String, String>, // 白名单属性
}
```

通过 RAII `SpanGuard` 在成功、提前返回、错误、取消时统一闭合 span，避免十几个 `return Err` 漏记。耗时序列化为整数微秒，避免浮点误差。

接入点：

| 现有位置 | 新增埋点 |
|---|---|
| `gateway/http/handlers/pull.rs` | API 接受、排队、评估、下载 CacheHit、提交、Ready 总耗时 |
| `orchestration/deploy.rs::prepare_with_progress` | 评估、选择、节点配置主 span |
| `orchestration/scheduler.rs` | 候选排序、复用匹配、库存读取耗时 |
| `provider/orchestrator.rs::provision_resources` | create/start/describe、节点复用、失败换候选 |
| `provider/orchestrator.rs::wait_until_running` | 开机等待、轮询次数、每次 describe 耗时 |
| 云端引擎 bootstrap/启动逻辑 | SSH 建连、远端命令、进程 spawn、健康探活 |
| 隧道管理逻辑 | SSH 握手、端口转发、隧道首次可用 |
| 台账提交 | Ready commit 耗时 |

### 3.2 Span 分解

```text
deploy_total
├─ request_queue
├─ assess
│  ├─ model_metadata_cache
│  └─ resource_estimate
├─ select
│  ├─ inventory_snapshot
│  ├─ reuse_match
│  └─ candidate_sort
├─ configure
│  ├─ port_allocate
│  └─ remote_plan_render
├─ weights_prepare
│  ├─ remote_weight_stat       # 应命中缓存
│  └─ remote_download          # 预热场景应为 0/Skipped
├─ node_acquire
│  ├─ provider_start           # create_new / reuse_stopped
│  └─ wait_running
│     └─ provider_describe[N]
├─ engine_launch
│  ├─ ssh_connect
│  ├─ remote_spawn
│  └─ engine_health_wait
├─ tunnel_establish
│  ├─ ssh_handshake
│  └─ local_forward_ready
└─ ready_commit
```

`node_acquire` 在运行中节点复用时也应保留，只是耗时接近 0，便于统一比较。六阶段 UI 继续映射到这些 span，不改变用户已有认知。

### 3.3 记录内容

每条 trace 至少包含：

- 模型：`qwen2.5-0.5b-instruct:q4_k_m`、文件大小、量化格式；
- 节点：provider、P40 规格、节点模式、是否显式 `reuse_instance_id`；
- 缓存：评估缓存、规格库存快照、权重、引擎二进制、SSH 连接是否复用；
- 等待：各轮询次数、实际 sleep 总时长、健康检查次数；
- 结果：Ready/Failed/Cancelled、失败阶段、稳定错误码；
- 并行：SSH/引擎启动与隧道建立是否重叠；
- 成本相关：实例是否新建、是否仅停机复用，不记录价格哨兵和密钥。

### 3.4 存储、查询与报表

存储：

```text
~/Library/Application Support/mangosteen/traces/deploy-YYYY-MM-DD.jsonl
```

- 部署线程中只更新内存 trace；完成后由 worker 追加 JSONL。
- 单条完成 trace 一次追加，避免每个 span 一次磁盘写。
- 文件按天滚动，默认保留 7 天或 1000 条；内存仅保留最近 100 条摘要。
- 台账与 trace 分离：trace 丢失不得影响部署状态机。

查询接口（只读）：

```text
GET /api/deployments/{id}/trace
GET /api/traces?model=...&provider=...&node_mode=...&limit=...
GET /api/traces/summary?...            # p50/p95、成功率、失败 TopN
```

CLI：

```text
mangosteen trace show <deployment-id>
mangosteen trace list --model qwen2.5-0.5b --node-mode reuse_running
mangosteen trace report --last 30 --slo-ms 10000
```

UI 在部署完成后显示：`总耗时 6.8s`，详情页展示六阶段耗时与最慢子步骤；失败时直接显示失败阶段和等待次数。

报表同时输出：

- 端到端 `p50/p95/p99/max`；
- 每阶段 `p50/p95`、占总耗时比例；
- 固定 sleep/轮询耗时占比；
- cache hit/miss 分组；
- 按 A/B/C 场景分组；
- 失败率、失败阶段 TopN、回滚与资源泄漏检查。

## 4. 10 秒时间预算

### 4.1 A：运行中 P40 节点复用（主验收预算）

| 环节 | 目标耗时 | 优化后的动作 |
|---|---:|---|
| API 接受与排队 | ≤ 50ms | 部署请求立即进入独立 worker，不做云端同步查询 |
| 模型评估 | ≤ 100ms | 使用已缓存模型元数据与静态评估结果 |
| GPU 节点选择 | ≤ 100ms | 读取后台共享快照，显式复用节点直接命中 |
| 节点配置 | ≤ 100ms | 端口、路径、启动参数本地预计算 |
| 权重检查 | ≤ 500ms | 一次 SSH 会话完成文件存在性和大小校验 |
| SSH/远端进程启动 | ≤ 1.5s | 复用 SSH 连接，单次远端命令启动 llama-server |
| 引擎加载与健康就绪 | ≤ 5.0s | 小模型直接 GPU 加载，快速自适应探活 |
| 隧道建立 | ≤ 1.0s | 与引擎加载并行；复用 SSH 握手 |
| 台账 Ready 提交 | ≤ 50ms | 内存状态更新后落盘 |
| 调度余量 | ≥ 1.6s | 处理系统抖动 |

**预算合计不超过 10 秒，余量约 1.6 秒。** Ready 必须经过真实健康检查，余量不能通过提前上报获得。

### 4.2 B：停机 P40 节点复用

可压缩部分约 2.5～4 秒，给厂商开机和 SSH 可用留下约 5～6 秒：

| 环节 | 目标耗时 |
|---|---:|
| 本地评估/选择/配置 | ≤ 300ms |
| `start` API 返回 | ≤ 800ms |
| Stopped → Running + SSH 可用 | ≤ 5.0s（外部依赖） |
| 权重检查 + 引擎启动 + 探活 | ≤ 3.0s |
| 隧道与 Ready 提交 | ≤ 0.8s |

若实测厂商开机本身超过 6 秒，则该场景无法稳定满足 10 秒，应如实把结论归为“需要运行中热节点”，不能通过放宽松件或修改指标掩盖。

## 5. 关键优化设计

### 5.1 消除非关键路径远端调用

- 模型评估、GPU 选择、端口分配、远端路径和启动参数全部在本地完成。
- 节点 Running 后只做必要远端操作：权重校验、进程启动、健康检查。
- 不在部署路径执行 `llama-server --version`、引擎搜索、重复环境诊断；这些信息由安装时缓存。
- 合并 SSH 命令，避免“检查文件一次 SSH、创建目录一次、启动一次、查状态一次”的串行握手放大。

### 5.2 节点复用优先级

1. 用户显式选择的运行中实例；
2. 同模型/同节点可安全复用的运行中实例；
3. 保留权重和环境的停机实例；
4. 最后才创建新实例。

继续遵守共享节点保护：同机多模型按 `engine_port` 停进程，失败回滚不能影响同节点其它部署。性能优化绝不绕过台账唯一性和端口分配。

### 5.3 事件驱动的快速等待

当前 `wait_until_running` 使用固定 `poll_interval` 并串行 `describe`。改为有界自适应轮询：

- 首次立即查询，不先 sleep；
- 建议间隔 `200ms → 300ms → 500ms → 800ms`，上限 1s；
- 云 API 状态、SSH 可用、引擎 `/health` 分开计时；
- 超时预算不变，失败仍按现有错误码处理；
- 不用无限缩短轮询间隔，避免云 API 限流导致更慢。

引擎健康检查同理：进程 spawn 后立即短间隔探测，稳定后拉长；一次成功即返回，不额外固定等待。

### 5.4 SSH 与隧道优化

- 使用 SSH `ControlMaster/ControlPersist` 复用加密连接，避免多次 TCP + SSH 握手；密码场景仍遵守现有 `SSH_ASKPASS` 安全约束。
- 长连接由隧道管理模块持有，部署路径只申请通道，不重复认证。
- 节点 Running 且 SSH 可用后，**并行执行**远端引擎启动和本地端口转发建立。
- 最终 Ready 必须同时满足：远端引擎健康、隧道本地端口可连；两个条件缺一不可。
- 隧道 stderr 继续捕获，失败只清理自己的隧道与端口。

### 5.5 模型加载优化

- 固定 Qwen2.5-0.5B Q4_K_M 小模型作为基准，避免大模型下载/加载掩盖链路问题。
- 权重在节点 NVMe/本地磁盘预热；重复实验分为首次冷页缓存和后续热页缓存两组。
- GPU layers 按模型实际层数全部 offload；P40 24GB 对该模型充足。
- 保持默认上下文与线程配置，不在性能实验中同时改变推理质量参数。
- 以 `/health` 就绪为加载完成，而不是 `spawn` 成功；另记录进程启动到健康检查的纯加载耗时。

### 5.6 配置与缓存

```toml
[deploy_trace]
enabled = true
retain_days = 7
max_in_memory = 100

[deploy_performance]
metadata_cache_ttl_secs = 300
inventory_snapshot_ttl_secs = 5
provider_poll_initial_ms = 200
provider_poll_max_ms = 1000
engine_probe_initial_ms = 100
engine_probe_max_ms = 500
```

缓存只用于元数据和只读快照；实例是否存在、引擎是否健康仍实时核实。`Local` 不静默上云、配置驱动、网关唯一收口等基线不变。

## 6. Qwen2.5-0.5B + P40 验证方案

### 6.1 实验前置

- 优云智算选择 P40 或同价位可创建规格；库存仍以真实创建结果为准。
- 使用纯 CUDA 镜像，llama.cpp 与依赖已安装。
- 目标节点已存在指定 GGUF，校验文件大小/散列；下载阶段应 CacheHit。
- 不使用 11434/Ollama 链路，统一验证 llama-server 主链路。
- 本机代理对回环请求使用 `NO_PROXY=127.0.0.1,localhost`。

### 6.2 实验矩阵

| 组别 | 节点状态 | 权重/引擎 | 次数 | 目的 |
|---|---|---|---:|---|
| B0 | Running 复用 | 已缓存 | 30 | 优化前基线 |
| B1 | Stopped 复用 | 已缓存 | 20 | 拆出厂商开机成本 |
| B2 | 全新创建 | 镜像预热 | 10 | 明确不纳入 10 秒承诺的真实成本 |
| O1 | Running 复用 | 已缓存 | 30 | 合并 SSH + 快速探活后 |
| O2 | Running 复用 | 已缓存 | 30 | SSH 复用 + 并行隧道后 |
| O3 | Stopped 复用 | 已缓存 | 20 | 全部优化后验证 |
| R1 | Running 复用 | 已缓存 | 100 | 稳定性与 p95/p99 回归 |

实验顺序固定，先记录基线再逐项打开优化，避免多个变量同时改变。每轮保留原始 JSONL，不只保留汇总数字。

### 6.3 每轮必须检查

- `total_us` 和各 span 完整，无未闭合 span；
- `remote_download=Skipped/CacheHit`；
- Ready 后经本地隧道执行一次最小推理，证明服务真实可用；
- 失败/取消 trace 同样完整；
- 无重复实例、无残留引擎进程、无隧道和端口泄漏；
- 共享节点上的其它模型不受影响；
- trace 文件无密钥、密码和凭证 URL。

### 6.4 分析决策树

```text
总耗时 > 10s
├─ assess/select 高 → 是否同步远端调用？改共享快照/缓存
├─ node_acquire 高 → 是厂商 start/describe 还是固定 sleep？
├─ ssh_connect 高 → 是否重复握手？改 ControlMaster
├─ engine_health_wait 高 → 是模型加载还是探活间隔？分开看 spawn 与首次 200
├─ tunnel 高 → 是否串行等待？与引擎加载并行
└─ p50 达标但 p95 不达标 → 看云 API 抖动、页缓存、SSH 重试和限流
```

只优化报表证明的瓶颈，不以猜测提前改架构。

## 7. 实施阶段与验收门槛

### P0：只加观测，不改行为

- 实现 trace 类型、RAII span、内存记录、后台 JSONL。
- 覆盖六阶段及云 API/SSH/引擎/隧道子 span。
- 单测覆盖成功、失败、取消、嵌套、并行、未闭合保护与敏感字段白名单。
- 验收：全 workspace 测试通过，零警告；基线结果与埋点前部署耗时的差异 < 2%。

### P1：报表与基线

- 实现只读 API、CLI report 和部署详情耗时展示。
- 完成 B0/B1/B2，输出每阶段 p50/p95 和关键路径。
- 验收：能够明确指出 10 秒超支由哪一段造成，无“其它耗时”大黑洞。

### P2：低风险优化

- 去掉非必要远端调用、合并 SSH 命令、快速自适应探活。
- 复用 SSH 连接，并行隧道和引擎加载。
- 每打开一项优化即跑 O1/O2，对照 B0，不混合计算收益。

### P3：SLO 回归

- 完成 O3/R1，产出分场景报告。
- A 场景 p95 ≤ 10s；B 场景若受厂商开机限制，单独公布瓶颈与可行热节点策略。
- 失败回滚、共享节点保护、云端不落本地下载等现有测试必须继续通过。

## 8. 风险与不做事项

- **不通过提前 Ready 达标**：必须真实健康检查和隧道可用。
- **不混场景平均**：运行中复用、停机复用、新创建分别统计。
- **不牺牲资源安全换速度**：取消/失败仍回滚，共享节点不互杀。
- **不在请求线程做云端监控**：沿用后台快照，API 只读内存。
- **不把首次推理速度混入部署 SLO**：TTFB 另列。
- **不为 10 秒默认长期运行空闲热节点**：热节点池若后续需要，必须独立配置并明确持续计费；默认仍优先低价 P40 和停机保留实例。
- **不新增重型依赖**：计时和 JSONL 复用现有 serde/serde_json；不引入 metrics 平台、chrono、UUID。

## 9. 预期结论形式

最终报告不是一句“部署缩短到 X 秒”，而应给出：

1. A/B/C 三种场景优化前后 p50/p95、成功率；
2. 各环节耗时瀑布图及关键路径；
3. 每项优化独立收益与引入风险；
4. Qwen2.5-0.5B + P40 是否达到 10 秒，未达标时外部阻塞的原始证据；
5. 后续可推广到其他模型/节点的参数化预算，而非写死某个节点 ID。
