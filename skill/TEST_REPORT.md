# Mangosteen Skill 本地自测方案

## 自测目标

验证 mangosteen-llm-deployer skill 的：
1. **文件完整性**：所有必需文件存在且格式正确
2. **文档完整性**：参考文档内容充足
3. **脚本可用性**：所有脚本可执行且语法正确
4. **Skill 可加载性**：能在 WorkBuddy 中正确加载
5. **实际可用性**：能在实际场景中正确工作

## 自测方法

### 方法 1：自动化脚本测试（已完成）✅

**执行命令**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/test-skill.sh
```

**测试结果**：
```
总测试数: 43
通过: 43
失败: 0
✅ 所有测试通过！Skill 已准备好发布。
```

**测试覆盖**：
1. 必需文件检查（4 个文件）
2. SKILL.md frontmatter 验证（10 个字段）
3. references 文档检查（5 个文档）
4. scripts 脚本检查（5 个脚本）
5. Skill 加载测试（需要在 WorkBuddy 中）
6. 文档引用检查（5 个引用）
7. JSON 文件格式验证（2 个文件）
8. 脚本语法检查（5 个脚本）
9. 文档字数统计（5 个文档）
10. 发布包检查（1 个包）

### 方法 2：WorkBuddy 内功能测试（需要手动执行）

#### 测试 1：Skill 加载测试

**步骤**：
1. 在 WorkBuddy 中输入："部署 qwen2.5-7b 模型"
2. 观察 skill 是否自动触发
3. 检查 skill 是否正确加载

**预期结果**：
- Skill 自动触发
- 显示 skill 的指导内容
- 不会出现加载错误

#### 测试 2：前置条件检查测试

**步骤**：
1. 在 WorkBuddy 中执行 skill 触发的命令
2. 检查是否正确执行环境检查
3. 验证输出是否符合预期

**预期结果**：
- 执行 `mangosteen --version`
- 执行 `mangosteen doctor`
- 输出环境检查结果

#### 测试 3：资源评估测试

**步骤**：
1. 在 WorkBuddy 中："评估 llama3.3-70b 能不能在本地跑"
2. 观察 skill 是否正确指导执行资源评估
3. 验证输出是否包含资源需求信息

**预期结果**：
- 执行 `mangosteen assess llama3.3-70b --context 8192`
- 输出显存/内存需求
- 输出本地是否足够

#### 测试 4：文档引用测试

**步骤**：
1. 在 WorkBuddy 中询问："如何配置云服务商"
2. 观察 skill 是否引用 @references/cloud-providers.md
3. 验证参考文档是否正确加载

**预期结果**：
- Skill 引用参考文档
- 参考文档内容正确显示
- 用户获得配置指引

#### 测试 5：脚本调用测试

**步骤**：
1. 在 WorkBuddy 中："帮我检查环境"
2. 观察 skill 是否调用 prepare-env.sh
3. 验证脚本输出是否符合预期

**预期结果**：
- Skill 调用 scripts/prepare-env.sh
- 输出环境检查结果
- 提供下一步指引

### 方法 3：手动功能测试（可选）

#### 测试 1：平台检测脚本

**执行**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/detect-platform.sh
```

**预期输出**：
```
=== Mangosteen 平台检测 ===
操作系统: macos
架构: x86_64
推荐的下载文件: mangosteen-macos-x86_64.tar.gz
```

#### 测试 2：环境健康检查脚本

**执行**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/health-check.sh
```

**预期输出**：
```
=== Mangosteen 环境健康检查 ===
✓ Mangosteen 已安装
✓ 配置文件存在
✓ 引擎配置正常
...
```

#### 测试 3：环境准备脚本

**执行**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/prepare-env.sh
```

**预期输出**：
```
=== Mangosteen 环境准备 ===
✓ Mangosteen 已安装
✓ 配置文件存在
⚠️ 配置文件中包含 device: ssh
...
```

#### 测试 4：快速部署脚本（需要 mangosteen 已安装）

**执行**：
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/quick-deploy.sh qwen2.5-0.5b-instruct 8192
```

**预期输出**：
```
=== Mangosteen 快速部署 ===
模型: qwen2.5-0.5b-instruct
上下文: 8192
✓ Mangosteen 已安装
步骤 1: 评估资源需求...
...
```

### 方法 4：发布包解压测试

**步骤**：
1. 解压发布包：
```bash
cd /tmp
unzip ~/.workbuddy/skills/mangosteen-llm-deployer/dist/mangosteen-llm-deployer-1.0.0.zip
```

2. 验证文件完整性：
```bash
cd mangosteen-llm-deployer
bash scripts/test-skill.sh
```

**预期结果**：
- 所有文件正确解压
- 自测脚本全部通过

## 自测结果总结

### 自动化测试结果 ✅

- **总测试数**：43
- **通过**：43
- **失败**：0
- **通过率**：100%

### 测试覆盖范围

| 类别 | 测试项 | 状态 |
|------|--------|------|
| 文件完整性 | 4 个必需文件 | ✅ |
| Frontmatter | 10 个字段 | ✅ |
| 参考文档 | 5 个文档 | ✅ |
| 脚本可用性 | 5 个脚本 | ✅ |
| 文档引用 | 5 个引用 | ✅ |
| JSON 格式 | 2 个文件 | ✅ |
| 脚本语法 | 5 个脚本 | ✅ |
| 文档字数 | 5 个文档 | ✅ |
| 发布包 | 1 个包 | ✅ |
| Skill 加载 | 需在 WorkBuddy 中 | ⏳ |

### 有效性证明

1. **文件结构正确**：所有必需文件存在，符合 WorkBuddy skill 规范
2. **文档内容充足**：参考文档平均 600+ 词，内容详细
3. **脚本可用**：所有脚本可执行且语法正确
4. **发布包完整**：48K 压缩包包含所有必要文件
5. **JSON 格式正确**：元数据文件格式验证通过

### 可用性证明

需要通过 WorkBuddy 内的功能测试验证：
- Skill 加载
- 前置条件检查
- 资源评估
- 文档引用
- 脚本调用

## 下一步建议

### 立即执行

1. **在 WorkBuddy 中测试 Skill 加载**
   - 触发关键词："部署模型"
   - 验证 skill 是否自动加载
   - 检查输出是否正确

2. **测试核心工作流**
   - 资源评估
   - 云端选型
   - 模型管理
   - 故障排查

### 可选执行

1. **手动测试所有脚本**
   - detect-platform.sh
   - health-check.sh
   - prepare-env.sh
   - quick-deploy.sh（需要 mangosteen）

2. **解压测试发布包**
   - 验证解压后的文件完整性
   - 重新运行自测脚本

## 结论

✅ **自动化测试全部通过**（43/43）

Skill 已通过本地自动化测试，文件结构、文档内容、脚本可用性、发布包完整性均已验证。建议在 WorkBuddy 中进行功能测试，验证实际使用场景中的表现。
