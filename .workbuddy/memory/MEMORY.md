# Mangosteen 项目长期记忆

## 身份与架构
- 个人级大模型部署编排客户端，Apache-2.0，仓库 `/Users/zhuzhichao/Documents/mangosteen`
- 纯云端部署（本地推理交 Ollama）；五层：交互 / 网关(唯一收口) / 编排 / 执行 / 基础
- 新增 crate 必改三处：根 Cargo.toml、cli/tests/layering.rs、cli/tests/release_assets.rs

## 不可违反基线
1. 网关唯一收口：禁止上层直连引擎/云厂商API
2. 引擎插件化：新增框架只加 EnginePlugin（网关禁现 `InferenceEngine::` 字面量）
3. 多云插件抽象：新增服务商只加驱动
4. 平台差异收敛 mangosteen-platform（cfg(target_os) 禁止出现在 platform 之外）
5. 配置驱动；不集群/多租户/分布式；云端优先 4090/5090

## 多平台兼容（用户强制，最高优先级）
**必须同时跑 macOS/Windows/Linux，"只在本机能跑"是缺陷。**
- 禁止业务代码出现 /dev/null、/bin/sh、~ 展开、$HOME、ps/pgrep/kill 等 POSIX 假设
- 路径一律 PathBuf；外部命令经 platform::process；平台分支必须补测试
- 历史欠账见 `.workbuddy/memory/platform-audit.md`

## 并发模型（2026-11 重构定稿，勿回退）
- **Gateway = Arc<Orchestrator> 只读共享，HTTP 层零全局锁**（51 处 lock 已移除）
- Orchestrator 可变状态全部内部锁：cloud(Mutex<Option>)、scheduler(RwLock)、
  config(RwLock<Arc<Config>>，读侧 clone Arc)、routes/device_tunnels(Mutex)、
  CloudOrchestrator.tunnels(Mutex)、Scheduler.local_ports(Mutex)
- 部署长操作绝不持上层锁；管理 API / UI 轮询不再被部署阻塞
- **教训：Mutex 同一表达式 lock 两次 = 同线程自锁死**（config_port_span 曾因此
  挂死 e2e）；guard 跨行必须显式绑定 `let g = self.lock()`；改锁后必须抓栈验证
  （macOS `sample <pid>`）

## 技术约定
- Rust 2021, MSRV 1.75，依赖极简（serde/toml/sha2/libc/windows-sys）；HTTP 手写、curl 直用、无 clap
- 提交前必跑 `cargo test --workspace` 零警告；unwrap <3/kloc；统一 MnError
- 测试不写死端口；TestEnv::new(tag) 哈希派生端口段；write_gguf 模型名固定 qwen2.5-7b-instruct
- **测试严禁写真实配置**：种子写入必须用注入的 ConfigStore

## 提交与版本控制规则（用户强制）
- 编译验证 OK 就自动本地 commit，不等确认；格式：`feat|fix|refactor(<范围>): <中文摘要>`
  + 正文一句话 + 「支撑改动：」+「验证：」+ `Co-Authored-By: Claude <noreply@anthropic.com>`
- 逻辑独立改动拆多个 commit；遗留改动先确认范围

## 云端部署(多厂商通用)
- CloudPlan 二选一：download_url(llama.cpp) 或 remote_model(Ollama)；都 None 报错
- 云端 gpu_layers = spec.n_layers；创建失败按价格换下一个候选；鉴权/余额立刻中止
- 销毁用 destroy_with_retry（8903 状态变更重试）；销毁前必须先关机
- **SSH 一律密钥认证**：密码只用于第一次装公钥；platform/ssh.rs 是平台差异唯一收口
- scheduler.require_stock 关（优云库存是模拟）；未知价格 f64::INFINITY 排最后
- 云端下载失败=停止+实例回收(CloudDownloadFailed)；无本地上传链路（勿加回）

## 云端生命周期
| 操作 | 路由 | 云端动作 |
| 停止服务 | POST /api/deployments/{id}/stop-service | 停进程+关机，实例保留 |
| 启动服务 | POST /api/deployments/{id}/start-service | 开机+起进程+重建隧道 |
| 卸载 | DELETE /api/deployments/{id} | 销毁实例 |
- stop_service：先停进程(失败记warning)→再关机(失败必须报错)→关隧道

## 压测与成本硬约束(用户强制)
- **同一时刻只允许 1 台 GPU 实例运行**；严禁并发创建；每轮结束销毁/关机再下一轮
- 登录凭证唯一 API 途径：DescribeCompShareInstance 返回 Password(Base64)
- cold start sshd 窗口可达 90s（confirm_remote_reachable 30×3s）
- 压测选型 2026-11 起：每小时 < 1 元，优先最低价有货规格

## 优云智算(UCloud RPC)
- 单端点+Action+签名+扁平表单+RetCode==0；Memory 单位 MB、显存 GB
- 状态归一化必须显式列全中间态（Initializing→Creating，落 _ 判 Failed 会反复计费）
- 带数组参数走 JSON 请求体；密码接口 Base64；SSH 走 TcpForwards(InternalPort=23)

## AutoDL Pro API(driver="autodl_pro")
- REST+Authorization token；成功 code=="Success"；payg_price 元×1000
- 创建需个人实名认证（当前无权限）；price_table 唯一价源

## SSH 可靠性
- 255+空 stderr=连接层中断自动重试 3 次；认证/解析类错误判死；轮询合并单条命令
- 隧道禁 ControlMaster；断开用 TunnelManager::heal 原地重建；建连重试 4 次
- atomic_write 临时名必须唯一(pid+seq+nanos)；停止按端口定位进程，确认后删 pid 文件

## 性能与监控
- llama-server --version 本机27秒→缓存+预热；探活 probe_http_ok(400ms)
- 云端监控不进请求路径：后台5秒刷新→共享快照→HTTP 只读
- 流式走 relay_stream 直通（先于 route() 分流）；SSE 必须 text/event-stream

## 模板与模型
- 模板唯一真源 crates/core/src/template.rs BUILTIN_TEMPLATES；内置模板改进需
  `config reset-templates --yes` + `ui restart` 手动送达；模板字段必须翻译成 CLI 参数
- 下载任务化立即返回 job_id；下载成功≠能用（warning 与 error 分开）
- 同一模型(完整ID含量化)只允许一个活跃实例；台账每次状态变更立即落盘

## 本机环境坑
- 有 HTTP_PROXY：回环验证加 --noproxy '*'；网关 11516，引擎从 11435 起，11434 是 Ollama
- ~/.cargo/bin 手动加 PATH；本机有 expect 无 sshpass；配置在 ~/Library/Application Support/mangosteen/
- llama-server Homebrew 版缺 LC_RPATH 属正常，Mangosteen 注入库路径
- CLI 无关机保留子命令：用 `cargo run -p mangosteen-provider --example instance_power -- stop <uhost-id>`

## Web UI
- 四页签 mount()/unmount()（unmount 清定时器）；批量删 JS 前枚举函数名（e2e 守护）
- 部署成功提示只留一条；失败/警告横幅必须保留；路径参数必须 percent-decode
- 设置页只接受白名单；凭证永不回显原文
