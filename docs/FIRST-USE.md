# Mangosteen 首次使用指南（新 Mac）

本指南以**云端 GPU 部署 qwen2.5-0.5b** 为例，从拿到发布包到模型可用，全程约 10 分钟（不含云端实例启动与模型下载时间）。

> 适用版本：0.1.0（macOS x86_64 / Apple Silicon）
> 目标读者：第一次在新 Mac 上使用 Mangosteen 的用户

---

## 第 1 步：安装发布件

从发布包目录取得对应架构的压缩包：

| Mac 芯片 | 下载文件 |
|----------|----------|
| Intel | `mangosteen-macos-x86_64.tar.gz` |
| Apple Silicon（M1/M2/M3/M4） | `mangosteen-macos-aarch64.tar.gz` |

> 查看芯片类型：左上角  →「关于本机」，看「芯片」一行。

解压并安装到 PATH：

```bash
# 解压
tar -xzf mangosteen-macos-x86_64.tar.gz
cd mangosteen-macos-x86_64

# 安装到系统 PATH（二选一）
sudo cp mangosteen /usr/local/bin/mangosteen     # 方式 A：系统目录
# 或
mkdir -p ~/bin && cp mangosteen ~/bin/           # 方式 B：用户目录
echo 'export PATH="$HOME/bin:$PATH"' >> ~/.zshrc && source ~/.zshrc
```

验证安装：

```bash
mangosteen version
```

**预期输出：**
```
Mangosteen 0.1.0 (macos / x86_64)
```

---

## 第 2 步：初始化配置

```bash
mangosteen config init
```

**预期输出：**
```
✓ 已创建配置文件
```

配置文件位置（记住它，下一步要编辑）：

```
~/Library/Application Support/mangosteen/config.toml
```

查看配置路径的快捷命令：

```bash
mangosteen config path
```

---

## 第 3 步：配置云服务商（优云智算 compshare）

云端部署需要一个云 GPU 服务商。以**优云智算 compshare** 为例：

### 3.1 准备 API 凭证

登录优云智算控制台 →「访问管理 / API 密钥」页面，获取：
- **AccessKey ID**（公钥，形如 `ak-xxxxxxxx`）
- **AccessKey Secret**（私钥，只在创建时显示一次，务必保存）

### 3.2 写入密钥

使用内置命令写入（密钥加密存储，不会明文落盘）：

```bash
# 写入私钥（Secret Key）——按提示粘贴，不回显
mangosteen cloud key compshare --secret

# 写入公钥（AccessKey ID）
mangosteen cloud key compshare --public-key ak-你的AccessKeyID
```

**预期输出：**
```
✓ 已更新 compshare 公钥
✓ 已更新 compshare 私钥
```

### 3.3 补全服务商参数

编辑 `config.toml`，确认 `[providers.compshare]` 段：

```toml
[providers.compshare]
driver = "compshare"
enabled = true
display_name = "优云智算"
priority = 1.0

[providers.compshare.secret]
file = "~/Library/Application Support/mangosteen/secrets/compshare_secret_key"

[providers.compshare.options]
public_key = "ak-你的AccessKeyID"
region = "cn-wlcb"                  # 地域（华北·乌兰察布）
zone = "cn-wlcb-01"                 # 可用区
image_id = "compshareImage-xxxx"    # 镜像 ID（必填，控制台「镜像」页获取）
charge_type = "Postpay"             # 按量计费：关机不收费
disk_type = "CLOUD_SSD"             # 系统盘类型
disk_size_gb = "100"                # 系统盘大小
gpu_types = ""                      # 允许的卡型号，留空不限制
min_memory_gb = "32"                # 候选规格的最低内存
max_price_queries = "10"            # 询价次数上限（控制 API 调用量）
poll_interval_secs = "5"            # 实例状态轮询间隔
```

> **image_id 怎么查**：优云智算控制台 →「云服务器」→「创建实例」→ 镜像列表，复制镜像 ID（形如 `compshareImage-xxxx`）。Ubuntu 22.04 镜像即可。

验证配置：

```bash
mangosteen config validate
mangosteen cloud providers
```

**预期输出（cloud providers）：**
```
  名称         显示名     驱动         状态  权重
  ---------  ------  ---------  --  ---
  aliyun     阿里云     aliyun     禁用  1.0
  autodl     AutoDL  autodl     禁用  1.0
  compshare  优云智算    compshare  启用  1.0
```

（只要 compshare 行显示「启用」即配置成功；aliyun/autodl 未配置密钥时显示「禁用」属正常。）

---

## 第 4 步：环境自检

```bash
mangosteen doctor
```

**预期输出（全部通过或有可接受的警告）：**
```
✓ 硬件采集
✓ 加速卡
✓ 推理引擎
✓ 配置
✓ 云服务商
✓ 目录权限
✓ 端口
✓ 磁盘
```

> **注意**：云端部署时，推理引擎（llama-server）会自动安装在远端 GPU 节点上，本机无需安装 llama.cpp。doctor 中「推理引擎」警告不影响云端部署。

---

## 第 5 步：查看可用 GPU 节点

```bash
# 查看价格低于 1.5 元/小时的 GPU 节点
mangosteen cloud cheapest --min-vram 1
```

**预期输出（cloud cheapest）：**
```
最便宜候选（按价格升序）：

  服务商        GPU型号         数量  显存        价格        地域
  ---------  ------------  --  --------  --------  ----------
  compshare  V100S         1x  32.0 GiB  ¥1.340/h  cn-wlcb-01
  compshare  RTX 4090      1x  24.0 GiB  ¥2.050/h  cn-wlcb-01
  compshare  RTX 4090-48G  1x  48.0 GiB  ¥3.610/h  cn-wlcb-01
  compshare  A800          1x  80.0 GiB  ¥6.990/h  cn-wlcb-01
  compshare  A100          1x  80.0 GiB  未知        cn-wlcb-01
```

记下想要的 GPU 型号（如 `V100S`、`3090`、`3080TI`），下一步部署用。

**cloud providers 预期输出：**
```
  名称         显示名     驱动         状态  权重
  ---------  ------  ---------  --  ---
  aliyun     阿里云     aliyun     禁用  1.0
  autodl     AutoDL  autodl     禁用  1.0
  compshare  优云智算    compshare  启用  1.0
```

---

## 第 6 步：部署 qwen2.5-0.5b

```bash
mangosteen deploy qwen2.5-0.5b --gpu-model 3090 --watch
```

参数说明：
- `qwen2.5-0.5b`：模型名（内置模板，无需下载登记）
- `--gpu-model 3090`：指定 GPU 型号（可换成 `3080TI` 等）
- `--watch`：持续显示部署进度，直到完成

**部署过程（约 2-15 分钟，取决于实例启动与模型下载）：**

```
· 配置模板：default（来源：设置页配置；可用 --template <槽位> 切换）
[perf] ▶ 阶段 实例创建(等待Running+SSH确认)
[cloud] ✓ 实例 uhost-xxxx 已进入 Running 状态
[perf] ▶ 阶段 节点配置(依赖安装+llama-server下载)
[perf] ▶ 阶段 模型下载
[perf] ✓ 阶段 模型下载 耗时 120.5s
[perf] ▶ 阶段 启动模型(拉引擎+健康检查)
[perf] ✓ 阶段 健康检查(/health就绪)

✓ 部署完成：qwen2.5-0.5b (云端算力 · compshare)
  部署 ID      dep-xxxx-qwen2.5-0.5b
  后端端口       11435
  OpenAI 入口  http://127.0.0.1:11516/v1
✓ 隧道已交由网关节扭持有（CLI 退出后依然有效）
```

**部署成功的标志：**
1. 显示「✓ 部署完成」
2. 显示「隧道已交由网关节扭持有」
3. `mangosteen status` 显示状态 `ready`、运行状态 `active`

---

## 第 7 步：验证模型可用

```bash
# 1. 查看部署状态
mangosteen status
```

**预期输出：**
```
部署 ID                            模型               状态   运行状态   本地端口  远端端口  健康
dep-xxxx-qwen2.5-0.5b  qwen2.5-0.5b-instruct  ready  active    11435    8080    就绪
```

```bash
# 2. 发送对话请求（模型名就是简化后的 qwen2.5-0.5b）
curl http://127.0.0.1:11516/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-0.5b",
    "messages": [{"role": "user", "content": "你好，请介绍一下你自己"}],
    "max_tokens": 128
  }'
```

**预期输出：**
```json
{
  "id": "chatcmpl-xxx",
  "object": "chat.completion",
  "choices": [{"message": {"role": "assistant", "content": "你好！我是 Qwen..."}}]
}
```

---

## 第 8 步：用完停止（重要：停止计费）

```bash
# 停止全部部署（保留实例与已下载模型，下次复用）
mangosteen stop --all --yes
```

> **计费说明**：`charge_type = "Postpay"` 时，**实例开机即计费、关机停止计费**。停止部署会关闭实例（保留磁盘与模型文件），下次部署直接复用，无需重新下载。

---

## 常用命令速查

| 命令 | 用途 |
|------|------|
| `mangosteen status` | 查看部署状态（ready/active） |
| `mangosteen stop --all --yes` | 停止全部部署（停止计费） |
| `mangosteen cloud cheapest --min-vram 1` | 查看 GPU 节点与价格 |
| `mangosteen cloud instances list` | 查看云实例状态 |
| `mangosteen ui start` | 启动 Web 控制台（http://127.0.0.1:11516） |
| `mangosteen models list` | 查看模型配置模板 |
| `mangosteen doctor` | 环境自检 |

## API 访问信息

| 项目 | 值 |
|------|-----|
| OpenAI 兼容入口 | `http://127.0.0.1:11516/v1` |
| Anthropic 兼容入口 | `http://127.0.0.1:11516/v1/messages` |
| 模型名 | `qwen2.5-0.5b` |

Python SDK 示例：

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:11516/v1", api_key="none")
resp = client.chat.completions.create(
    model="qwen2.5-0.5b",
    messages=[{"role": "user", "content": "你好"}],
)
print(resp.choices[0].message.content)
```

---

## 常见问题

### 1. 部署报「接口调用过于频繁 [226619]」
云厂商 API 限流，等待 5-10 分钟后重试即可。这不是代码问题。

### 2. 部署报「无库存 [226604]」
该 GPU 型号当前无库存，换型号（`--gpu-model 3080TI`）或稍后重试。

### 3. status 显示 ready 但 unactive
正常瞬态（模型加载中）。若持续超过 1 分钟，执行 `mangosteen ui restart` 重建隧道；新版本已内置探测自愈（约 15 秒自动重建）。

### 4. curl 连接被拒绝（11516）
网关未启动。执行 `mangosteen ui start` 启动（部署成功时 CLI 会自动拉起守护网关，若手动 stop 过则需重启）。

### 5. 模型名是 qwen2.5-0.5b 还是 qwen2.5-0.5b-instruct？
API 调用一律用简化名 **`qwen2.5-0.5b`**（默认激活别名）。台账中的 `-instruct` 只是内部规格名。

### 6. 部署其他模型（qwen3.8-27b）
```bash
# 默认配置
mangosteen deploy qwen3.8-27b --gpu-model 3090 --watch
# Uncensored 配置
mangosteen deploy qwen3.8-27b --template uncensored --gpu-model 3090 --watch
```

### 7. 自定义 API 模型名
```bash
mangosteen deploy qwen2.5-0.5b --gpu-model 3090 --alias my-qwen --watch
# API 调用时使用 "model": "my-qwen"
```

---

## 完整文档

- 全量指令说明：`docs/USAGE.md`
- 架构设计：`docs/DESIGN.md`