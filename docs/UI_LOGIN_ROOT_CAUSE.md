# UI「连接中」故障根因分析报告

日期：2026-11-20
影响版本：0.1.0（安全加固分支 safety）
严重级别：P0（控制台完全不可用）

## 一、现象

用户访问 `http://127.0.0.1:11516/` 登录后：

1. 模型下拉永远显示「（加载中…）」，永不出现模型列表
2. 对话/部署/设置三个页签点击无反应（无法切换）
3. 页面底部导航栏状态永远「连接中…」
4. 开发者工具报 CSP 错误：`Executing inline script violates ... script-src 'self'`
5. audit 日志中零 `/api/status` 请求（浏览器从未调用后端 API）

## 二、排查过程与逐层排除

### 第 1 层：怀疑 CSP 拦截（表象）

- 现象：控制台报 `script-src 'self'` 违规
- 排查：curl 验证服务端返回的 CSP 头只有宽松版（`script-src 'unsafe-inline'`）
- 结论：服务端 CSP 正确；但历史上确实存在过「登录页/控制台页返回全局严格 CSP」的问题（已在前序提交 cc85a1f 等修复）。**CSP 是干扰项，不是本次根因**

### 第 2 层：怀疑浏览器缓存

- 排查：用 `?nocache=1` 参数强制绕过缓存新建标签页，问题依旧
- 结论：**排除浏览器缓存**

### 第 3 层：怀疑 localStorage / cookie 认证

- audit 日志显示 `GET /api/status` 有 401 也有 200（带 token）
- curl 三种认证方式（仅 cookie / 仅 Bearer / 双带）全部 200
- 结论：认证链路正常

### 第 4 层：audit 日志零请求（决定性证据）

- 浏览器加载控制台后 12 秒内，audit 日志只有 runprobe 的 `/health`，**零 `/api/status`**
- 说明：不是「请求失败」，而是「JS 从未执行到发起请求那一步」
- 范围缩小到：**内联 `<script>` 整体没有执行**

### 第 5 层：node --check 语法检查（根因定位）

```bash
# 提取页面实际返回的内联 script
curl -s -b "ui_session=$TOKEN" http://127.0.0.1:11516/ \
  | python3 -c "import sys,re; m=re.search(r'<script>(.*?)</script>', sys.stdin.read(), re.S); open('/tmp/s.js','w').write(m.group(1))"
node --check /tmp/s.js
```

输出：

```
/tmp/console_script.js:613
    },
     ^
SyntaxError: Unexpected token ','
```

## 三、根因

前一轮给 `refresh()` 添加「失败时显示明确错误提示」时（提交 cea571308224），
catch 块的闭合括号误写为 `},`（对象属性逗号）而非 `}`：

```js
// 错误写法（引入语法错误的版本）
      } catch (e) {
        ...
        throw e;
    },              ← 错误：catch 块用「} ,」闭合，且缩进错乱
  };
})());

// 正确写法（修复后）
      } catch (e) {
        ...
        throw e;
      }             ← catch 块正确闭合
    },
  };
})());
```

**一个字符的语法错误导致整个 68KB 控制台内联 script 解析失败，
所有 JS（轮询、页签绑定、模型加载）一概不执行。**

## 四、为什么之前的多轮「修复」都没解决

| 轮次 | 采取的措施 | 实际效果 |
|------|-----------|----------|
| 1 | 登录页加宽松 CSP（HTTP 头） | 登录页可用（独立小 script 未受影响），控制台依旧 |
| 2 | 登录页加 meta CSP | 同上 |
| 3 | localStorage gw_token 写入 | 登录态正常（cookie 路径），但非根因 |
| 4 | runprobe `--resolve` 修复 | 修复了 run_status unactive，但 UI 依旧 |
| 5 | 控制台页宽松 CSP（cc85a1f） | 消除了潜在 CSP 风险，但 JS 仍因语法错误不执行 |
| 6 | 重启网关加载新二进制 | 无济于事（问题在 HTML 内容本身） |
| 7 | **node --check 语法检查** | **一步定位：ca48548 修复** |

**教训**：前端内联 script 的任何改动，必须用 `node --check` 验证语法；
「页面显示但功能全无 + audit 零请求」应首先怀疑 JS 整体未执行，而不是网络层。

## 五、修复与验证

修复提交：`ca48548`（`},` → `}`）

浏览器实测验证（全部通过）：

| 验证项 | 结果 |
|--------|------|
| 内联 script 语法 | node --check OK |
| 模型下拉 | 「（待部署模型）」+ 提示「请先在「部署」页启动一个模型」 |
| 后台轮询 | audit 日志持续 `GET /api/status 200`（localStorage token 生效） |
| 对话页签 | 完整渲染（输入框/快捷问题/发送按钮） |
| 部署页签 | 模型下拉（Qwen2.5-0.5B / Qwen3.8-27B）+ 开始/停止/详情按钮 |
| 设置页签 | 节点管理 + 模板管理按钮 |

## 六、遗留的正向加固（保留）

以下前序修复虽然不是本次根因，但均为必要防护，继续保留：

1. 登录页/控制台页宽松 CSP（防全局严格版误伤内联 JS）
2. runprobe TLS `--resolve`（run_status 正确性）
3. API_TIMEOUT 180s→10s（失败快速反馈）
4. refresh 失败时明确提示（本次语法修复后生效）