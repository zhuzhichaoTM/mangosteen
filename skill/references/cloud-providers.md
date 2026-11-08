# 云服务商配置指南

## 内置服务商

### 优云智算（UCloud RPC）

当前唯一内置、已验证的 GPU 供应商。

**特点**：
- UCloud RPC 形态接口（与常见 REST 不同）
- 按量计费，关机不收费
- 支持 RTX 4090 / 5090

**配置示例**：

```toml
[providers.compshare]
driver = "compshare"
enabled = true
display_name = "优云智算"
secret = { env = "COMPSHARE_PRIVATE_KEY" }

[providers.compshare.options]
public_key = "your-accesskey-id"
region = "cn-wlcb"
zone = "cn-wlcb-01"
image_id = "compshareImage-xxxx"   # 必填：从优云智算控制台「镜像」页复制
charge_type = "Postpay"            # 按量计费：关机不收费
gpu_types = "4090,5090"
max_gpu_count = "1"
```

**为什么优云智算要单独一个 driver**：

它的开放接口是 UCloud RPC 形态，与其它服务商的 REST 形态有四处根本差异：

| 维度 | 常见 REST 服务商 | 优云智算 |
|------|------------------|----------|
| 端点 | 每个动作一个路径 | 全部 `POST https://api.compshare.cn` |
| 动作区分 | HTTP 方法 + 路径 | 表单参数 `Action=...` |
| 认证 | 请求头 Token | 参数 `PublicKey` + `Signature`（SHA1 签名） |
| 成功判定 | HTTP 2xx | HTTP 2xx **且** `RetCode == 0` |

签名无法用 profile 的字段映射表达，所以由 `mangosteen-provider/src/compshare` 提供专用驱动。

驱动替你处理了厂商的三个硬限制：
- **创建前强制查库存**
- **销毁前必须先关机**
- **状态并发锁（10 分钟 TTL）**

上层调用方式与其他服务商完全一致。

## 通用 REST 服务商配置

对于支持标准 REST API 的云服务商，可以通过配置 profile 动态接入，无需改代码。

### 配置模板

```toml
[providers.mycloud]
driver = "mycloud"
enabled = true
display_name = "我的算力平台"
secret = { env = "MYCLOUD_TOKEN" }

[providers.mycloud.profile]
name = "mycloud"
display_name = "我的算力平台"

[providers.mycloud.profile.endpoints]
base_url = "https://api.mycloud.example"
list_specs = "/v1/gpu/specs"
create_instance = "/v1/instances"
destroy_instance = "/v1/instances/{id}"
get_instance = "/v1/instances/{id}"
list_instances = "/v1/instances"

[providers.mycloud.profile.mappings]
# 字段映射：将厂商字段映射到统一模型
spec_id = "id"
gpu_type = "gpu_type"
vram_gb = "vram"
price_per_hour = "price"
memory_gb = "memory"
disk_gb = "disk"
region = "region"
zone = "zone"
```

### 凭证配置方式

支持三种方式（都不会在 `config show` 里明文输出）：

```toml
# 方式 1：从环境变量读（推荐）
secret = { env = "MY_TOKEN" }

# 方式 2：从文件读
secret = { file = "/run/secrets/tok" }

# 方式 3：直接内联（仅调试用）
secret = { inline = "xxx" }
```

## 自定义模型源

### 内置源

- ModelScope（国内快，默认）
- HuggingFace

### 添加自定义源

要接内网私有仓库，只需在配置里加一段：

```toml
[model_sources.myhub]
name = "myhub"
kind = "Custom"
base_url = "https://models.corp.internal"
default_revision = "main"
url_template = "{base}/repo/{repo}/raw/{revision}/{file}"
```

### 查看当前源

```bash
mangosteen models sources
```

### 指定源下载

```bash
# 默认源（modelscope）
mangosteen models pull Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf

# 指定 HuggingFace
mangosteen models pull hf:Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf

# 指定自定义源
mangosteen models pull myhub:Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf
```

## 调度策略配置

### 本地优先策略

```toml
[scheduler]
local_first = true                    # 不要关掉
max_fallbacks = 2                     # 降级重试上限
max_price_per_hour = 5.0              # 云端价格上限（元/小时）
```

### 四维打分权重

```toml
[scheduler.weights]
price = 40                            # 四维相对权重，不必加和为 1
latency = 15
availability = 30
hardware = 15
```

### 价格权重说明

- 价格权重越高，越倾向于选择便宜的实例
- 延迟权重越高，越倾向于选择低延迟实例
- 可用性权重越高，越倾向于选择库存充足的实例
- 硬件权重越高，越倾向于选择高性能 GPU

## 云端实例生命周期

| 操作 | 路由 | 云端动作 |
|------|------|----------|
| 停止服务 | POST /api/deployments/{id}/stop-service | 停进程+关机，实例保留 |
| 启动服务 | POST /api/deployments/{id}/start-service | 开机+起进程+重建隧道 |
| 卸载 | DELETE /api/deployments/{id} | 销毁实例 |

**stop_service 流程**：
1. 停进程（失败记 warning）
2. 关机（失败必须报错）
3. 关隧道

## 配置最佳实践

### 场景 1：个人开发者（本地 CPU）

**适用**：本地无 GPU，主要用于测试和小规模使用

```toml
[local]
allow_cpu_fallback = true
default_context = 8192
batch_size = 512
```

**特点**：
- 允许 CPU 降级
- 推理速度较慢（<5 tok/s）
- 无云端费用

### 场景 2：有 GPU 的开发者

**适用**：本地有 NVIDIA/AMD/Apple GPU

```toml
[local]
allow_cpu_fallback = false
kv_cache = "f16"
batch_size = 512
allow_partial_offload = true
```

**特点**：
- 强制使用 GPU
- 推理速度快
- 允许部分卸载（显存不足时）

### 场景 3：企业用户（云端优先）

**适用**：需要高性能 GPU，愿意按需付费

```toml
[scheduler]
local_first = false
max_price_per_hour = 10.0
max_fallbacks = 2

[scheduler.weights]
price = 50.0      # 更注重价格
latency = 10.0
availability = 30.0
hardware = 10.0
```

**特点**：
- 云端优先
- 价格上限控制
- 注重成本优化

### 场景 4：混合部署（本地+云端）

**适用**：小模型本地跑，大模型云端跑

```toml
[scheduler]
local_first = true
max_price_per_hour = 5.0

[local]
allow_cpu_fallback = true
default_context = 8192
```

**特点**：
- 本地优先
- 自动降级到云端
- 灵活切换

### 通用配置建议

1. **默认上下文长度**：8192（平衡性能和资源）
2. **KV 精度**：f16（推荐）或 q8_0（节省显存）
3. **批大小**：512（推荐）
4. **价格上限**：根据预算设置（建议 < 5 元/小时）
5. **本地优先**：始终保持 true（除非有特殊需求）

## 云端选点示例

### 按显存和价格筛选

```bash
mangosteen cloud pick --min-vram 24 --max-price 3.0 --explain
```

**输出示例**：
```
★ autodl/RTX 4090 [cn-west-1] ¥2.380/h · 42ms · 24.0GiB
  价格 1.00 延迟 1.00 可用 0.92 硬件 0.94 → 总分 0.9654
  ucloud/RTX 4090 [cn-east-2] ¥2.650/h · 78ms · 24.0GiB
  价格 0.90 延迟 0.54 可用 0.92 硬件 0.94 → 总分 0.8123
```

### 最便宜优先

```bash
mangosteen cloud cheapest --min-vram 24
```

纯价格序，不受打分策略影响。

### 创建实例

```bash
# 一步到位：选最便宜的并创建
mangosteen cloud cheapest --min-vram 24 --create --yes --name mn-dev
```

### 销毁实例

```bash
mangosteen cloud destroy --instance compshare/uhost-xxxx --yes
```

**注意**：裸实例不在部署台账里，不会被 `stop/abort` 自动回收，必须手动销毁。

## 查看云端实例

```bash
mangosteen cloud instances
```

## 压测与成本硬约束

### 硬性规则

- **同一时刻只允许 1 台 GPU 实例运行**
- 严禁并发创建
- 每轮结束销毁/关机再下一轮

### 登录凭证

唯一 API 途径：`DescribeCompShareInstance` 返回 `Password(Base64)`

### 冷启动窗口

cold start sshd 窗口可达 90s（`confirm_remote_reachable` 30×3s）

### 压测选型

每小时 < 1 元，优先最低价有货规格。

## 服务商 API 改版怎么办

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

这条配置路径**不需要改代码、不需要重新编译**；核对完成的厂商才会进入内置模板清单。

## 当前内置模板清单

- `compshare`：优云智算（UCloud RPC 形态，专用驱动）
- 其他：暂无内置（通过配置 profile 动态接入）
