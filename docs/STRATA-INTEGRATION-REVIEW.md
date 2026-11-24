# Strata 集成评审与实施方案（mangosteen 新增推理框架后端）

> 评审日期：2026-11-22 · 评审对象：`mangosteen` safety 分支未提交 WIP（Strata 插件化）+ `strata-deploy/deploy-qwen-v100s-64k.sh`
> 评审结论：**骨架已就位，但有 7 个 P0 级阻断问题，不能直接端到端部署。** 实施按 §5 三阶段推进。

## 1. 现状（WIP 已完成）

- `InferenceEngine::Strata` + `EngineCapabilities` + `ApiKeyMode`（core/config.rs）
- `StrataEngine` 插件：`launch_config` / `probe` / `ensure_engine` / `start` / `stop` / `is_running`（device/strata.rs）
- provider `configure_node` / `start_node_engine` 按引擎分发（provider/orchestrator.rs）
- provision.rs：Strata 跳过预下载、跳过 VRAM 预检、健康路径走 capabilities
- scheduler.rs：模板显式 `engine: strata`、Strata 资源估算分支、端口按 capabilities
- template.rs：`qwen3.8-flash-next-125b` 模板（engine: strata，双分片，extra_args）
- UI/CLI：doctor 引擎检查、设置页引擎下拉、layering 测试 2→3

## 2. 评审发现（按严重度）

### P0（阻断部署）

| # | 问题 | 证据 | 影响 |
|---|------|------|------|
| P0-1 | `StrataEngine::strata_args` **不传 `--port`**，而 README/deploy script 明确支持 `--port` | strata.rs `let _ = (bind_host, port)`；deploy-qwen-v100s-64k.sh `--port "$PORT"` | mangosteen 为每个部署分配 8080..8191，Strata 永远监听 8080 → 第二个部署隧道连不上，端口冲突 |
| P0-2 | `configure_node` 对 llama 传 `""` 作 sha256 | provider/orchestrator.rs `ensure_llama_server_from_archive(..., "")` | P0 供应链校验（default_llama_server_sha256）被静默取消 |
| P0-3 | `start_service` / `stop_service_on_port` / `is_service_running`（provider）**硬编码 llama-server** | provider/orchestrator.rs:924-960 | 云实例上 Strata 的重启/恢复/停止链路全部失效；`stop_service_on_port` 无法杀 Strata 进程 → 节点不关机持续计费 |
| P0-4 | `finish_start` 与 lifecycle restore **硬编码 `/health`**；runprobe **硬编码 `/health`** | lifecycle.rs:1707；runprobe.rs | 若 Strata 健康路径选 `/v1/models`，探活永远失败；当前 capabilities 选 `/v1/models`，runprobe `/health` 不一致 |
| P0-5 | provision TLS 探测不看 `supports_tls` | provision.rs:1116 | Strata 节点若残留 llama 证书 → tls=true → https 探活 → Strata 不支持 TLS → 失败 |
| P0-6 | `strata launch` 内下载 76-84GB 与 `launch_timeout 600s` 不匹配 | provision.rs wait_http_ready 600s | 首次部署必然超时失败 |
| P0-7 | Strata 健康路径应选 `/health`（免密钥），与 README/deploy script 一致 | strata.rs probe 用 `/v1/models` + Bearer | `/v1/models` 需 Bearer，runprobe 无 key → UI 显示全部 "未运行" |

### P1（设计/健壮性）

| # | 问题 | 建议 |
|---|------|------|
| P1-1 | pid/log 文件名 `{workdir}/strata.pid` 不带端口，多 Strata 部署互相覆盖 | 改为 `strata-{port}.pid` / `strata-{port}.log` |
| P1-2 | `stop` 调 `strata launch --stop` 会停掉节点上**所有** Strata 实例 | 优先 pid 文件 port-scoped kill，`--stop` 仅作为兜底 |
| P1-3 | `is_running` 回落 8080 硬编码 | 去掉回落，用端口探针 |
| P1-4 | Strata 模型下载完全交给引擎，mangosteen 的 aria2c/SHA256/镜像/进度链路全部失效 | 切换为 mangosteen 预下载 + `--data-dir {wd}`（Strata 已实测复用 `{wd}/models/<quant>/`），保留 `--model-path` 作为挂载选项 |
| P1-5 | `HF_ENDPOINT` / `STRATA_FEED` 从未注入 | start() 注入 HF_ENDPOINT（镜像），ensure_engine 传 feed |
| P1-6 | 资源估算 `engine == "strata"` 字符串比较 + 硬编码 24GB/64GB/2.0元 | 下沉到 `EngineCapabilities` + 模板字段（min_vram_bytes 等），scheduler 不写引擎分支 |
| P1-7 | 磁盘下限 80GB < IQ3_S 实际 83.6GB | 改为 `file_size * 1.1 + 20GB`，并加模板字段 |
| P1-8 | `strata_feed` 配置字段死代码 | wire into ensure_engine |
| P1-9 | `builtin_plugin` 只覆盖 Strata，llama 走专用路径 → 两套真相 | 统一为 `OnceLock<EngineRegistry>`（llama 用 const 构造） |
| P1-10 | `.enc` 解密 `path_flags` 硬编码数组，不读 capabilities | 改用 `capabilities.model_path_flags` |
| P1-11 | `stop_keep_ciphertext` + `shred_encrypted_models` 对 plaintext 模型是死代码 | deprecate（保留字段兼容），strata 不调用 |
| P1-12 | Gateway `/v1/messages` 用 `/v1` 前缀，Strata Anthropic 走根路径 | v1 仅支持 OpenAI；Anthropic 兼容标记 v2 |
| P1-13 | Gateway 重写 `model` 为台账 ID，Strata 期望 `"strata"` | `EngineCapabilities.upstream_model_name` + BackendTarget 字段 |

### P2（清理）

- `strata_api_key_env` 死代码
- `~` 目录残留（`mangosteen/~`）
- 注释里 `.enc` 提及

## 3. 关键决策点（请确认）

D1 **下载策略**：推荐 mangosteen 预下载（aria2c + SHA256 + 镜像 + 断点续传）+ `--data-dir {wd}` 复用；引擎自下载作为 `strata_model_source=engine` 兜底。理由：不重复磁盘（用户明确要求），且 supply-chain 校验不丢失。

D2 **健康路径**：Strata 用 `/health`（免密钥，README 已确认），与 runprobe/lifecycle/gateway 一致，避免 Bearer 传递复杂度。

D3 **模型名**：Gateway 为 Strata 发送 `"strata"`（README 固定值）。

D4 **VRAM 下限**：24GB → 12GB（Strata 官方 RTX 3060 12GB 可跑），内存 64GB → 32GB（Q2_0/Coder 最低），磁盘 80GB → 100GB。

## 4. 实施文件清单

| 文件 | 改动 |
|------|------|
| crates/core/src/config.rs | EngineCapabilities 新增字段；strata health_path=/health；min_vram/mem/disk；upstream_model_name；cli_model_flag；launch_timeout_secs；strata_model_source config |
| crates/device/src/strata.rs | --port/--access/--parallel/--data-dir；pid per-port；stop scoped；is_running port-based；HF_ENDPOINT env；probe /health |
| crates/device/src/plugin.rs | builtin_plugin 统一（OnceLock）；trait 扩展 ensure_engine 上下文 |
| crates/provider/src/orchestrator.rs | configure_node EngineInstallContext（含 sha256）；start_service/stop_service_on_port/is_service_running 引擎分发；engine_urls 用 feed |
| crates/orchestration/src/scheduler.rs | capabilities 驱动资源；template price cap → 模板字段；engine 字符串比较 → parsed_engine |
| crates/orchestration/src/provision.rs | TLS 按 capabilities；health path 用 capabilities；long budget for strata |
| crates/orchestration/src/deploy.rs | 模板引擎优先于 config |
| crates/orchestration/src/lifecycle.rs | health path from capabilities；ENGINE_PORTS from capabilities；stop 按引擎 |
| crates/gateway/src/runprobe.rs | health path from record engine |
| crates/gateway/src/protocol.rs | upstream_model_name rewrite |
| crates/gateway/src/orchestration/src/query.rs | BackendTarget.upstream_model |
| crates/core/src/template.rs | min_vram_bytes 等字段 + extra_args（--parallel/--port?） |

## 5. 开发与验证计划

### Phase A — 先评审（本文件）

### Phase B — P0 修复（阻断）
1. `--port` + `--access` 透传（tests 更新）
2. sha256 回归修复
3. start/stop/is_running 引擎分发
4. TLS gating
5. health path /health 统一
6. runprobe engine-aware
7. gateway upstream_model_name
8. launch budget per engine

### Phase C — P1 修复 + 验证
9. pid per-port
10. stop scoped
11. HF_ENDPOINT / STRATA_FEED
12. capabilities-driven resources
13. 模型名 upstream
14. `cargo build` + `cargo test` 全绿
15. 新增 strata 插件单测：port/access/pid naming
16. 端到端（真机）：`mangosteen deploy qwen3.8-flash-next-125b` → /health → /v1/chat/completions with model="strata" → 200

### 验收矩阵
- `cargo test -p mangosteen-device` 全绿（含 strata_*）
- `cargo test -p mangosteen-core -p mangosteen-cli` 全绿
- `cargo build --release` 无 error
- strata 插件 launch_config 包含 `--port` 与 `--access`
- gateway runprobe 对 strata 部署显示 Ready
- 二次部署 disk < 1.2 × 模型大小（无重复下载）

## 6. 风险与开放问题

- R1: Strata 的 `--model-path` 对双分片 GGUF 的支持未验证（需真机）
- R2: Strata `--data-dir` 复用布局是否与 mangosteen 下载路径一致（实测注释说一致，需真机确认）
- R3: `strata launch` 是否在启动时自行 daemonize（影响 pid 文件正确性）
- R4: Strata `--api-key` env 是否被 daemon 继承（影响鉴权）
- R5: 驱动版本（V100S 需 ≥575，CUDA 13 需 ≥580）需在 precheck 中 fail-fast

## 7. 不做（明确排除）

- 不重新加密模型（用户明确；encryption 已取消）
- 不按 llama.cpp 的 `--n-gpu-layers`/`--flash-attn` 等旗号传给 Strata（Strata 自管）
- 不新建第二套下载/调度链路（完全复用 mangosteen）
- 不改 CLI/UI 操作方式
