# Mangosteen 常见问题 FAQ

## 安装与配置

### Q1: Mangosteen 支持哪些平台？

**A**: 支持 macOS、Windows、Linux 三个平台，每个平台提供 x86_64 和 aarch64 两种架构的发布包。

### Q2: 如何安装 Mangosteen？

**A**: 有三种方式：
1. 从 Release 下载对应平台的压缩包，解压到 PATH 目录
2. 从源码编译：`cargo build --release`
3. 使用辅助脚本检测平台：`bash scripts/detect-platform.sh`

### Q3: 配置文件在哪里？

**A**:
- macOS: `~/Library/Application Support/mangosteen/config.toml`
- Linux: `~/.config/mangosteen/config.toml`
- Windows: `%APPDATA%\mangosteen/config.toml`

查看配置位置：`mangosteen config path`

### Q4: 如何重置配置文件？

**A**:
```bash
mangosteen config init --force
```
注意：这会覆盖现有配置，请先备份。

## 部署相关

### Q5: 部署失败，提示"无可用云端算力节点"怎么办？

**A**: 这是配置问题，可能是：
1. 配置文件中 `model_whitelist` 强制使用 `device: ssh`
2. 没有配置 `remote_devices`
3. 云服务商未启用或余额不足

**解决方法**：
1. 检查配置：`mangosteen config show`
2. 简化配置：移除 `model_whitelist` 和 `remote_devices`
3. 如果需要云端，启用云服务商并充值

### Q6: 本地部署很慢怎么办？

**A**: CPU 推理速度通常 <5 tok/s，这是正常的。如果需要更快：
1. 配置云服务商，使用 GPU 实例
2. 本地安装 GPU 并安装对应驱动
3. 使用更小的模型或更低的量化精度

### Q7: 如何查看当前部署状态？

**A**:
```bash
mangosteen status    # 查看活跃部署
mangosteen ps        # 列出全部部署记录
```

### Q8: 如何停止所有部署？

**A**:
```bash
mangosteen stop --all --yes
```

## 云端相关

### Q9: 云端实例会自动停止吗？

**A**: 不会。云端实例按小时计费，必须手动停止：
```bash
mangosteen stop <部署ID>
```
或
```bash
mangosteen stop --all
```

**重要**：用完必须停止，否则会持续计费。

### Q10: 如何查看云端实例？

**A**:
```bash
mangosteen cloud instances
```

### Q11: 云端实例忘记关了怎么办？

**A**:
```bash
mangosteen cloud instances    # 查看实例
mangosteen stop <部署ID>      # 停止实例
```

台账会记录所有实例，建议定期检查。

### Q12: 如何选择最便宜的云端实例？

**A**:
```bash
mangosteen cloud cheapest --min-vram <显存GB>
```

### Q13: 云端部署如何收费？

**A**: 按小时计费，关机不收费（按量计费）。不同规格价格不同，可用 `mangosteen cloud specs` 查看。

## 模型相关

### Q14: 如何下载模型？

**A**:
```bash
mangosteen models pull <完整模型路径>
```

示例：
```bash
mangosteen models pull Qwen/Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_k_m.gguf
```

### Q15: 如何查看本地模型？

**A**:
```bash
mangosteen models list
```

### Q16: 如何添加已有模型？

**A**:
```bash
mangosteen models add <本地路径>
```

### Q17: 模型下载失败怎么办？

**A**:
1. 切换源：`mangosteen models pull hf:<模型>`
2. 检查网络连接
3. 使用代理（如需要）
4. 重新下载（支持断点续传）

### Q18: 如何评估模型资源需求？

**A**:
```bash
mangosteen assess <模型> --context <上下文长度>
```

## 引擎相关

### Q19: 引擎动态库找不到怎么办？

**A**:
1. 查看注入路径：`mangosteen engine env`
2. 检查引擎位置：`mangosteen engine locate`
3. 手动指定（如果需要）：在配置中设置 `local.engine_path`

### Q20: llama-server 从哪里获取？

**A**:
- macOS/Linux: `brew install llama.cpp`
- 从 Release 下载随包分发的运行时
- Mangosteen 会自动注入库搜索路径

### Q21: 如何检查引擎版本？

**A**:
```bash
mangosteen engine probe
```

## 端口与网络

### Q22: 端口被占用怎么办？

**A**:
```bash
mangosteen stop --all
```
或修改配置中的 `gateway.port`

### Q23: 默认端口是多少？

**A**: 11516（可在配置中修改）

### Q24: 如何修改网关端口？

**A**: 编辑配置文件：
```toml
[gateway]
port = 8080
```

## OpenAI 兼容接口

### Q25: 如何使用 OpenAI SDK 访问？

**A**:
```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:11516/v1",
    api_key="not-needed"
)

resp = client.chat.completions.create(
    model="auto",
    messages=[{"role": "user", "content": "你好"}],
)
print(resp.choices[0].message.content)
```

### Q26: model 参数应该填什么？

**A**: 填 `"auto"` 表示使用当前部署的模型，或填具体的模型 ID。

## 故障排查

### Q27: 如何运行环境自检？

**A**:
```bash
mangosteen doctor
```

### Q28: 如何查看日志？

**A**: 日志目录：
- macOS: `~/Library/Logs/mangosteen/`
- Linux: `~/.local/share/mangosteen/logs/`
- Windows: `%LOCALAPPDATA%\mangosteen\logs/`

### Q29: 如何验证配置文件？

**A**:
```bash
mangosteen config validate
```

## 性能优化

### Q30: 如何提高推理速度？

**A**:
1. 使用 GPU（本地或云端）
2. 降低上下文长度
3. 使用更好的量化格式（Q5_K_M > Q4_K_M）
4. 增加批大小（batch_size）
5. 使用 GQA 模型（如 Qwen2.5）

### Q31: 如何降低显存占用？

**A**:
1. 降低 KV 精度（f16 → q8_0 → q4_0）
2. 降低上下文长度
3. 使用部分卸载
4. 使用更小的模型

## 安全与成本

### Q32: 云端凭证如何安全存储？

**A**: 支持三种方式：
1. 环境变量（推荐）：`secret = { env = "MY_TOKEN" }`
2. 文件：`secret = { file = "/run/secrets/tok" }`
3. 内联（仅调试）：`secret = { inline = "xxx" }`

`config show` 不会明文输出凭证。

### Q33: 如何控制云端成本？

**A**:
1. 设置价格上限：`scheduler.max_price_per_hour`
2. 用完立即停止：`mangosteen stop --all`
3. 定期检查实例：`mangosteen cloud instances`
4. 使用 cheapest 命令选择最便宜的实例

## 其他

### Q34: 支持 Web UI 吗？

**A**: 支持：
```bash
mangosteen ui
```

### Q35: 如何查看版本信息？

**A**:
```bash
mangosteen --version
```

### Q36: 如何获取帮助？

**A**:
```bash
mangosteen --help
mangosteen <命令> --help
```

### Q37: 如何提交 Issue？

**A**: 访问 https://github.com/zhuzhichaoTM/mangosteen/issues

提交时请包含：
- mangosteen 版本：`mangosteen --version`
- 操作系统
- 完整错误信息
- 复现步骤
- 相关日志（脱敏后）
