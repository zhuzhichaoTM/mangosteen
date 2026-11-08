# Mangosteen Skill 本地测试指南

## 测试目标

验证 mangosteen-llm-deployer skill 在 WorkBuddy 中的有效性和可用性。

## 测试方法

### 方法 1：WorkBuddy 内功能测试（推荐）⭐

#### 测试 1：Skill 加载测试

**步骤**：
1. 打开 WorkBuddy
2. 输入："部署 qwen2.5-7b 模型，上下文 8192"
3. 观察 skill 是否自动触发

**预期结果**：
- Skill 自动触发并加载
- 显示 skill 的指导内容
- 包含前置条件检查步骤

#### 测试 2：环境检查测试

**步骤**：
1. 在 WorkBuddy 中输入："帮我检查 mangosteen 环境"
2. 观察 skill 是否指导执行环境检查

**预期结果**：
- 调用 `mangosteen doctor`
- 输出环境检查结果
- 提供下一步指引

#### 测试 3：资源评估测试

**步骤**：
1. 在 WorkBuddy 中输入："评估 llama3.3-70b 能不能在本地跑"
2. 观察 skill 是否指导执行资源评估

**预期结果**：
- 调用 `mangosteen assess llama3.3-70b --context 8192`
- 输出资源需求（显存/内存）
- 输出本地是否足够

#### 测试 4：文档引用测试

**步骤**：
1. 在 WorkBuddy 中输入："如何配置优云智算"
2. 观察 skill 是否引用参考文档

**预期结果**：
- 引用 `@references/cloud-providers.md`
- 显示配置指引
- 提供配置模板

---

### 方法 2：脚本单元测试

#### 测试所有脚本

```bash
# 进入 skill 目录
cd ~/.workbuddy/skills/mangosteen-llm-deployer

# 运行自测脚本
bash scripts/test-skill.sh
```

**预期结果**：
```
总测试数: 43
通过: 43
失败: 0
✅ 所有测试通过！
```

#### 测试单个脚本

```bash
# 平台检测
bash scripts/detect-platform.sh

# 环境健康检查
bash scripts/health-check.sh

# 环境准备
bash scripts/prepare-env.sh
```

---

### 方法 3：手动功能测试

#### 测试 1：发布包解压测试

```bash
# 解压发布包
cd /tmp
unzip ~/.workbuddy/skills/mangosteen-ellm-deployer/dist/mangosteen-llm-deployer-1.0.0.zip

# 验证文件完整性
cd mangosteen-llm-deployer
bash scripts/test-skill.sh
```

#### 测试 2：生成脚本测试

```bash
# 重新生成发布包
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/generate.sh

# 验证生成结果
ls -lh ~/.workbuddy/skills/mangosteen-llm-deployer/dist/
```

---

### 方法 4：完整流程测试（需要 mangosteen 已安装）

#### 前置条件

确保 mangosteen 已安装并可用：

```bash
# 检查安装
mangosteen --version

# 运行环境自检
mangosteen doctor
```

#### 测试环境准备脚本

```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/prepare-env.sh
```

**预期输出**：
- 检查 mangosteen 是否安装
- 运行环境自检
- 检查配置文件
- 检查本地模型

#### 测试快速部署脚本

```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/quick-deploy.sh qwen2.5-0.5b-instruct 8192
```

**预期输出**：
- 检查 mangosteen 是否安装
- 评估资源需求
- 确认部署
- 执行部署（需要 mangosteen 已安装）

---

## 测试检查清单

### 文件完整性检查

- [ ] SKILL.md 存在
- [ ] README.md 存在
- [ ] QUICKSTART.md 存在
- [ ] CHANGELOG.md 存在
- [ ] references/ 目录包含 5 个文档
- [ ] scripts/ 目录包含 9 个脚本
- [ ] assets/ 目录存在
- [ ] 发布包存在

### 功能性检查

- [ ] Skill 可以在 WorkBuddy 中加载
- [ ] 前置条件检查正确执行
- [ ] 资源评估正确执行
- [ ] 文档引用正确加载
- [ ] 脚本可以正确调用

### 脚本可用性检查

- [ ] 所有脚本可执行
- [ ] 脚本语法正确
- [ ] 脚本功能正常

---

## 测试结果记录

### 自动化测试结果

```
总测试数: 43
通过: 43
失败: 0
通过率: 100%
```

### 手动测试结果

- [ ] WorkBuddy 内功能测试：待执行
- [ ] 脚本单元测试：已通过
- [ ] 发布包解压测试：待执行
- [ ] 完整流程测试：待执行（需要 mangosteen）

---

## 常见问题

### Q1: Skill 没有自动触发？

**A**: 检查触发关键词是否匹配：
- "部署模型"
- "评估资源"
- "云端算力"
- "模型下载"
- "mangosteen"

### Q2: 脚本无法执行？

**A**: 检查脚本权限：
```bash
chmod +x ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/*.sh
```

### Q3: 测试失败怎么办？

**A**:
1. 查看错误信息
2. 检查脚本语法：`bash -n scripts/xxx.sh`
3. 查看日志：如果 mangosteen 已安装，查看日志目录

### Q4: 如何重新生成发布包？

**A**:
```bash
bash ~/.workbuddy/skills/mangosteen-llm-deployer/scripts/generate.sh
```

---

## 测试报告模板

测试完成后，可以记录测试结果：

```
测试日期：YYYY-MM-DD
测试人员：
测试环境：macOS / Windows / Linux

自动化测试：
- 总测试数：43
- 通过：43
- 失败：0
- 通过率：100%

WorkBuddy 内测试：
- Skill 加载：通过/失败
- 环境检查：通过/失败
- 资源评估：通过/失败
- 文档引用：通过/失败

脚本测试：
- detect-platform.sh：通过/失败
- health-check.sh：通过/失败
- prepare-env.sh：通过/失败
- quick-deploy.sh：通过/失败

问题记录：
1. ...
2. ...

结论：
- Skill 是否可用：是/否
- 是否可以发布：是/否
```

---

## 下一步

### 如果测试全部通过

1. 上传到 WorkBuddy 开放平台
2. 发布到 SkillHub
3. 通知团队成员使用

### 如果测试失败

1. 查看具体失败的测试项
2. 根据错误信息修复问题
3. 重新运行测试
4. 直到所有测试通过
