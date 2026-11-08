# Mangosteen Windows 用户使用指南

## 概述

本指南适用于在 Windows 环境下运行 Mangosteen 大模型部署客户端。Mangosteen 是一款个人级大模型部署客户端，支持自动选择租赁多家云厂商 GPU 算力资源，最优化选择 GPU 节点，并在该节点上自动化部署开源大模型。

## 一、系统要求

| 项目 | 要求 |
|------|------|
| 操作系统 | Windows 10 / Windows 11 (64 位) |
| 架构 | x86_64 |
| 网络 | 能够访问云服务商 API（如优云智算） |
| 磁盘空间 | 建议预留 100 GB+ 用于模型存储 |
| 内存 | 建议 16 GB+ |

## 二、安装步骤

### 2.1 获取发布包

从 Mangosteen 发布页面下载适用于 Windows 的发布包：

```
mangosteen-windows-x86_64.zip
```

### 2.2 解压与安装

```powershell
# 在 PowerShell 中执行
Expand-Archive -Path mangosteen-windows-x86_64.zip -DestinationPath C:\Program Files\mangosteen
```

或直接右键点击 zip 文件，选择“全部解压到...”。

### 2.3 添加到系统 PATH（可选）

若要在任意目录直接运行 `mangosteen` 命令，请将 `mangosteen.exe` 所在目录添加到系统 PATH：

```powershell
# PowerShell（当前用户，需重新打开终端生效）
[Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "User") + ";C:\Program Files\mangosteen", "User")
```

或通过“系统属性 → 环境变量”手动添加。

## 三、配置文件

### 3.1 配置文件位置

Mangosteen 在 Windows 上的配置文件默认位置：

```
%APPDATA%\mangosteen\config.toml
```

即：

```
C:\Users\<你的用户名>\AppData\Roaming\mangosteen\config.toml
```

### 3.2 初始化配置

首次运行前，请执行：

```powershell
mangosteen config init
```

这会生成默认配置模板。

或手动从发布包中的 `config/config.example.toml` 复制：

```powershell
# 创建配置目录
mkdir "%APPDATA%\mangosteen"

# 复制配置模板
copy "C:\Program Files\mangosteen\config\config.example.toml" "%APPDATA%\mangosteen\config.toml"

# 编辑配置
notepad "%APPDATA%\mangosteen\config.toml"
```

### 3.3 配置云服务商凭证

编辑 `%APPDATA%\mangosteen\config.toml`，在 `[providers.compshare]` 段填入你的云服务商凭证：

```toml
[providers.compshare]
driver = "compshare"
enabled = true
display_name = "优云智算"
secret = { env = "COMPSHARE_PRIVATE_KEY" }
priority = 1.0

[providers.compshare.options]
public_key = "你的AccessKey ID"
region = "cn-wlcb"
zone = "cn-wlcb-01"
image_id = "compshareImage-xxxx"      # 必填，从优云智算控制台获取
charge_type = "Postpay"                # 按量计费：关机不收费
disk_type = "CLOUD_SSD"
disk_size_gb = "100"
gpu_types = "4090,5090"
max_gpu_count = "1"
min_memory_gb = "32"
```

**凭证三种写法（推荐使用环境变量）：**

```toml
# 方式 1：环境变量（推荐，安全）
secret = { env = "COMPSHARE_PRIVATE_KEY" }

# 方式 2：从文件读取
secret = { file = "C:\\secrets\\compshare.key" }

# 方式 3：直接内联（仅调试用，不安全）
secret = { inline = "你的私钥明文" }
```

### 3.4 设置环境变量（PowerShell 示例）

```powershell
# 设置优云智算私钥（当前会话）
$env:COMPSHARE_PRIVATE_KEY = "你的私钥"

# 持久化设置（重启后生效）
[Environment]::SetEnvironmentVariable("COMPSHARE_PRIVATE_KEY", "你的私钥", "User")
[Environment]::SetEnvironmentVariable("COMPSHARE_PUBLIC_KEY", "你的公钥", "User")
```

## 四、基本操作

### 4.1 环境自检

```powershell
mangosteen doctor
```

检查输出中各项状态，确保没有 `FAIL` 项。

### 4.2 查看可用云服务商

```powershell
mangosteen cloud providers
```

### 4.3 查看 GPU 规格

```powershell
mangosteen cloud specs
```

### 4.4 智能选点（查看打分依据）

```powershell
mangosteen cloud pick --min-vram 24 --max-price 3.0 --explain
```

### 4.5 部署模型

```powershell
# 部署 Qwen2.5-7B，上下文 8192
mangosteen deploy qwen2.5-7b-instruct --context 8192

# 部署完成后，通过 OpenAI 兼容接口调用
# 默认端口：http://127.0.0.1:11516/v1
```

### 4.6 查看部署状态

```powershell
mangosteen status
mangosteen ps
```

### 4.7 停止部署（避免持续计费）

```powershell
# 停止所有部署
mangosteen stop --all --yes

# 或停止指定部署
mangosteen stop <部署ID>
```

## 五、OpenAI 兼容接口

部署成功后，可以使用标准 OpenAI SDK：

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:11516/v1",
    api_key="not-needed"  # Mangosteen 不需要 API Key
)

resp = client.chat.completions.create(
    model="auto",
    messages=[{"role": "user", "content": "你好"}],
)
print(resp.choices[0].message.content)
```

或使用 `curl`：

```powershell
curl http://127.0.0.1:11516/v1/chat/completions `
  -H "Content-Type: application/json" `
  -d '{"model":"auto","messages":[{"role":"user","content":"你好"}]}'
```

## 六、Web 控制台（可选）

```powershell
# 启动 Web 控制台并自动打开浏览器
mangosteen ui

# 仅启动服务，不打开浏览器
mangosteen ui --no-open
```

Web 控制台默认地址：`http://127.0.0.1:11516`

## 七、常见问题

### 7.1 配置文件找不到

**现象**：`mangosteen doctor` 提示配置文件不存在。

**解决**：
```powershell
mangosteen config init
# 或手动复制 config.example.toml 到 %APPDATA%\mangosteen\config.toml
```

### 7.2 端口 11516 被占用

**现象**：部署失败，提示端口已占用。

**解决**：
```powershell
# 停止所有 mangosteen 进程
mangosteen stop --all

# 或修改配置文件中的 gateway.port
```

### 7.3 llama-server 动态库找不到

**现象**：`mangosteen doctor` 提示引擎动态库缺失。

**解决**：
- 确保 `runtime/windows-x86_64/` 目录下包含 `llama-server.exe` 及其依赖的 `.dll` 文件
- 或在配置中指定 `[local.engine_path]`
- 检查 Windows PATH 环境变量是否包含 DLL 所在目录

### 7.4 Windows SmartScreen 警告

**现象**：首次运行 `mangosteen.exe` 时 Windows 提示“已阻止运行”。

**解决**：
1. 点击“更多信息”
2. 点击“仍要运行”
3. 或使用已签名的发布版本（生产环境建议代码签名）

### 7.5 云端实例忘记关机

**现象**：持续产生云端计费。

**解决**：
```powershell
# 查看所有云端实例
mangosteen cloud instances

# 停止指定实例
mangosteen stop <部署ID>

# 停止所有
mangosteen stop --all --yes
```

## 八、配置文件完整参考

Windows 配置文件 `%APPDATA%\mangosteen\config.toml` 完整示例：

```toml
config_version = 1

[gateway]
port = 11516
host = "127.0.0.1"
require_api_key = false

[scheduler]
local_first = true
max_fallbacks = 2
launch_timeout_secs = 120
provision_timeout_secs = 600
max_price_per_hour = 5.0

[scheduler.weights]
price = 40
latency = 15
availability = 30
hardware = 15

[local]
# Windows 下 llama-server 路径示例
# engine_path = "C:\\Program Files\\mangosteen\\runtime\\windows-x86_64\\llama-server.exe"

default_context = 8192
min_context = 2048
kv_cache = "f16"
batch_size = 512
allow_cpu_fallback = true
allow_partial_offload = true
auto_library_path = true

port_range_start = 11435
port_range_end = 11999

[storage]
models_dir = "%LOCALAPPDATA%\\mangosteen\\models"
cache_dir = "%LOCALAPPDATA%\\mangosteen\\cache"
verify_hash = false
max_model_bytes = 214748364800
default_source = "modelscope"
allowed_read_roots = []

[providers.compshare]
driver = "compshare"
enabled = true
display_name = "优云智算"
secret = { env = "COMPSHARE_PRIVATE_KEY" }
priority = 1.0

[providers.compshare.options]
public_key = "你的AccessKey ID"
region = "cn-wlcb"
zone = "cn-wlcb-01"
image_id = "compshareImage-xxxx"
charge_type = "Postpay"
disk_type = "CLOUD_SSD"
disk_size_gb = "100"
gpu_types = "4090,5090"
max_gpu_count = "1"
min_memory_gb = "32"
max_price_queries = "10"
operation_timeout_secs = "600"
poll_interval_secs = "5"
release_data_disk = "true"
```

**注意**：Windows 路径中建议使用正斜杠 `/` 或双反斜杠 `\\`，避免转义问题。环境变量如 `%APPDATA%` 在配置文件中不会自动展开，建议直接写绝对路径。

## 九、卸载

```powershell
# 1. 停止所有部署
mangosteen stop --all --yes

# 2. 删除安装目录
Remove-Item -Recurse -Force "C:\Program Files\mangosteen"

# 3. 删除配置目录（可选，如需保留配置可跳过）
Remove-Item -Recurse -Force "$env:APPDATA\mangosteen"

# 4. 删除模型目录（如不再需要模型文件）
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\mangosteen"
```

## 十、技术支持

- 项目仓库：https://github.com/zhuzhichaoTM/mangosteen
- 问题反馈：https://github.com/zhuzhichaoTM/mangosteen/issues
- 配置示例：`config/config.example.toml`

---

**文档版本**：1.0.0  
**适用平台**：Windows 10 / Windows 11 (x86_64)  
**适用版本**：Mangosteen 0.1.0+
