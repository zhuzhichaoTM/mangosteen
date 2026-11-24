# 通过 Strata 接入 Qwen3.8-Flash-Next 125B：mangosteen 架构扩展分析

**目标**：在现有 mangosteen 部署客户端上，通过新的推理引擎 **Strata** 支持 Qwen3.8-Flash-Next 125B（MoE，三级存储：显存专家缓存 + 内存专家池 + SSD 查找表），并把这条路径固化为「快速扩展新大模型/新推理框架」的标准模式。

**三条不可打破的约束**（来自需求）：
1. **CLI 与 UI 操作方式完全不变**：`mangosteen deploy <模型>` / `ui` 四个页签 / 设置页，用户感知不变。
2. **推理框架独立演进**：Strata（及未来的第三个引擎）的代码、版本、下载地址与主程序发布周期解耦，不随 app 重新编译。
3. **上层模型下载与调度完全复用**：远端直连下载（aria2c + `.download.ok` + SHA256）、模板系统、四维打分选点、资源评估、SSH 隧道、台账/生命周期全部复用，不新建一套下载/调度链路。

---

## 一、当前架构现状（与代码对齐）

### 1.1 分层（严格自上而下，`cargo test -p mangosteen-cli --test layering` 守护）

```
cli/webui (L1 交互)  →  gateway (L2 唯一收口)  →  orchestration (L3 编排调度)
   →  device (L4 设备执行: 引擎插件/远端运行时)  →  provider (L4 节点供应: GPU 实例 CRUD + 隧道)
   →  base/core/platform (L5 基础支撑)
```

核心机制（代码已读取确认）：
- **引擎插件化**：`mangosteen-device::EnginePlugin`（`launch_config()` / `probe()` / `precheck()`），注册表 `EngineRegistry::with_builtin(llama.cpp, Ollama)` 唯一登记点。编排层仅通过 `engines_registry.get(engine)` 分发，不写引擎专属 `if`。
- **部署流程**：`deploy` → `prepare_inner`（引擎可用性校验）→ `build_cloud_plan`（模板驱动 `ModelDeploySpec`）→ `prepare_cloud`（`configure_node`: 依赖安装 + `ensure_llama_server_from_archive`；`download_model_node`: aria2c 下载 + GGUF 魔数 + SHA256）→ `commit_cloud_inner`（`cloud_launch_config` 插件生成启动参数 → `start_node_engine`: `{workdir}/bin/llama-server` + `start_llama_server_with_key`）→ 隧道重建 → `wait_http_ready("/health")` → `Ready`。
- **模型下载**：完全在远端节点完成（不传本地文件），模板（`crates/core/template.rs`）声明仓库/文件/量化/辅助文件，下载器与本地 `models pull` 同源同规则。
- **调度**：资源评估（权重 + KV Cache + 计算缓冲 + 运行时开销）→ 选点约束 → 四维归一化打分 → `CloudPlan`（`engine: LocalPlan`、端口 8080..8191、本地隧道端口 11435-11999）。

### 1.2 与 llama.cpp 的硬耦合点（必须拆开才能接 Strata）

| 位置 | 硬编码内容 | 影响 |
|------|-----------|------|
| `core/config.rs` | `InferenceEngine` 闭合枚举（LlamaCpp/Ollama/Vllm/Sglang/TensorrtLlm），`required_format()`/`binary_name()`/`is_supported()` 全是 match | 新增 Strata 必须改枚举 + 5 个 match；Vllm/Sglang/TensorrtLlm 目前「选中即明确拒绝」 |
| `provider/orchestrator.rs` | `configure_node` → `ensure_llama_server_from_archive`；`start_node_engine` → `{workdir}/bin/llama-server` + `start_llama_server_with_key`（`--api-key-file`、`--ssl-key-file`、`.enc` 路径解密）；`start_service`/`stop_service`/`is_service_running`/`engine_ports_busy` 都直调 llama-server 路径与进程名 | 远端二进制准备、启停、探活全部写死了 llama-server；违反「provider 只负责节点供应，不跑模型」的 C7 层级守则精神 |
| `device/remote.rs` | `start_llama_server*`（pid 文件 `llama-server.pid` / `llama-server-<port>.pid`）、`llama_server_running[_on_port]`、`stop_llama_server[_by_model/_on_port]`、`.enc` shred | 进程命名、停止脚本、端口判据全是 llama-server 专属 |
| `orchestration/provision.rs` | `download_all_node_models` 的 `.enc` 路径标识 `["--model","-m","--mmproj","--model-draft"]` | Strata 启动参数形态不同（`strata launch --family qwen --model q2_0`），这里会误解密 |
| `scheduler.rs` `build_cloud_plan` | `LocalPlan::from_assessment` 产出 llama 语义（`--n-gpu-layers`/`--cache-type-k/v`/`--flash-attn`）；`engine_port` 固定 `LLAMA_CPP_PORT` (8080) | Strata 不认 llama.cpp 旗号；端口基址应可按引擎配置 |
| `lifecycle.rs` | `unload`/`stop`/`repair` 路径引用 `llama-server` 进程名与 pid 文件 | 停止/恢复路径需按引擎分发 |
| 资源评估 | `cloud_vram_requirement` 按 llama 权重+KV 公式 | 125B MoE 全量权重约百 GB，Strata 靠三级存储只需 12-24 GB 显存，llama 公式会算出「跑不了」而拒绝部署 |
| 密钥/协议 | API Key 写远端 600 文件 + `--api-key-file`；TLS `--ssl-*`；健康检查 `/health` | Strata 官方做法是 `API_KEY` 环境变量/CLI，OpenAI 兼容 API (`/v1/*`)，不提供 llama 风格 TLS 标志；健康点应改为 `/v1/models` |

### 1.3 Strata 引擎特征（需适配的差异）

- 引擎本身：MoE 推理运行时，GGUF 格式，OpenAI + Anthropic 兼容 API，默认监听 `127.0.0.1:8080`。
- 启动方式：`strata launch --family qwen --model q2_0 --yes --no-tui`（自身做 daemon 管理），配套 `--status` / `--stop`；Docker 镜像 `ghcr.io/niko1221/strata:0.2.7-cuda13` 等。
- 资源：CUDA 卡，建议 32-64 GB 内存，磁盘 66-274 GB（按量化/家族），显存需求远小于模型总参数（专家热缓存）。
- 密钥：`API_KEY` 环境变量，而非 `--api-key-file` 文件。

---

## 二、架构调整方案（文件级改动点）

核心思路：**把 EnginePlugin 从「只生成启动参数」扩展为「完整的远端引擎生命周期 SPI」**，provider 层只调 device 暴露的引擎操作，不再直接碰任何 llama-server 文件/进程名；下载、评估、调度、隧道、台账、网关全部不动。

### 2.1 Phase 0：引擎能力元数据（core/config.rs）

在 `InferenceEngine` 增加 `Strata` 变体，并新增一张能力表（一次性改动，之后新增框架只加变体 + 一行能力）：

```rust
pub enum InferenceEngine { LlamaCpp, Ollama, Strata, Vllm, Sglang, TensorrtLlm }

// 新增：每个引擎的「形态差异」集中在这里，下游不再散落 match
pub struct EngineCapabilities {
    pub id: &'static str,            // "strata"
    pub required_format: &'static str, // "gguf"（Strata 也是 GGUF，下载链路可完全复用）
    pub binary_name: &'static str,   // "strata"
    pub health_path: &'static str,   // "/v1/models"
    pub default_port: u16,           // 8080
    pub supports_tls: bool,          // false（SSH 隧道已加密，Strata 不需 --ssl-*）
    pub api_key_mode: ApiKeyMode,    // Env 或 File
    pub model_path_flags: &'static [&'static str], // ["--model"] 供 .enc 解密识别
}
```

注册表保持唯一：`EngineRegistry::with_builtin(llama, ollama, strata)`（`crates/device/src/plugin.rs`）。层级守护测试 C3（`with_builtin` 仅在 device 定义；编排层经 `engines_registry.get(` 分发）继续满足。

### 2.2 Phase 1：远端引擎生命周期插件化（crates/device）

`EnginePlugin` 扩展为完整 SPI（默认实现 = 现有 llama 语义，现有行为不迁移即不变）：

```rust
pub trait EnginePlugin: Debug + Send + Sync {
    fn engine(&self) -> InferenceEngine;
    fn capabilities(&self) -> EngineCapabilities;
    fn launch_config(&self, plan: &LocalPlan, bind_host: &str, port: u16) -> EngineLaunchConfig;
    fn probe(&self, plan: &LocalPlan) -> bool;                 // 用 capabilities.health_path
    fn precheck(&self) -> MnResult<()> { Ok(()) }
    // 新增：远端生命周期（均经 SSH 执行，provider/编排不碰进程细节）
    fn ensure_engine(&self, target: &SshTarget, workdir: &str, url: &str) -> MnResult<String>; // 返回二进制绝对路径
    fn start(&self, target, workdir, bin, args, env) -> MnResult<()>;   // setsid/nohup 或 strata launch --no-tui
    fn stop(&self, target, workdir) -> MnResult<()>;
    fn is_running(&self, target, workdir, port) -> MnResult<bool>;
}
```

`LlamaCppEngine` 实现新增方法（把 `device/remote.rs` 里的 `start_llama_server_with_key`/`llama_server_running_on_port`/`stop_llama_server*` 逻辑按引擎封装进去）；新增 `StrataEngine` 实现：
- `ensure_engine`: 下载 Strata 发行包（URL 来自 `[engines.strata].url`，配置化，独立于 app 版本）；或复用实例上的已有二进制。
- `start`: `strata launch --family <family> --model <model> --yes --no-tui` + `API_KEY` 环境变量（不进 argv、不进日志）；或直接启动其服务二进制。
- `stop/is_running`: 用 `strata launch --status`/`--stop` 语义，或按端口/pid 文件。
- `probe`: `GET /v1/models`（带 Bearer Key）。

`device/remote.rs` 的 llama_* 自由函数改为按引擎分发的通用函数（`start_engine_server` / `engine_running_on_port` / `stop_engine_by_model` / `stop_engine_on_port`），内部交给对应 `EnginePlugin`；现有 llama 行为作为默认实现，保证回归兼容。

### 2.3 Phase 2：provider 层去 llama 硬编码（crates/provider/src/orchestrator.rs）

- `configure_node(target, workdir, engine, url, sha256)`：按 engine 调用 `plugin.ensure_engine(...)`；不再硬编码 `ensure_llama_server_from_archive`。
- `start_node_engine` / `start_service`：调用 `plugin.start(...)`，传入 `launch_config.args` 与密钥 env；pid/log 命名由引擎能力决定（不再是 `llama-server.pid`）。
- `stop_service` / `stop_service_on_port` / `is_service_running` / `engine_ports_busy`：交给 `plugin.stop` / `plugin.is_running`（按端口集合判断，端口占用制不变）。
- TLS：仅 `capabilities.supports_tls == true` 时追加 `--ssl-key-file/--ssl-cert-file`；Strata 走纯 HTTP + SSH 隧道，网关转发不变。

### 2.4 Phase 3：编排层（orchestration）与评估适配

- **引擎解析**（`deploy.rs` `prepare_inner`）：不再全局强制 `config().local.engine`。解析顺序：模板/模型注册声明的 `engine` > 请求显式指定 > `config().local.engine`（默认 llama.cpp）。这样同一实例上可共存 llama.cpp 模型与 Strata 模型（端口已按 `engine_port` 隔离，`allocate_remote_engine_port` 保持不变）。
- **启动参数**：`build_cloud_plan` 产出 `LocalPlan` 后，由对应插件的 `launch_config` 解释；Strata 插件忽略 `gpu_layers`/`cache-type`/`flash-attn` 等 llama 语义，使用模板透传的 `extra_args` 与 engine-specific 字段（family/model/quant）。
- **资源评估**：`cloud_vram_requirement` 保持 llama 公式作为默认；`EngineCapabilities` 新增 `vram_estimate(&spec) -> Option<u64>` 钩子，Strata 返回按专家缓存 + KV + 激活估算的显存（12-24 GB 级），避免把 125B 全量权重当作显存需求导致调度拒绝。内存/磁盘下限由模板/能力表覆写（Strata: RAM ≥ 32-64 GB, disk ≥ 80 GB）。
- **端口**：`LLAMA_CPP_PORT..=8191` 改为按 `capabilities.default_port` 起始的段，Strata 默认 8080，与现有行为一致。

### 2.5 Phase 4：模型与模板（Qwen3.8-Flash-Next 125B）

在 `crates/core/template.rs` 注册 `qwen3.8-flash-next-125b` schema（模型维度由系统控制，随版本发布；模板内容由用户在设置页自定义，不改代码）：

```text
template: qwen3.8-flash-next-125b
engine: strata
family: qwen
model: q2_0            # 可在设置页改为 IQ2_XS / IQ3_XXS / IQ3_S 等
repository: ...        # Strata 官方仓 / ModelScope / HF 镜像
download_url: ...
context: 8192
batch: ...
aux_files: [mtp 草稿层, ...]  # Strata 自带 MTP，可选下载
min_vram_bytes: ...   # 覆写评估默认值
min_memory_bytes: ... # ≥ 32 GB
min_disk_bytes: ...   # ≥ 80 GB
```

- 下载链路完全复用：GGUF 文件，aria2c + `.download.ok` + SHA256，走 `download_model_node`，不改一行下载代码。
- `ModelSpec.quant`：Strata 量化名（Q2_0/IQ2_XS…）与 llama.cpp 的 `Q4_K_M` 体系不同，建议由模板原样透传到 `extra_args` / `engine-specific` 字段，评估层按 `file_size_bytes` 估算权重，不强制套 llama 量化枚举。
- `.enc` 静态解密：路径标识由 `capabilities.model_path_flags()` 提供（Strata: `["--model"]`），启动前解密逻辑复用。

### 2.6 Phase 5：网关/UI/CLI 零改动验证

- **网关**：`/v1/*` OpenAI 兼容 relay 不动（Strata 原生 OpenAI + Anthropic 兼容）；健康检查路径由插件 `health_path()` 提供（`/v1/models`）；API Key 转发 `Authorization: Bearer` 不变。
- **UI**：设置页引擎下拉来自 `InferenceEngine::all()` + `is_supported()`，Strata 注册后自动出现；部署/对话/设备页签不改。
- **CLI**：`mangosteen deploy qwen3.8-flash-next-125b --context 8192` 完全不改；引擎由模板自动解析为 strata；可选（不强制）追加 `--engine strata` 覆盖，既有行为不受影响。

---

## 三、什么不变（证明三约束已满足）

| 层 | 复用项 | 改动 |
|----|--------|------|
| CLI/UI | `deploy/run/stop/ps/status/ui/doctor/config` 命令；四页签 Web 控制台；单文件内嵌前端 | 无改动，或仅在设置页引擎下拉多一项 |
| 模型下载 | 远端直连下载（aria2c，`.download.ok` + GGUF 魔数 + SHA256）；模板 `ModelDeploySpec`；`ModelSource`（modelscope/hf） | 无改动；Strata 用 GGUF，模板声明 URL 即可 |
| 调度 | 资源评估（默认公式保留）、四维打分选点、降级轨迹、`CloudPlan` 契约、SSH 免密/隧道、台账 | 无改动；仅在评估层新增引擎可选钩子 |
| 网关 | 唯一收口、鉴权、OpenAI 兼容协议、SSE 流式转发、API Key Bearer 转发 | 无改动 |
| 运维 | 台账 `state.json`、停止即关机止费、失败保留实例、端口占用制关机判据 | 无改动；停止脚本按引擎插件分发 |

---

## 四、推荐实施顺序与验收

1. **Phase 0**：`InferenceEngine::Strata` + `EngineCapabilities` 表（core/config.rs）。
2. **Phase 1**：`EnginePlugin` 扩展生命周期方法；`LlamaCppEngine` 迁入现有 llama 逻辑；新增 `StrataEngine`（crates/device/src/strata.rs）。此时跑满 device crate 单测。
3. **Phase 2**：`provider/orchestrator.rs` 去 llama 硬编码，全部改为调 device 按引擎分发的操作（保持 C7：provider 不再定义任何 `ensure/start/stop_llama_server*`）。
4. **Phase 3**：`build_cloud_plan` 按引擎能力设端口/参数语义；`prepare_inner` 按模板解析引擎；评估层新增 `vram_estimate` 钩子。
5. **Phase 4**：注册 `qwen3.8-flash-next-125b` 模板；端到端联调：`mangosteen deploy qwen3.8-flash-next-125b` → 节点创建/配置 → 下载 strata 二进制与模型 → 启动 → `/v1/models` → Ready → 对话。
6. **测试**：更新 layering 守护测试（`with_builtin` register 次数 2→3；provider 无 llama 函数定义）；新增 strata 插件单测（launch_config/ probe / start/stop）；回归现有 llama.cpp 与 Ollama 部署。

**关键风险**：Strata CLI 的实际参数语义（家族名 vs 模型路径、daemon 与子进程混合、API Key 注入方式）需以真实 strata 二进制行为为准修正 `StrataEngine`；若 Strata 仅支持桌面/本机无头受限，云端实例链路可走其官方 Docker 镜像（`ghcr.io/niko1221/strata`）， provider 的节点供应与镜像选择逻辑无需改动。

---

> 一句话结论：本次扩展的核心不是「加一个模型」，而是把 `device` 里的远端引擎生命周期（准备二进制、启停、探活、密钥/端口/健康检查）从 llama-server 硬编码提升为 `EnginePlugin` 插件 SPI；Strata 作为一个插件独立演进，而下载、调度、网关、UI、CLI 上层全部复用，用户零感知。
