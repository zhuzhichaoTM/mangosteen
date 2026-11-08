# Mangosteen 命令速查表

## 最常用的 5 个命令

1. **`mangosteen deploy <模型>`** - 部署模型（本地优先，不足自动上云）
2. **`mangosteen status`** - 查看系统状态（硬件/引擎/配置/活跃部署）
3. **`mangosteen stop --all`** - 停止所有部署并释放资源
4. **`mangosteen models list`** - 列出本地模型
5. **`mangosteen doctor`** - 环境自检（引擎、硬件、配置、权限）

## 核心命令

| 命令 | 作用 | 示例 |
|------|------|------|
| `deploy <模型>` | 部署（本地优先，不足自动上云） | `mangosteen deploy qwen2.5-7b --context 8192` |
| `run <模型>` | 部署并前台保持运行，Ctrl-C 优雅退出 | `mangosteen run qwen2.5-7b` |
| `stop <ID> \| --all` | 停止并释放资源 | `mangosteen stop abc123`<br>`mangosteen stop --all --yes` |
| `status` | 系统状态（硬件/引擎/配置/活跃部署） | `mangosteen status` |
| `ps` | 列出全部部署记录 | `mangosteen ps` |
| `delete <ID>` | 卸载（云端会销毁实例） | `mangosteen delete abc123` |

## 模型管理

| 命令 | 作用 | 示例 |
|------|------|------|
| `models search <仓库>` | 查看仓库里的 GGUF 文件 | `mangosteen models search Qwen/Qwen2.5-0.5B-Instruct-GGUF` |
| `models pull <路径>` | 下载模型 | `mangosteen models pull Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf` |
| `models list` | 列出本地模型 | `mangosteen models list` |
| `models show <模型>` | 显示模型详情 | `mangosteen models show qwen2.5-7b-instruct` |
| `models add <路径>` | 添加已有模型 | `mangosteen models add ./qwen2.5-7b-instruct-q4_k_m.gguf` |
| `models remove <模型>` | 删除模型 | `mangosteen models remove qwen2.5-7b-instruct` |
| `models verify <模型>` | 验证模型 SHA-256 | `mangosteen models verify qwen2.5-7b-instruct` |
| `models sources` | 查看模型源列表 | `mangosteen models sources` |

## 资源评估

| 命令 | 作用 | 示例 |
|------|------|------|
| `assess <模型>` | 只评估不部署，看本地能不能跑 | `mangosteen assess llama3.3-70b --context 8192` |

## 云端算力

| 命令 | 作用 | 示例 |
|------|------|------|
| `cloud providers` | 列出云服务商 | `mangosteen cloud providers` |
| `cloud specs` | 列出所有规格 | `mangosteen cloud specs` |
| `cloud pick` | 智能选点（四维打分） | `mangosteen cloud pick --min-vram 24 --max-price 3.0 --explain` |
| `cloud cheapest` | 最便宜优先（纯价格序） | `mangosteen cloud cheapest --min-vram 24` |
| `cloud instances` | 查看云端实例 | `mangosteen cloud instances` |
| `cloud destroy` | 销毁裸实例 | `mangosteen cloud destroy --instance compshare/uhost-xxxx --yes` |

## 配置管理

| 命令 | 作用 | 示例 |
|------|------|------|
| `config show` | 显示配置（凭证脱敏） | `mangosteen config show` |
| `config init` | 生成配置模板 | `mangosteen config init` |
| `config path` | 显示配置文件路径 | `mangosteen config path` |
| `config validate` | 验证配置合法性 | `mangosteen config validate` |

## 引擎管理

| 命令 | 作用 | 示例 |
|------|------|------|
| `engine env` | 显示注入的库路径 | `mangosteen engine env` |
| `engine locate` | 显示搜索过的目录 | `mangosteen engine locate` |

## 服务与界面

| 命令 | 作用 | 示例 |
|------|------|------|
| `serve` | 只起 API 网关（多终端协作） | `mangosteen serve --port 8080` |
| `tui` | 全屏交互界面 | `mangosteen tui` |
| `ui` | 启动 Web 控制台 | `mangosteen ui`<br>`mangosteen ui --no-open` |

## 环境自检

| 命令 | 作用 | 示例 |
|------|------|------|
| `doctor` | 环境自检 | `mangosteen doctor` |

## 常用组合

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

## 全局参数

| 参数 | 作用 | 适用命令 |
|------|------|----------|
| `--context <长度>` | 指定上下文长度 | `deploy`, `assess` |
| `--local-only` | 强制本地部署 | `deploy` |
| `--yes` | 跳过确认 | `stop`, `cloud destroy`, `cloud cheapest --create` |
| `--explain` | 显示打分依据 | `cloud pick` |
| `--min-vram <GB>` | 最低显存要求 | `cloud pick`, `cloud cheapest` |
| `--max-price <元/小时>` | 价格上限 | `cloud pick` |
| `--name <名称>` | 实例名称 | `cloud cheapest --create` |
