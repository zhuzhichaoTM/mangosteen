# 资源评估公式详解

## 总需求公式

```
总需求 = 权重 + KV Cache + 计算缓冲 + 运行时开销
```

## 各项详解

### 1. 权重（Weights）

**决定因素**：参数量 × 量化位宽

**典型值**（7B Q4_K_M）：
- 权重：4.36 GiB

**计算方式**：
- 权重大小 = 参数量 × 量化位宽（字节）
- Q4_K_M：平均每参数 4 bit = 0.5 字节
- 7B × 0.5 = 3.5 GB（实际会有一些元数据开销）

**常见量化格式**：
| 格式 | 位宽 | 特点 |
|------|------|------|
| Q4_K_M | 4 bit | 最常用，平衡精度和大小 |
| Q5_K_M | 5 bit | 精度更高，稍大 |
| Q8_0 | 8 bit | 高精度，大 |
| F16 | 16 bit | 全精度，很大 |

### 2. KV Cache（关键）

**决定因素**：层数 × KV头数 × head_dim × 上下文 × KV精度

**典型值**（7B Q4_K_M, 8K ctx）：
- KV Cache：0.44 GiB

**为什么单独算 KV Cache**：
- 消费级显卡上真正卡人的往往是它
- GQA 模型（如 Qwen2.5，28 heads / 4 KV heads）在 32K 上下文只需 ~1.7 GiB
- 不带 GQA 的老模型（如 Llama-2-7B，32 heads / 32 KV heads）8K 就要 4 GiB

**GQA（Grouped Query Attention）的影响**：
- GQA 减少 KV head 数量，大幅降低 KV Cache 占用
- Qwen2.5-7B：28 attention heads，4 KV heads（7:1）
- Llama-2-7B：32 attention heads，32 KV heads（1:1）

**KV Cache 计算公式**：
```
KV Cache = 2 × 层数 × KV头数 × head_dim × 上下文长度 × KV精度
```
- 2：Key 和 Value 两个缓存
- 层数：模型层数（如 32 层）
- KV头数：GQA 优化后的 KV head 数量
- head_dim：每个 head 的维度（如 128）
- 上下文长度：如 8192
- KV精度：f16 = 2 字节，q8_0 = 1 字节，q4_0 = 0.5 字节

**示例计算**（Qwen2.5-7B, 8K ctx, f16）：
- 层数：28
- KV头数：4
- head_dim：128
- 上下文：8192
- KV精度：2 字节

```
KV Cache = 2 × 28 × 4 × 128 × 8192 × 2
         = 2 × 28 × 4 × 128 × 8192 × 2
         = 2 × 28 × 4 × 128 × 16384
         = 2 × 28 × 4 × 2097152
         = 2 × 28 × 8388608
         = 2 × 234881024
         = 469762048 字节
         ≈ 448 MiB
```

### 3. 计算缓冲（Compute Buffer）

**决定因素**：批大小 × 隐藏维度

**典型值**（7B Q4_K_M, 8K ctx）：
- 计算缓冲：0.19 GiB

**作用**：
- 存储中间激活值
- 批处理时的临时空间

### 4. 运行时开销（Runtime Overhead）

**典型值**（7B Q4_K_M, 8K ctx）：
- 运行时开销：0.06–0.39 GiB

**组成**：
- CUDA context（NVIDIA GPU）
- Metal context（Apple Silicon）
- 线程栈
- 其他运行时数据结构

**平台差异**：
- macOS Metal：~0.06 GiB
- Linux CUDA：~0.39 GiB
- Windows CUDA：~0.3 GiB

## 量化自动降档策略

当显存放不下时，Mangosteen 会按以下顺序降级：

1. **降 KV 精度**：f16 → q8_0 → q4_0
2. **部分卸载**：部分层卸载到 CPU
3. **CPU 降级**：完全 CPU 推理

每一步都会在 `warnings` 里说明原因和影响。

## 实际案例

### 案例 1：Qwen2.5-7B Q4_K_M, 8K 上下文

| 组成 | 大小 |
|------|------|
| 权重 | 4.36 GiB |
| KV Cache | 0.44 GiB |
| 计算缓冲 | 0.19 GiB |
| 运行时开销 | 0.39 GiB |
| **总计** | **5.38 GiB** |

### 案例 2：Llama-3.3-70B Q4_K_M, 8K 上下文

| 组成 | 大小 |
|------|------|
| 权重 | 42.8 GiB |
| KV Cache | 4.2 GiB |
| 计算缓冲 | 1.5 GiB |
| 运行时开销 | 0.5 GiB |
| **总计** | **49.0 GiB** |

**结论**：70B 模型在 24GB 显存卡上无法本地部署，必须上云。

### 案例 3：Qwen2.5-7B Q4_K_M, 32K 上下文（GQA 优势）

| 组成 | 大小 |
|------|------|
| 权重 | 4.36 GiB |
| KV Cache | 1.7 GiB（GQA 优化） |
| 计算缓冲 | 0.19 GiB |
| 运行时开销 | 0.39 GiB |
| **总计** | **6.64 GiB** |

**对比无 GQA 模型**：32K 上下文需要 ~17 GiB KV Cache，总计 ~22 GiB

## 如何查看资源评估

```bash
# 只评估，不部署
mangosteen assess <模型名> --context <上下文长度>

# 示例
mangosteen assess qwen2.5-7b --context 8192
```

**输出示例**：
```
✓ 资源评估完成：qwen2.5-7b-instruct
  模型规格   Qwen2.5-7B Q4_K_M, 28 层, 28 heads / 4 KV heads
  上下文     8192
  权重       4.36 GiB
  KV Cache   0.44 GiB (f16)
  计算缓冲   0.19 GiB
  运行时开销 0.39 GiB
  总需求     5.38 GiB
  本地可用   24.0 GiB
  结论       ✓ 本地资源充足
```

## 调度决策

根据资源评估结果，调度器会自动决策：

```
本地资源满足 → 本地部署（永远优先）
本地资源不足 → 云端降级（只在必要时）
本地部署失败 → 云端降级 + 记录原因
```

**本地优先策略**在三个层面被强制：
- 配置层：`scheduler.local_first` 默认 `true`
- 代码层：`Scheduler::decide()` 的本地判断在云端构造之前
- 测试层：`tests/layering.rs` 里的 `c4_*` 用例会扫描源码验证顺序
