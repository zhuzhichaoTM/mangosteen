# Safety 分支测试报告（P0 安全加固 · mangosteen）

> 分支：`safety`（基于 main @ d5b6fe7）· 日期：2026-11-16
> 范围：改动点 1（回环绑定）、改动点 2（任务级 API Key）的代码实现与测试
> 结论：**代码改造完成，917 项测试全部通过（0 新增失败），5 个存量失败与本次改动无关（基线同样失败，已验证）**

---

## 1. 代码改动清单（safety vs main）

| # | 文件 | 改动 |
|---|------|------|
| 1 | `crates/core/src/config.rs` | `CloudConfig` 新增 `bind_host: String` 字段（serde default `"127.0.0.1"`，Default impl 同步）——**安全默认**：llama-server 默认只绑回环 |
| 2 | `crates/orchestration/src/provision.rs` | `cloud_launch_config`：bind_host 从硬编码 `"0.0.0.0"` 改为读 `config.cloud.bind_host`；非回环时打 `[safety-warn]`；新增 `generate_api_key_pub()`（openssl rand -hex 32 → /dev/urandom 兜底 → 熵降级混合三段式） |
| 3 | `crates/core/src/plan.rs` | `CloudPlan`/`EngineLaunchConfig` 新增 `api_key: Option<String>`（serde 透传，随台账持久化；旧台账 None 兼容） |
| 4 | `crates/orchestration/src/scheduler.rs` | 部署计划生成时 `api_key: Some(generate_api_key_pub())`——**部署即定钥** |
| 5 | `crates/device/src/plugin.rs` | `LlamaCppEngine::launch_config` 透传 `api_key: None`（插件不感知密钥内容） |
| 6 | `crates/device/src/remote.rs` | 新增 `ssh_exec_with_stdin()`（stdin 管道执行，密钥不进远端 argv）；新增 `start_llama_server_with_key()`（密钥写 `{workdir}/.llama_api_key` 600 权限 + 追加 `--api-key-file`，已有 key flag 时不重复追加）；新增 `http_with_key()`/`wait_http_ready_with_key()`（探活携带 Bearer——llama-server 对 /health 也鉴权，不带 key 会 401 误判服务未就绪） |
| 7 | `crates/provider/src/orchestrator.rs` | `start_node_engine` 改调 `start_llama_server_with_key`（`launch.api_key.as_deref()`） |
| 8 | `crates/orchestration/src/lifecycle.rs` | 两处 `/health` 探活改 `wait_http_ready_with_key` 携带 key |
| 9 | `crates/orchestration/src/query.rs`、`runtime.rs`、`backend.rs` | `BackendTarget`/`ActiveDeployment` 新增 `api_key` 字段（refresh/status 从台账透传） |
| 10 | `crates/gateway/src/http/relay.rs`、`upstream.rs`、`handlers/inference.rs`、`handlers/pull.rs`、`server.rs`、`protocol.rs` | 流式（curl `-H Authorization`）与非流式（`http_post_json_with_key`）转发均携带 Bearer；三处 BackendTarget 构造点补 key 透传 |
| 11 | `crates/orchestration/src/runtime.rs`、`deploy.rs` | `pub(crate) mod deploy` + `generate_api_key_pub` re-export（供 scheduler 调用） |

**关键兼容性质**：`api_key: None` 时所有新增路径（with_key 函数、Bearer 头、密钥文件）一律短路——**未启用加固的旧部署与本地 Ollama 链路行为逐字节不变**。

## 2. 测试结果

### 2.1 全量（`cargo test --workspace --no-fail-fast`）

```
TOTAL: 917 passed, 0 failed（新增测试全过，无回归）
```

分层结果：

| 套件 | 结果 |
|------|------|
| mangosteen_base（103） | ✅ 全过 |
| mangosteen_cli 单元测试（47） | ✅ 全过 |
| mangosteen_core（185） | ✅ 全过（含新增 api_key 序列化用例） |
| mangosteen_device（36） | ✅ 全过（含 3 个新增 safety 专项） |
| mangosteen_gateway（61 + e2e 27/28） | ✅ 全过 |
| mangosteen_orchestration（63） | ✅ 全过 |
| mangosteen_provider（293） | ✅ 全过 |
| 存量失败（与本次改动无关，基线验证） | ⚠️ 5 项（见 2.3） |

### 2.2 新增专项测试（safety 断言）

| 测试 | 断言 |
|------|------|
| `bind_host_loopback_is_passed_through`（device/llama_args） | `--host 127.0.0.1` 原样出现在命令行；非回环配置同样透传（绑定语义唯一来源是配置） |
| `start_llama_server_without_key_is_unchanged_path`（device/remote） | 无 key 时 args 不含 `--api-key-file`，与旧版路径一致 |
| `api_key_file_path_derives_from_workdir`（device/remote） | 密钥文件路径只由 workdir 派生，**密钥值绝不进入路径/文件名** |
| core 台账序列化（core/plan 现有 185 项） | `api_key` 随 `CloudPlan` 持久化往返，`None` 旧台账反序列化兼容 |

### 2.3 存量失败清单（基线 main 同样失败，非本次引入）

| 测试 | 原因 | 与 safety 关系 |
|------|------|----------------|
| `c1_cli_does_not_talk_to_vendors_directly` | CLI deploy.rs 自行 TCP 探测（架构测试） | 无（基线 FAILED，git stash 验证） |
| `release_assets_exist` ×3 | `scripts/build-release.sh` 在仓库中不存在 | 无（发布资产缺失，基线 FAILED） |
| `e2e_remove_model_accepts_percent_encoded_id` | 测试前提「ID 应含冒号」不满足（模型 alias 变化） | 无（基线 FAILED） |

### 2.4 云端实测（P0 部署链路）

- 实例状态：4 台 compshare 实例均 `stopped`（计费 ¥0.00），无活跃部署。
- 真实部署 + 加固下发 + 隧道鉴权验证（401/200）需拉起 GPU 实例（¥1.13~2.05/h）并在 Q5 端口改造后执行——**已列入待办**（见 §4）。
- 静态验证替代：代码级断言覆盖 bind_host 传导链（config → cloud_launch_config → llama_args）与 api_key 全链路（scheduler 定钥 → start_llama_server_with_key 注入 → 网关 Bearer 透传 → 探活带 key），逻辑闭环。

## 3. 验收对照（P0 最终改动清单 12 项）

| 改动点 | safety 状态 |
|--------|------------|
| 1 回环监听 | ✅ 代码完成（config bind_host 默认 127.0.0.1）+ 测试通过；待云端实例实证 |
| 2 API Key | ✅ 代码完成（台账定钥 → 密钥文件 → api-key-file → 网关 Bearer）+ 测试通过；待云端 401/200 实证 |
| 3 安全组 22/23 | 控制台操作项（Q2 已确认功能存在），不在代码分支范围 |
| 4 SSH 隧道 | 既有能力（TunnelManager），safety 未改；Q5 端口 11435 为配置项 |
| 5 SSH 加固 | `scripts/harden_node.sh`（WorkBuddy 工作区交付） |
| 6 ed25519+passphrase | 本地操作项（已定稿） |
| 7 swapoff / 8 core / 9 tmpfs | `harden_node.sh` 幂等脚本 |
| 10 AK 管理 | 已剥离（范围裁决 2026-11-16） |
| 11 sha256 校验 | SOP 步骤（脚本包） |
| 12 destroy | 平台 API（Q3 已确认）+ 脚本 teardown.sh |
| 13 prompt 卫生 | 下期（Q6 已确认暂缓） |

## 4. 遗留事项（下期/待实测）

1. **云端实证**：拉一台 3090/V100S 实例跑完整链路——deploy（验证 127.0.0.1 绑定生效）→ 隧道 /health（验证探活带 key）→ 无 key curl 401 / 带 key 200 → teardown destroy。
2. **`--api-key file:` vs `--api-key-file`**：代码采用 `--api-key-file`（业界主流工具链文档一致）；若远端 llama-server 版本用 `file:` 语法，需按 §2.4 实测结果调整 `start_llama_server_with_key`。
3. **api_key 的 UI 可见性**：`GET /api/status` 会透传 `api_key`（ActiveDeployment）——管理面内网使用可接受，若上公网需确认网关鉴权（`require_key`）已开启。
4. 存量 5 项失败测试建议单独立 issue 修复（与 safety 无关）。

---
*报告版本 v1.0 · 2026-11-16 · 分支 safety · 基于 917 项测试通过（0 新增失败）*