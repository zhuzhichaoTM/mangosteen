# Mangosteen 项目长期记忆

## 身份与架构
- 个人级大模型部署编排客户端，Apache-2.0，仓库 `/Users/zhuzhichao/Documents/mangosteen`
- 本地算力优先 + 云端弹性补位；不做企业集群；目标用户消费级 RTX/Apple Silicon
- 五层：交互(session/cli/webui) / 网关(server唯一收口) / 编排(local-plugin+assess+scheduler+core+cloud) / 执行(download+engine) / 基础(core/base/platform)
- `core` 不依赖自家 crate；`cfg(target_os)` 只在 `mangosteen-platform`
- 新增 crate 必改三处：根 Cargo.toml、cli/tests/layering.rs、cli/tests/release_assets.rs

## 不可违反基线
1. 本地优先：资源不足或本地失败才降级云端
2. 网关唯一收口：禁止上层直连引擎/云厂商API
3. 引擎插件化：新增框架只加 EnginePlugin（C3 守护：网关禁现 `InferenceEngine::` 字面量）
4. 多云插件抽象：新增服务商只加驱动
5. 平台差异收敛 platform
6. 配置驱动
7. 产品边界：不集群/多租户/分布式；云端优先4090/5090

## 技术约定
- Rust 2021, MSRV 1.75，依赖极简（serde/serde_json/toml/sha2/libc/windows-sys）。不用 reqwest(走curl)/axum(手写HTTP)/clap/ratatui/UUID/chrono/dirs/base64(手写)
- 生产代码禁滥用 unwrap（layering 卡 <3/kloc），统一 MnError
- 提交前必跑 `cargo test --workspace`，零警告
- 测试不写死端口：`bind(("127.0.0.1",0))`；`TestEnv::new(tag)` 用 tag 哈希派生端口段，tag 重复会污染 state 目录
- `TestEnv::write_gguf` 模型名固定 `qwen2.5-7b-instruct`

## 引擎管理
- EngineRegistry::with_builtin 是内置清单唯一来源；已实现 llama.cpp、Ollama
- Ollama 三差异：常驻服务不为单次部署拉实例；固定11434且无/health(探/api/version)；用自己blob存储不认下载目录.gguf
- EngineHandle.child 是 Option：None=外部服务，shutdown 无操作
- LocalPlan.engine 记部署时的引擎

## 模型资产
- 下载成功≠能当模型用：warning 与 error 分开；登记失败应为 done+warning，绝不判 failed
- .gguf 也装 imatrix/mmproj/LoRA/分片（无架构字段）；搜索按文件名过滤
- UI 不提供下载（只有 CLI models pull）；部署页「部署」=下载+启动
- 下载任务化：接口立即返回 job_id，绝不把 download_model 放回请求路径
- 内置模型源清单只一处：ModelSourceRegistry::builtin()；ModelScope 无公开检索，按关键字搜只用 hf-mirror
- 量化可选非主链路：输出路径去旧量化后缀；输出与源相同必须拒绝；后台线程执行

## 协议适配(llama.cpp)
- 响应 model 是绝对路径→改写模型ID（非流式 rewrite_response_model；流式 rewrite_sse_line 逐行）
- SSE Content-Type 必须 text/event-stream
- 流式必须走直通 relay_stream：handle_connection 里先于 route() 分流；用 curl --no-buffer 逐行读、chunked+每块flush+TCP_NODELAY；出错不能 ? 抛给 handle_connection
- ActiveDeployment.model 存完整ID

## 调度与部署
- 六步进度：Assessing→Selecting→Configuring→Downloading→Quantizing→Launching；终态 Ready/Failed
- 部署分两段：prepare_with_progress→commit_local，下载夹中间
- PreparedLocal::with_model_path：远端评估 model_path 空，commit 前用下载落地路径补上并同步台账
- GGUF 头部预取：元数据段可能>4MB要自适应翻倍；--dump-header - 与 --output - 不能同时指 stdout
- Gateway::deploy() 转调 deploy_with_progress(req,&noop_sink)；失败上报集中在 wrapper（避免进度条卡死）
- PullJobs::set_stage 只前进不回退；sink 在持网关锁期间只能写内存
- 「模型量化」恒 Skipped
- 中止回滚：MnError::Cancelled 与失败分开→kill curl→abort：停引擎/释放端口/删台账
- delete_file 传路径不传 bool
- 失败路径必须清 provisioning 记录
- ComputeTarget：Auto/Local/Cloud。Local 绝不静默降级上云；Cloud 则 preview.local_viable 恒 false
- DeploymentPreview.blocked 必须单独判断；未知 target 一律400
- 「最便宜」走 cheapest_first 不是 scheduler.strategy；共用 policy::cheapest_cmp
- 「可用」由创建结果定义不由 stock 字段；优云库存是模拟调度
- 裸实例(cloud cheapest --create)不写台账；必须用 cloud destroy --instance 清理
- --create 类开关必须登记进 cli.rs 的 BOOLEAN_FLAGS
- e2e 断言 preview.local_viable==true 用 TestEnv::warmed_gateway()
- 同一模型(完整ID含量化)只允许一个活跃实例
- 台账每次状态变更立即落盘

## 推理速度
- 默认上下文 8192；mlock 默认关；线程数取物理核；本机 i7-4770HQ 无GPU 纯CPU 0.6~2 tok/s

## 性能陷阱
- llama-server --version 本机27秒：读缓存+启动预热
- HTTP 服务固定线程池
- 探活用 process::probe_http_ok(400ms)
- HTTP 判断引擎状态：缓存命中直接给；未命中现场探测限2秒
- 云端监控不进请求路径：后台5秒刷新→共享快照→HTTP只读
- 云端 curl 传输层：URL 只走 argv，不能同时写进 --config

## Web UI
- 四页签：对话/部署/设备/设置；内嵌单文件HTML无构建链
- 部署页卡片：部署模型→部署进度→模型列表→模型实例
- 进度默认 .dp-compact 精简条，六步详情收「详情」后
- 停止按钮靠 job.can_abort；走 POST /api/models/jobs/{id}/cancel
- 两张卡片按钮态同源→刷新走唯一入口 refreshModelCards()；3秒轮询取指纹
- 部署成功提示只留一条；失败/警告横幅必须保留
- 页签 mount()/unmount()；unmount 必须清定时器
- 跨页签复用JS函数必须全局；批量删JS前先枚举函数名确认区间外调用点（e2e_ui_script_has_no_undefined_function_calls）
- 对话生成可中断：streaming 时按钮变停止，AbortController；区分 AbortError
- 对话页模型信息只列运行中实例：/api/status.active；发送传具体模型ID
- 模型管理走 GET /api/models
- 路径参数必须 percent-decode（parse_request::percent_decode_path）
- file_deleted 必须是核实结果不能回显请求参数
- 设置页只接受白名单；凭证永不回显原文

## 云端部署(多厂商通用)
- CloudPlan 两条路径二选一：download_url(llama.cpp主链路) 或 remote_model(Ollama按名拉)；都None直接报错
- 云端 gpu_layers 必须 = spec.n_layers（本机无GPU时是0，搬云端变CPU推理）
- 创建失败按价格顺序换下一个候选：无库存/售罄重试；实例建出但起不来(failed/destroyed/start_timeout)清理后也换下一个；鉴权/参数/余额立刻中止
- 销毁要重试 destroy_with_retry：厂商状态变更时返回8903 UHost is in the task
- SSH 密码认证：SSH_ASKPASS + SSH_ASKPASS_REQUIRE=force，临时脚本0700；本机无sshpass
- askpass 脚本不能 spawn 后立刻删：要等隧道建成/明确失败后再删
- 密码优先级：Instance.network.ssh_password > [cloud].ssh_password；厂商已给密码不要 reset_root_password
- 隧道进程必须捕获 stderr
- scheduler.require_stock：优云库存是模拟调度必须关；AutoDL较准
- 「最便宜」未知价格 f64::INFINITY 排最后不能当0元；价格哨兵 hourly_price 拒 ≥MAX/2 及 >1000元/h

## 云端落位无关原则(勿改回去)
- 部署位置决定下载与执行位置：云端→云端下载+云端跑；本地→本地下载+本地跑
- 云端 download_url 主链路；云端下载失败=部署停止+实例回收(MnError::CloudDownloadFailed)，无本地下载后上传兜底
- 不存在本地上传链路：CloudPlan.upload_from/remote::upload_file 已删除
- 权重获取只两步 bootstrap_llama：远端已有(跳过)→远端下载；无下载地址明确拒绝

## 云端生命周期(启动/停止/卸载)
| 操作 | 路由 | 云端动作 | 实例 | 计费 |
| 停止服务 | POST /api/deployments/{id}/stop-service | 停llama-server+关机 | 保留 | 停机不计 |
| 启动服务 | POST /api/deployments/{id}/start-service | 开机+起进程+重建隧道 | 保留 | 恢复 |
| 卸载 | DELETE /api/deployments/{id} | 销毁实例 | 删除 | 完全停止 |
- stop_service：先停进程(失败记warning)→再关机(失败必须报错)→关隧道
- start_service 不需重传权重
- 台账：stopped 保留实例；destroyed 已回收

## 优云智算(mangosteen-backend, UCloud RPC)
- 单端点+Action参数+PublicKey/Signature+扁平表单+RetCode==0；公钥 options.public_key，私钥 secret
- 单位：创建/价格/升降配 Memory 是 MB；机型表/库存/显存是 GB
- 销毁前必须先关机(Terminate 仅对 Stopped 有效)
- 状态归一化必须显式列出所有中间态：Initializing→Creating；落到_会被判Failed→反复计费
- 云端不用Ollama社区镜像：用纯CUDA环境(cuda128_python312)；云端镜像与本地隔离
- 创建前库存预检可按 require_stock 关；库存是模拟调度
- 带数组参数必须走JSON请求体(Monitor/RefundPrice/Bandwidth/Ports)；表单UHostIds.0全失败
- DescribeCompShareSoftwarePort 不含SSH：SSH走容器实例TcpForwards(InternalPort=23)
- 定时关机需 ProjectId；GetCompShareInstanceUpgradePrice 的 Price 是元(差价)
- 带宽切换顶层RetCode=0不代表每实例成功：看Results[].Success
- 密码接口要Base64；签名有官方测试向量

## AutoDL 容器实例 Pro API(driver="autodl_pro")
- REST+Authorization:<token>；成功 code=="Success"不是HTTP200
- 读接口正常但创建需个人实名认证；无当前资源访问权限
- snapshot 直接返回 proxy_host/ssh_port/root_password
- payg_price 单位 元×1000；无报价接口，price_table 唯一价源
- gpu_spec_uuid 无完整映射：映射到归一化名小写空格换-；映射不到提示配 gpu_spec_map

## 本机环境坑
- 有 HTTP_PROXY，验证回环要 --noproxy '*' 或 NO_PROXY=127.0.0.1,localhost
- llama-server 是 Homebrew 版缺 LC_RPATH；Library not loaded 是正常的，Mangosteen 注入库路径
- 11434 是 Ollama；网关默认11516，引擎从11435起；清理进程别误杀
- ~/.cargo/bin 需手动加PATH；本机有 expect、无 sshpass
- 用户配置在 ~/Library/Application Support/mangosteen/；密钥放 secrets/(0700)
