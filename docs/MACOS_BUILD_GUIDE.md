# Mangosteen macOS 使用指南

本指南基于 `README.md` 编写，适用于 macOS (Intel / Apple Silicon) 平台。

## 一、编译构建

### 前提条件

```bash
# 安装 Rust（如未安装）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 确认 cargo 可用
cargo --version
```

### 安装交叉编译目标

本机是 **Intel Mac (x86_64)**，默认已安装 `x86_64-apple-darwin`。如需构建 Apple Silicon 归档，需额外安装：

```bash
rustup target add aarch64-apple-darwin
```

> 注：`aarch64-apple-darwin` 的标准库约 26 MB，首次下载可能需要数分钟，请保持网络畅通。

### 使用构建脚本编译两个平台归档

```bash
cd /Users/zhuzhichao/Documents/mangosteen

# 构建两个平台（Intel + Apple Silicon）
./scripts/build-macos.sh --all

# 或仅构建某一平台
./scripts/build-macos.sh --intel          # x86_64-apple-darwin
./scripts/build-macos.sh --apple-silicon  # aarch64-apple-darwin
```

构建产物：

| 文件 | 对应平台 |
|------|---------|
| `dist/mangosteen-macos-x86_64.tar.gz` | macOS (Intel) |
| `dist/mangosteen-macos-aarch64.tar.gz` | macOS (Apple Silicon) |
| `dist/*.sha256` | 校验和文件 |

每个归档包含：`mangosteen` 二进制、`README.md`、`LICENSE`、`docs/`、`config/`、`INSTALL.txt`。

### 使用 release 服务器（快速启动，不编译）

如果只需在本机运行，可直接使用 release 二进制：

```bash
# 启动 API 网关（后台运行）
/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen serve

# 验证服务
curl http://127.0.0.1:11516/health
```

---

## 二、安装与初始化

### 1. 安装二进制到 PATH

```bash
# 从归档解压后
sudo cp mangosteen /usr/local/bin/
mangosteen --version
```

### 2. 初始化配置

```bash
mangosteen config init
mangosteen doctor   # 环境自检，会提示缺少的依赖
```

配置文件位置：

```text
macOS: ~/Library/Application Support/mangosteen/config.toml
```

### 3. 安装推理引擎 (llama.cpp)

Mangosteen 唯一支持的推理引擎是 llama.cpp：

```bash
brew install llama.cpp
# 确保 llama-server 在 PATH 中
which llama-server
```

---

## 三、配置云服务商凭证

编辑 `~/Library/Application Support/mangosteen/config.toml`，添加优云智算（compshare）配置：

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
image_id = "compshareImage-xxxx"   # 从优云智算控制台「镜像」页复制
charge_type = "Postpay"            # 按量计费，关机不收费
gpu_types = "4090,5090"
max_gpu_count = "1"
```

凭证三种写法（均不会在 `config show` 中明文输出）：

```toml
secret = { env = "MY_TOKEN" }         # 从环境变量读（推荐）
secret = { file = "/run/secrets/tok" } # 从文件读
secret = { inline = "xxx" }            # 直接内联（仅调试用）
```

### 验证配置

```bash
mangosteen config show
mangosteen cloud providers
```

---

## 四、部署模型

### CLI 方式

```bash
# 查看可用云端规格（不创建实例，不产生费用）
mangosteen cloud pick --min-vram 24 --max-price 3.0 --explain

# 登记模型
mangosteen models add /path/to/qwen2.5-7b-instruct-q4_k_m.gguf

# 部署到云端
mangosteen deploy qwen2.5-7b-instruct --context 8192

# 查看部署状态
mangosteen ps
mangosteen status

# 停止部署（关机，实例保留，停止计费）
mangosteen stop <部署ID>

# 彻底删除（销毁实例与磁盘，不可恢复）
mangosteen stop --all --yes
```

### Web UI 方式

```bash
# 启动 Web UI（自动打开浏览器）
mangosteen ui

# 或仅启动服务（不打开浏览器）
mangosteen ui --no-open
```

访问 `http://127.0.0.1:11516`，四个页签：

| 页签 | 用途 |
|------|------|
| 对话 | 流式对话，可附加文档 |
| 部署 | 搜索模型 → 下载 → 部署 → 停止/删除，确认前弹窗显示算力规格与预估花费 |
| 设备 | CPU/内存/显存实时曲线、加速卡、磁盘剩余 |
| 设置 | 调度策略、本地推理参数、云服务商开关（凭证不回显） |

---

## 四、使用 OpenAI 兼容接口

部署成功后，可通过 OpenAI SDK 调用：

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:11516/v1", api_key="not-needed")
resp = client.chat.completions.create(
    model="auto",          # "auto" = 用当前部署的模型
    messages=[{"role": "user", "content": "你好"}],
)
print(resp.choices[0].message.content)
```

---

## 五、常用命令速查

| 命令 | 作用 |
|------|------|
| `mangosteen deploy <模型>` | 部署到云端 GPU 节点 |
| `mangosteen run <模型>` | 部署并前台保持运行，Ctrl-C 退出 |
| `mangosteen stop <ID> --all` | 停止并释放资源 |
| `mangosteen status` | 系统状态（配置 / 活跃部署） |
| `mangosteen ps` | 列出全部部署记录 |
| `mangosteen cloud providers/specs/pick/cheapest/instances/destroy` | 云端算力管理 |
| `mangosteen config show/init/path/validate` | 配置管理 |
| `mangosteen serve` | 只起 API 网关 |
| `mangosteen tui` | 全屏交互界面 |
| `mangosteen doctor` | 环境自检 |

---

## 六、故障处理

### 部署失败，如何定位

```bash
mangosteen ps                          # 查看状态
mangosteen status                      # 查看活跃部署
# 云端日志在 <日志目录>/engine-<端口>.log
```

### 云端实例忘了关（避免持续计费）

```bash
mangosteen ps                    # 找到部署 ID
mangosteen stop <部署ID>          # 停掉
mangosteen stop --all --yes      # 或者全停
mangosteen cloud instances       # 只看云端实例
```

台账是**每次状态变更立即落盘**的，进程重启也不会丢。

### 端口被占用

```bash
mangosteen stop --all            # 清理残留进程
# 或修改 config.toml 中的 gateway.port
```

### 构建问题

如果 `rustup target add aarch64-apple-darwin` 下载失败，请检查网络连接，或手动执行：

```bash
rustup target add aarch64-apple-darwin
```

---

## 七、产品边界

Mangosteen 是**个人级桌面客户端**，明确不做：

* 集群管理、多租户、分布式推理（企业级能力）
* Agent 框架（只提供部署与推理 API，上层应用自带）
* 未经核对的云厂商内置模板（通过配置 profile 动态接入）

云端优化目标为 RTX 4090 / 5090 等消费级显卡实例。

---

**许可**: Apache-2.0
