# 故障排查手册

## 环境自检

### 完整自检

```bash
mangosteen doctor
```

Doctor 会检查：
- 本地推理引擎是否存在且可运行
- 配置文件是否合法
- 云服务商凭证是否配置
- 端口是否被占用
- 模型目录是否存在

## 引擎相关问题

### 问题 1：引擎动态库找不到（最常见）

**错误信息**：
```
[FAIL] 本地推理引擎
       /usr/local/bin/llama-server 存在但无法运行：Library not loaded:
       @rpath/libllama-server-impl.dylib ... no LC_RPATH's found
```

**原因**：Homebrew 装的 `llama-server` 常常依赖 `@rpath/libllama-server-impl.dylib`，但二进制里**没有写 `LC_RPATH`**。

**解决方案**：

Mangosteen 已经自动注入库搜索路径，如果仍然失败：

1. 查看注入的库路径
   ```bash
   mangosteen engine env
   ```
   输出示例：
   ```
   二进制                       /usr/local/bin/llama-server
   DYLD_FALLBACK_LIBRARY_PATH  /usr/local/bin:/usr/local/lib:/usr/lib
   ```

2. 查看搜索过的目录
   ```bash
   mangosteen engine locate
   ```

3. 确认库的真实位置后，在配置里手工指定
   ```toml
   [local.engine_env]
   DYLD_FALLBACK_LIBRARY_PATH = "/opt/my-llama/lib"  # macOS/Linux
   ```

**为什么需要这一步**：
- Homebrew 装的 `llama-server` 常常依赖 `@rpath/libllama-server-impl.dylib`
- 直接执行会报 `Library not loaded ... no LC_RPATH's found`
- 而 `.dylib` 其实就在 `/usr/local/lib` 里
- Mangosteen 启动引擎前会自动探测库目录并注入 `DYLD_FALLBACK_LIBRARY_PATH`（Linux 为 `LD_LIBRARY_PATH`）
- 所以**不需要你手工设环境变量**

### 问题 2：架构不匹配

**错误信息**：
```
Bad CPU type in executable
```

**原因**：Homebrew 在 Apple Silicon 上装到了 `/usr/local`（Intel 路径），或反之。

**解决方案**：

1. 用 `mangosteen engine locate` 看实际路径
2. 必要时用 `local.engine_path` 指向正确的那份
   ```toml
   [local]
   engine_path = "/opt/homebrew/bin/llama-server"  # Apple Silicon 正确路径
   ```

### 问题 3：引擎根本没装

**错误信息**：
```
[FAIL] 本地推理引擎
       未找到 llama-server。已搜索：/usr/local/bin: /usr/bin: ...
```

**解决方案**：

Doctor 会把**搜索过的目录**全列出来——这比让你猜 PATH 有没有配更直接。

安装方式：
```bash
# macOS / Linux
brew install llama.cpp

# 或从 Release 下载随包分发的运行时（完全自包含）
# 把 llama-server 与它的 .dylib/.so 一起放到 mangosteen 同级目录：
# <mangosteen 所在目录>/runtime/darwin-x86_64/llama-server
# <mangosteen 所在目录>/runtime/darwin-x86_64/lib*.dylib
```

查找顺序：`local.engine_path` → 可执行文件同级 `runtime/<平台-架构>/` → `PATH`。

## 部署失败

### 查看失败原因

每次部署都会落台账（`state.json`），失败原因在里面：

```bash
mangosteen ps                          # 看状态
mangosteen status                      # 看活跃部署 + 引擎路径
```

### 查看引擎日志

引擎日志在 `<日志目录>/engine-<端口>.log`，错误消息里会给出完整路径。

### 常见失败原因

1. **显存不足**
   - 解决：降低上下文长度，或使用云端部署
   - 评估：`mangosteen assess <模型> --context <长度>`

2. **端口被占用**
   - 解决：`mangosteen stop --all`
   - 或改配置 `local.port_range_start` / `port_range_end`

3. **模型文件损坏**
   - 解决：重新下载 `mangosteen models pull <模型>`
   - 验证：`mangosteen models verify <模型>`

4. **云端实例创建失败**
   - 检查凭证是否正确
   - 检查库存是否充足
   - 检查余额是否充足

## 云端实例忘了关

### 查看云端实例

```bash
mangosteen cloud instances
```

### 停止实例

```bash
mangosteen ps                    # 找出来
mangosteen stop <部署ID>          # 停掉
mangosteen stop --all --yes      # 或者全停
```

### 销毁裸实例

```bash
mangosteen cloud destroy --instance compshare/uhost-xxxx --yes
```

**重要提醒**：
- 台账是**每次状态变更立即落盘**的，进程重启也不会丢
- 这是为了避免忘记关机导致持续扣费
- 裸实例不在部署台账里，不会被 `stop/abort` 自动回收，必须手动销毁

## 端口被占用

### 清理残留进程

```bash
mangosteen stop --all
```

### 修改端口范围

编辑配置文件：

```toml
[local]
port_range_start = 11435
port_range_end = 11999
```

## 配置问题

### 查看配置

```bash
mangosteen config show
mangosteen config path
```

### 验证配置

```bash
mangosteen config validate
```

### 重新生成配置

```bash
mangosteen config init
```

## 模型下载问题

### 下载失败

**原因**：
- 网络抖动
- 源服务器不可用
- 磁盘空间不足

**解决方案**：
- 断点续传：重新执行同一条 `pull` 会从断点继续
- 切换源：`mangosteen models pull hf:<模型>` 或 `mangosteen models pull myhub:<模型>`
- 检查磁盘空间

### SHA-256 校验失败

**原因**：文件损坏或不完整

**解决方案**：
```bash
mangosteen models remove <模型>
mangosteen models pull <模型>
```

### 下载很慢

**原因**：
- 默认使用 ModelScope（国内），通常较快
- 如果使用 HuggingFace，可能较慢

**解决方案**：
- 使用 ModelScope（默认）
- 配置代理（如需要）
- 使用自定义源（内网私有仓库）

## SSH 连接问题

### 连接超时

**原因**：
- 云端实例冷启动窗口可达 90s
- 网络延迟
- 防火墙阻止

**解决方案**：
- 等待冷启动完成（`confirm_remote_reachable` 30×3s）
- 检查网络连接
- 检查防火墙规则

### 认证失败

**原因**：
- 密钥配置错误
- 密钥过期

**解决方案**：
- 检查凭证配置
- 更新密钥

**SSH 一律密钥认证**：密码只用于第一次装公钥；`platform/ssh.rs` 是平台差异唯一收口。

## Web UI 问题

### 无法打开浏览器

**解决方案**：
```bash
mangosteen ui --no-open
# 然后手动访问 http://127.0.0.1:11516
```

### 前端无法加载

**原因**：
- 前端是内嵌在二进制里的单文件 HTML
- 不引 CDN、不引框架、没有构建步骤
- 离线与内网环境也能正常使用

**解决方案**：
- 检查 mangosteen 是否正常运行
- 检查端口是否被占用
- 检查防火墙规则

## 性能问题

### 推理速度慢

**可能原因**：
- CPU 降级
- 部分卸载
- 上下文过长
- 模型量化格式不合理

**解决方案**：
- 检查部署状态：`mangosteen status`
- 降低上下文长度
- 使用更好的量化格式（如 Q5_K_M）
- 使用云端 GPU

### 内存占用高

**可能原因**：
- KV Cache 过大
- 多个部署同时运行

**解决方案**：
- 降低上下文长度
- 停止不需要的部署：`mangosteen stop <ID>`
- 使用 GQA 模型（如 Qwen2.5）

## 跨平台问题

### macOS 特有问题

1. **Library not loaded**
   - 见"引擎动态库找不到"章节

2. **Apple Silicon 与 Intel 混用**
   - 见"架构不匹配"章节

### Windows 特有问题

1. **路径分隔符**
   - Mangosteen 使用 `PathBuf`，自动处理路径分隔符
   - 配置文件中可以使用 `/` 或 `\`

2. **防火墙**
   - 可能需要允许 mangosteen 通过防火墙
   - 端口 11516 需要放行

### Linux 特有问题

1. **权限问题**
   - 确保对模型目录有读写权限
   - 确保对配置目录有读写权限

2. **SELinux**
   - 可能需要调整 SELinux 策略

## 获取帮助

### 查看详细日志

日志目录位置：
- macOS: `~/Library/Logs/mangosteen/`
- Linux: `~/.local/share/mangosteen/logs/`
- Windows: `%LOCALAPPDATA%\mangosteen\logs\`

### 提交 Issue

提交 Issue 时请包含：
- mangosteen 版本：`mangosteen --version`
- 操作系统：`uname -a`（Linux/macOS）或系统信息（Windows）
- 完整错误信息
- 复现步骤
- 相关日志（脱敏后）

## 常用诊断命令

```bash
# 完整自检
mangosteen doctor

# 查看配置
mangosteen config show

# 查看状态
mangosteen status

# 查看部署
mangosteen ps

# 查看云端实例
mangosteen cloud instances

# 查看引擎环境
mangosteen engine env

# 查看引擎搜索路径
mangosteen engine locate

# 评估资源
mangosteen assess <模型> --context <长度>
```
