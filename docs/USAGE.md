# Mangosteen 使用指南

本文档记录了 Mangosteen 云端租赁部署大模型客户端的详细使用说明。如需快速查看命令列表，请执行 `mangosteen --help`。

---

## 1. 快速开始

### 查看当前状态
```bash
mangosteen status
```

### 查看可用GPU节点
```bash
mangosteen cloud cheapest --min-vram 1
```

### 部署模型到云端
```bash
mangosteen deploy qwen2.5-0.5b --gpu-model V100S --watch
```

### 停止部署
```bash
mangosteen stop <部署ID> --yes
```

---

## 2. 命令参考

### 2.1 deploy - 部署模型

```bash
mangosteen deploy <模型> [--gpu-model <型号>] [--device <节点ID>] [--template <槽位>] [--watch] [--async]
```

| 参数 | 说明 |
|------|------|
| `<模型>` | 模型名称（如 qwen2.5-0.5b） |
| `--gpu-model` | GPU 型号（如 3080TI、V100S、P40） |
| `--device` | 指定自定义节点 ID |
| `--template` | 配置模板名称（如 default） |
| `--watch` | 轮询部署进度 |
| `--async` | 异步返回，不等待部署完成 |

### 2.2 stop - 停止部署

```bash
mangosteen stop <部署ID> [--all] [--yes]
```

| 参数 | 说明 |
|------|------|
| `<部署ID>` | 部署 ID，可通过 `mangosteen status` 查看 |
| `--all` | 停止全部部署 |
| `--yes` | 免确认直接执行 |

### 2.3 status - 查看状态

```bash
mangosteen status [--json]
```

| 参数 | 说明 |
|------|------|
| `--json` | 以 JSON 格式输出 |

### 2.4 cloud - 云端算力管理

#### cloud instances - 云端实例

```bash
mangosteen cloud instances list
mangosteen cloud instances start <厂商>/<实例ID>
mangosteen cloud instances stop <厂商>/<实例ID>
mangosteen cloud instances destroy <厂商>/<实例ID> --yes
```

| 子命令 | 说明 |
|--------|------|
| `list` | 列出云端实例（含已关机） |
| `start` | 启动实例（开机） |
| `stop` | 关闭实例（停止 GPU 计费，保留磁盘） |
| `destroy` | 删除实例（销毁实例与磁盘，不可恢复，需 `--yes`） |

#### cloud key - 云厂商密钥管理

```bash
mangosteen cloud key <厂商> [--secret <密钥>] [--public-key <公钥>] [--enable|--disable]
```

| 参数 | 说明 |
|------|------|
| `--secret` | 更新厂商密钥（私钥，存储为文件引用） |
| `--public-key` | 更新厂商公钥（AccessKey ID） |
| `--enable` | 启用该厂商 |
| `--disable` | 禁用该厂商 |

#### cloud pick - 智能选点（四维打分）

```bash
mangosteen cloud pick [--min-vram <GB>] [--max-price <元/小时>] [--explain]
```

| 参数 | 说明 |
|------|------|
| `--min-vram` | 最小显存要求（GB） |
| `--max-price` | 最高价格上限（元/小时） |
| `--explain` | 显示评分明细 |

#### cloud cheapest - 最便宜优先

```bash
mangosteen cloud cheapest [--min-vram <GB>]
```

| 参数 | 说明 |
|------|------|
| `--min-vram` | 最小显存要求（GB） |

### 2.5 device - 自定义节点

```bash
mangosteen device list
mangosteen device add <主机> <端口> [--user <用户>] [--workdir <目录>] [--password <密码>] [--name <名称>]
mangosteen device remove <ID>
mangosteen device test <ID>
```

| 子命令 | 说明 |
|--------|------|
| `list` | 列出自定义节点 |
| `add` | 新增自定义节点 |
| `remove` | 删除自定义节点 |
| `test` | 测试节点 SSH 连通性 |

### 2.6 config - 配置管理

```bash
mangosteen config show | init | path | validate | reset-templates
```

| 子命令 | 说明 |
|--------|------|
| `show` | 显示当前配置 |
| `init` | 初始化配置 |
| `path` | 显示配置文件路径 |
| `validate` | 验证配置 |
| `reset-templates` | 重置内置模板 |

### 2.7 ui - Web UI 控制台

```bash
mangosteen ui [start|stop|restart|status] [--port <端口>] [--no-open]
```

### 2.8 doctor - 环境自检

```bash
mangosteen doctor
```

---

## 3. 全局选项

| 选项 | 说明 |
|------|------|
| `--config <path>` | 指定配置文件路径 |
| `--json` | 以 JSON 格式输出 |
| `--quiet` | 抑制 banner 与进度提示 |
| `--help, -h` | 显示帮助 |
| `--version, -V` | 显示版本 |

---

## 4. 配置文件

配置文件位于：`~/Library/Application Support/mangosteen/config.toml`

主要配置段：

```toml
[scheduler]
local_first = true       # 本地优先
strategy = "cheapest"    # 调度策略：cheapest / balanced

[providers.compshare]
enabled = true
[providers.compshare.options]
public_key = "xxx"
image_id = "compshareImage-xxx"
```

---

## 5. 注意事项

1. **GPU 库存**：云厂商 GPU 资源可能暂时无库存，部署失败请更换 GPU 型号或稍后重试。
2. **API 频率限制**：短时间内频繁调用厂商 API 可能触发限流（错误码 [226619]），请稍后重试。
3. **SSH 密码**：云厂商实例密码重置后需要等待 sshd 生效，系统已自动处理等待逻辑。
4. **计费提醒**：云端实例正在计费，使用完毕请及时执行 `mangosteen stop` 释放资源。

---

## 6. 常见问题

### Q: 部署失败，提示 "Permission denied"
A: 实例密码重置后 sshd 可能尚未生效，系统会自动重试，请稍后再试。

### Q: 部署失败，提示 "[226604] 该规格当前无库存"
A: 该 GPU 型号当前无库存，请更换 GPU 型号或稍后重试。

### Q: 部署失败，提示 "[226619] 接口调用过于频繁"
A: 触发厂商 API 限流，请等待 5-10 分钟后重试。

---

文档维护路径：`/Users/zhuzhichao/Documents/mangosteen/docs/USAGE.md`
