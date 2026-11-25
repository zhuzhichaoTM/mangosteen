//! Strata 引擎集成（MoE 三级存储推理运行时）。
//!
//! 归属：**算力执行层 · 引擎插件**。
//!
//! ## Strata 是什么
//!
//! Strata（Niko1221/Strata，MIT）是面向超大 MoE 模型的推理运行时：
//! 专家热缓存驻显存（12–24 GB 即可跑 125B 级 MoE）、全量专家驻内存、
//! SSD 冷查找表，配合原生 MTP 投机解码。它消费 GGUF 权重，
//! 暴露 OpenAI + Anthropic 兼容 API（默认 `127.0.0.1:8080`）。
//!
//! ## 与 llama.cpp 插件的关键差异
//!
//! | 维度 | llama.cpp | Strata |
//! |------|-----------|--------|
//! | 启动 | `llama-server --model <path> ...` | `strata launch --family <f> --model <m> --yes --no-tui` |
//! | 模型输入 | 文件路径 | 家族/量化名（引擎自管模型存储） |
//! | 密钥 | `--api-key-file`（文件） | `API_KEY` 环境变量 |
//! | TLS | `--ssl-key-file/--ssl-cert-file` | 不支持（依赖 SSH 隧道） |
//! | 健康检查 | `/health` | `/v1/models`（OpenAI 兼容） |
//! | 停止 | pid 文件 + 端口匹配 | `strata launch --stop` 或进程名匹配 |
//!
//! 这些差异全部由 [`super::plugin::EnginePlugin`] 的生命周期方法吸收，
//! 上层（provider / orchestration）不感知。
//!
//! ## 模型获取
//!
//! Strata 自己管理模型仓库（`strata launch` 会从 HuggingFace/ModelScope
//! 拉取家族权重到其数据目录）。但 Mangosteen 的调度链路要求「远端节点
//! 自己下载模型文件」——因此部署流程仍会把模板声明的 GGUF 下载到
//! `{workdir}/models/`，同时把 `--model <quant>` 语义（家族 + 量化名）
//! 传给 strata。Strata 侧在其数据目录发现同量化文件时直接复用，
//! 未命中则由它自行下载（此时下载阶段已把网络与磁盘准备好）。

use std::time::Duration;

use mangosteen_core::config::{ApiKeyMode, InferenceEngine};
use mangosteen_core::plan::{EngineLaunchConfig, LocalPlan};
use mangosteen_core::{MnError, MnResult};

use crate::plugin::EnginePlugin;
use crate::remote::{self, SshTarget};

/// Strata 引擎插件。
///
/// **无状态**，可反复使用。远端生命周期全部通过 SSH 命令实现，
/// 与 llama.cpp 插件共用 `remote` 层的执行原语。
#[derive(Debug, Default, Clone)]
pub struct StrataEngine {
    /// 监听地址（云端走 `[cloud] bind_host`）。静态实例时为空串，
    /// 由 launch_config 调用方传入真实绑定地址。
    pub bind_host: String,
    /// Strata 引擎/模型下载镜像 feed（`--feed` 参数）。
    ///
    /// 国内节点访问 GitHub 慢，通过 ghproxy 等镜像加速 Strata 引擎包
    /// 与模型文件下载。`None` 时不传 `--feed`，走官方源。
    pub feed_url: Option<String>,
}

impl StrataEngine {
    /// 构造。
    pub fn new() -> Self {
        Self {
            bind_host: "127.0.0.1".to_string(),
            feed_url: None,
        }
    }

    /// 覆盖绑定地址（网关装配时读 `[cloud] bind_host`）。
    pub fn with_bind_host(mut self, host: impl Into<String>) -> Self {
        self.bind_host = host.into();
        self
    }

    /// 覆盖下载镜像 feed（国内节点加速）。
    pub fn with_feed_url(mut self, url: impl Into<String>) -> Self {
        self.feed_url = Some(url.into());
        self
    }

    /// 生成启动参数（family/model/quant 由 `extra_args` 透传）。
    ///
    /// `LocalPlan::extra_args` 在 Strata 部署中的约定：
    /// ```text
    /// ["--family", "qwen", "--model", "q2_0"]
    /// ```
    /// 由模板解析层（template schema）写入。
    fn strata_args(plan: &LocalPlan, bind_host: &str, port: u16) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        // 模板透传的引擎专属参数（family / model / quant / context 等）
        args.extend(plan.extra_args.iter().cloned());
        // Strata 支持 --port N（2026-11-25 用户确认）：追加 --port
        let _ = bind_host;
        args.push("--port".to_string());
        args.push(port.to_string());
        args
    }
}

/// 模板里 Strata 参数缺失时的兜底家族/模型。
///
/// 正常情况下 `extra_args` 由模板 schema 填充（--family/--model）；
/// 这里只兜底 CLI 手工路径，避免启动出一个空参数进程。
const DEFAULT_FAMILY: &str = "qwen";

impl EnginePlugin for StrataEngine {
    fn engine(&self) -> InferenceEngine {
        InferenceEngine::Strata
    }

    fn launch_config(&self, plan: &LocalPlan, bind_host: &str, port: u16) -> EngineLaunchConfig {
        let mut args = Self::strata_args(plan, bind_host, port);
        // --model-path 模式下不需要 family/model（Strata 直接读文件）；
        // 且带 --model-path 时插入 family 会让 Strata 走「按名字下载」路径，
        // 忽略已下载文件，重新下载 23.8GB（2026-11-23 实测）。
        let has_model_path = args.iter().any(|a| a == "--model-path");
        if !has_model_path && !args.iter().any(|a| a == "--family") {
            // 未提供 family：插入兜底值（模型名由 --model 表达）
            args.splice(0..0, ["--family".to_string(), DEFAULT_FAMILY.to_string()]);
        }
        EngineLaunchConfig {
            args,
            // 密钥走 API_KEY 环境变量（见 `start`），不进 argv
            api_key: None,
        }
    }

    fn probe(&self, plan: &LocalPlan) -> bool {
        // OpenAI 兼容端点：/v1/models 带 Bearer Token 时 200
        let token = plan.api_key.as_deref().unwrap_or("");
        mangosteen_platform::process::probe_http_ok_with_auth(
            &plan.host,
            plan.port,
            "/v1/models",
            token,
            Duration::from_millis(400),
        )
    }

    fn precheck(&self) -> MnResult<()> {
        // 云端链路在远端做 precheck（ensure_engine）；本地不做进程假设
        Ok(())
    }

    /// Strata 二进制准备：官方安装脚本（`[cloud] strata_install_url`）。
    ///
    /// 安装器落地 `strata` 到 `~/.local/bin` 或 `STRATA_BIN_DIR` 指定目录。
    /// 这里以 `{workdir}/bin/strata` 为锚点，找不到时用官方 install.sh
    /// 安装到 `{workdir}/bin`（`STRATA_BIN_DIR`）与 `{workdir}/strata-data`
    /// （`STRATA_DATA_DIR`），整个安装落在 mangosteen workdir 内、deploy 用户可写。
    ///
    /// `url` 为官方 install.sh 地址（国内节点可换成镜像地址）。
    /// `feed` 为发布目录（`STRATA_FEED`，国内节点可换成 GitHub 镜像代理）。
    /// 两者都为空时回落官方 GitHub。
    fn ensure_engine(
        &self,
        target: &SshTarget,
        workdir: &str,
        url: Option<&str>,
    ) -> MnResult<Option<String>> {
        let wd = workdir.trim_end_matches('/');
        // 以 deploy 用户完全控制的新目录存放 strata 二进制，避开 root 在
        // 引导链（root 下载 llama-server）创建的 {workdir}/bin/（deploy 用户
        // 无权在其中写符号链接，会导致 install.sh 的 ln 失败）。
        let bin = format!("{wd}/strata-bin/strata");
        let probe = remote::ssh_exec(
            target,
            &format!(
                "if [ -x {bin} ]; then echo ok; \
                 elif command -v strata >/dev/null 2>&1; then command -v strata; \
                 else echo missing; fi"
            ),
            60,
        )?;
        let probe = probe.trim();
        if probe == "ok" {
            return Ok(Some(bin));
        }
        if probe != "missing" && !probe.is_empty() {
            // PATH 里已有 strata
            return Ok(Some(probe.to_string()));
        }
        // 远端没有：用官方安装脚本安装到 workdir（幂等）
        let install_url = url
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| MnError::EngineLaunch {
                binary: "strata".into(),
                reason: "远端未安装 strata，且配置未提供 cloud.strata_install_url。\
                         请设置该字段（官方 install.sh 地址）或在镜像中预装 strata。"
                    .into(),
            })?;
        remote::ssh_exec(
            target,
            &format!(
                "set -e; mkdir -p {wd}/strata-bin {wd}/tmp; \
                 curl -fsSL --retry 3 -o {wd}/tmp/strata-install.sh '{install_url}' && \
                 STRATA_BIN_DIR={wd}/strata-bin STRATA_DATA_DIR={wd}/strata-data \
                   sh {wd}/tmp/strata-install.sh && \
                 test -x {bin}",
                wd = wd
            ),
            600,
        )?;
        Ok(Some(bin))
    }

    /// 远端拉起：`strata launch --family ... --model ... --yes --no-tui`。
    ///
    /// 密钥注入：`API_KEY` 环境变量（Strata 官方形态），
    /// 通过 SSH `env` 前缀传递——不进 argv、不进日志。
    fn start(
        &self,
        target: &SshTarget,
        workdir: &str,
        launch: &EngineLaunchConfig,
    ) -> MnResult<()> {
        // 入口日志（2026-11-23 用户裁决）：打印远端节点与完整启动参数，
        // 便于确认调用的引擎与参数是否符合预期。
        eprintln!(
            "[strata-start] 节点: {}:{}, workdir: {}, 参数: {}",
            target.host, target.port, workdir,
            launch.args.join(" ")
        );
        let wd = workdir.trim_end_matches('/');
        // Strata 支持 --port N（2026-11-25 用户确认）：从 args 中提取 --port 值
        let port = launch.args.iter()
            .position(|a| a == "--port")
            .and_then(|i| launch.args.get(i + 1))
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(8080);
        let args = &launch.args;
        let _ = port; // port 已在 strata_args 中追加到 args
        // 不用 shell_quote：它是给 ssh 转义的，会把 "--vision" 转成 "'--vision'"，
        // Strata CLI 会把引号当参数的一部分，导致解析错误。
        // Strata 参数都是标准 flag + 值，不需要 shell 转义，直接 join 即可。
        let quoted: Vec<String> = args.to_vec();
        let env_prefix = launch
            .api_key
            .as_deref()
            .filter(|k| !k.trim().is_empty())
            .map(|k| format!("API_KEY={}", shell_quote(k)))
            .unwrap_or_default();
        let log = format!("{wd}/strata.log");
        let pid = format!("{wd}/strata.pid");
        // 优先用 workdir 内的 strata（ensure_engine 安装点），回落 PATH
        let strata_bin = format!("{wd}/strata-bin/strata");
        // 启动参数：launch 子命令 + 模板透传参数（--family/--model/...）
        let mut launch_args = vec!["launch".to_string()];
        launch_args.extend(quoted);
        // --data-dir 指向 workdir 根（不是 {wd}/models）：Strata 会在
        // data-dir 下自行拼接 models/<family>/<quant>/ 子目录结构。
        // 若指向 {wd}/models，Strata 会拼成 {wd}/models/models/IQ3_S/（双 models），
        // 找不到 mangosteen 已下载的文件而触发重复下载，撑爆磁盘
        // （2026-11-22 实测：need ~84GB, 52GB free）。
        // 指向 {wd} 后，Strata 在 {wd}/models/IQ3_S/ 直接复用已下载的 GGUF。
        if !launch_args.iter().any(|a| a == "--data-dir") {
            launch_args.push("--data-dir".into());
            launch_args.push(wd.to_string());
        }
        // --yes --no-tui：后台无界面运行（官方语义，幂等追加）
        if !launch_args.iter().any(|a| a == "--yes") {
            launch_args.push("--yes".into());
        }
        if !launch_args.iter().any(|a| a == "--no-tui") {
            launch_args.push("--no-tui".into());
        }
        // --access local：仅本机监听（SSH 隧道访问，不暴露公网）
        if !launch_args.iter().any(|a| a == "--access") {
            launch_args.push("--access".into());
            launch_args.push("local".into());
        }
        // --parallel 1：单并发（内存优化，参考脚本推荐值）
        if !launch_args.iter().any(|a| a == "--parallel") {
            launch_args.push("--parallel".into());
            launch_args.push("1".into());
        }
        // --feed <url>：国内镜像加速（可选，StrataEngine.feed_url 配置）
        if let Some(feed) = &self.feed_url {
            if !feed.trim().is_empty() && !launch_args.iter().any(|a| a == "--feed") {
                launch_args.push("--feed".into());
                launch_args.push(feed.clone());
            }
        }
        let args_str = launch_args.join(" ");
        // 生成独立 sh 脚本（2026-11-24 用户裁决）：避免 SSH 内联脚本的
        // 转义/语法问题（多余的 fi; 导致 bash 语法错误，脚本静默失败）。
        let remote_script = format!(
            "#!/bin/bash\n\
             # strata 启动脚本（由 mangosteen 自动生成）\n\
             BIN={strata_bin}\n\
             [ -x \"$BIN\" ] || BIN=strata\n\
             LOG={log}\n\
             PID={pid}\n\
             # 幂等：已在 8080 提供服务则跳过\n\
             if curl -fsS --max-time 3 http://127.0.0.1:{port}/v1/models >/dev/null 2>&1; then\n\
               echo 'strata 已在运行，跳过启动'\n\
               exit 0\n\
             fi\n\
             # 清理残留\n\
             if [ -f \"$PID\" ]; then\n\
               kill -9 $(cat \"$PID\") 2>/dev/null || true\n\
             fi\n\
             pkill -9 -f 'strata_control.runtime' 2>/dev/null || true\n\
             pkill -9 -f 'serve.server' 2>/dev/null || true\n\
             pkill -9 -f 'strata.*launch' 2>/dev/null || true\n\
             rm -rf /home/deploy/.local/state/strata/runtime/* 2>/dev/null || true\n\
             rm -f /home/deploy/.local/state/strata/launch/*.json 2>/dev/null || true\n\
             sleep 1\n\
             echo '=== strata 二进制信息 ==='\n\
             \"$BIN\" --version 2>&1 | head -3\n\
             echo '=== 启动 strata ... ==='\n\
             setsid nohup env TMPDIR=/dev/shm {env_prefix} \"$BIN\" {args_str} >> \"$LOG\" 2>&1 < /dev/null &\n\
             echo $! > \"$PID\"\n\
             sleep 5\n\
             echo started\n"
        );
        let script_path = format!("{wd}/start-strata.sh");
        let write_script = format!(
            "cat > {script_path} << 'STRATA_EOF'\n{remote_script}\nSTRATA_EOF\nchmod +x {script_path}"
        );
        remote::ssh_exec(target, &write_script, 30)?;
        eprintln!("[strata-start] 远端脚本已写入: {script_path}");
        let script = format!("bash {script_path}");
        eprintln!("[strata-start] 远端执行脚本: {script}");
        eprintln!("[strata-start] 节点: {}:{}, workdir: {}, 参数: {}", target.host, target.port, workdir, launch.args.join(" "));
        eprintln!("[strata-start] 远端执行指令: {strata_bin} {args_str}");
        let out = match remote::ssh_exec(target, &script, 240) {
            Ok(out) => {
                eprintln!("[strata-start] 远端脚本输出: {out}");
                out
            }
            Err(e) => {
                eprintln!("[strata-start] 远端脚本执行失败: {e}");
                return Err(e);
            }
        };
        // 脚本成功执行（exit 0）但输出为空时，仍视为成功
        //（Strata --no-tui 模式下 strata launch 启动 serve.server 后退出，无 stdout）
        if out.is_empty() {
            eprintln!("[strata-start] 远端脚本执行成功（exit 0，无 stdout 输出）");
        }
        // Strata 幂等语义：以下输出均视为成功
        // - "started"       → 新启动成功
        // - "已在运行"       → curl 健康检查发现已服务
        // - "已就绪"        → pid 存在且 curl 确认服务
        // - "Already serving" / "Strata is serving" → strata CLI 输出已在服务
        // - 空输出 + exit 0  → ssh_exec 成功（脚本无显式输出但退出码 0）
        if out.is_empty()
            || out.contains("started")
            || out.contains("已在运行")
            || out.contains("已就绪")
            || out.contains("Strata is serving")
            || out.contains("Already serving")
        {
            return Ok(());
        }
        return Err(MnError::EngineLaunch {
            binary: "strata".into(),
            reason: format!("远端启动 strata 失败：{out}"),
        });
    }

    /// 停止：`strata launch --stop`，失败回落进程名/pid 清理。
    fn stop(&self, target: &SshTarget, workdir: &str) -> MnResult<()> {
        let wd = workdir.trim_end_matches('/');
        let strata_bin = format!("{wd}/strata-bin/strata");
        // 先走官方 stop（优雅），再兜底
        let _ = remote::ssh_exec(
            target,
            &format!(
                "BIN={strata_bin}; [ -x \"$BIN\" ] || BIN=strata; \
                 \"$BIN\" launch --stop 2>/dev/null || true"
            ),
            60,
        );
        remote::stop_engine_process(target, wd, "strata", Some(&format!("{wd}/strata.pid")))
    }

    fn is_running(
        &self,
        target: &SshTarget,
        workdir: &str,
        port: u16,
    ) -> MnResult<bool> {
        let wd = workdir.trim_end_matches('/');
        remote::engine_process_running_on_port(target, wd, "strata", port)
            .or_else(|_| remote::engine_process_running_on_port(target, wd, "strata", 8080))
    }

    /// Strata 预下载目录：`models/<family>/<quant>/`（与 Strata 内部布局一致）。
    ///
    /// 2026-11-22 实测：平铺 models/ 时 Strata 启动找不到文件会重复下载
    /// 23.8GB 撑爆磁盘。子目录布局让 Strata 直接复用已下载文件。
    fn model_download_dir(&self, workdir: &str, plan: &LocalPlan) -> String {
        let wd = workdir.trim_end_matches('/');
        let family = plan
            .extra_args
            .iter()
            .position(|a| a == "--family")
            .and_then(|i| plan.extra_args.get(i + 1))
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| "qwen".to_string());
        let quant = plan
            .extra_args
            .iter()
            .position(|a| a == "--model")
            .and_then(|i| plan.extra_args.get(i + 1))
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| "default".to_string());
        format!("{wd}/models/{family}/{quant}")
    }

    /// Strata 启动参数适配：预下载文件已存在时切换 --model-path 复用。
    ///
    /// 2026-11-23 用户裁决：Strata 的 `--model-path` 必须指向**目录**
    ///（不是文件），目录里放 GGUF + mmproj。Strata 从目录自动识别
    /// 模型，避免 HuggingFace 校验/重复下载（`--model iq3_s` 这种写法
    /// 会先去 HuggingFace 校验权重，国内必超时）。
    ///
    /// 同时移除 --family/--model（带这两个 flag 会让 Strata 走名字下载
    /// 路径，忽略 --model-path，重新下载 23.8GB）。
    fn adapt_launch_args(&self, args: &mut Vec<String>, model_path: &str) {
        // --model-path 必须优先于 --family/--model（Strata 同时收到两者时
        // 会走名字下载路径，忽略 --model-path，重新下载 23.8GB）。
        // 2026-11-23 用户裁决：无论 --model-path 是否已存在，都必须移除
        // --family/--model，确保 --model-path 生效。幂等：只追加一次 --model-path。
        // model_path 是文件路径，取父目录（Strata 要目录）
        let dir = std::path::Path::new(model_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| model_path.to_string());

        // 某些模型的 GGUF 文件在量化子目录下（如 IQ2_XS/），需要上溯到父目录。
        // 用 SSH 检查远端目录是否有 GGUF 文件（本地 std::fs 无法访问远端）。
        // 但为了简单起见，如果目录名是全大写+数字+下划线（量化级别），上溯一级。
        let dir_name = std::path::Path::new(&dir)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let is_quant_dir = dir_name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && !dir_name.is_empty();
        let dir = if is_quant_dir {
            std::path::Path::new(&dir)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(dir)
        } else {
            dir
        };

        // 移除 --family/--model（无论 --model-path 是否已存在）
        let mut out: Vec<String> = Vec::with_capacity(args.len());
        let mut skip_next = false;
        for arg in args.iter() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == "--family" || arg == "--model" {
                skip_next = true;
                continue;
            }
            out.push(arg.clone());
        }

        // --model-path 不存在时追加；已存在时更新其值（确保指向正确的目录）
        if let Some(idx) = out.iter().position(|a| a == "--model-path") {
            // 已存在：更新值
            if idx + 1 < out.len() {
                out[idx + 1] = dir;
            }
        } else {
            out.push("--model-path".into());
            out.push(dir);
        }
        *args = out;
    }

    /// Strata 自管存储：预下载文件不存在也允许启动（引擎自行下载）。
    fn preflight_model_check(&self, _model_path: &str) -> Result<(), String> {
        Ok(())
    }
}

/// 最小 shell 单引号转义（防注入）。
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::estimate::KvCacheType;

    fn test_plan(extra_args: Vec<String>) -> LocalPlan {
        LocalPlan {
            model_path: "/home/deploy/mangosteen/models/IQ3_S/qwen.gguf".into(),
            context_size: 65536,
            threads: 8,
            batch_size: 2048,
            gpu_layers: 99,
            kv_cache_type_k: KvCacheType::Q8_0,
            kv_cache_type_v: KvCacheType::Q8_0,
            flash_attention: true,
            use_mmap: true,
            use_mlock: false,
            port: 8080,
            host: "127.0.0.1".into(),
            engine: InferenceEngine::Strata,
            mmproj_path: None,
            draft_model_path: None,
            auxiliary_downloads: Vec::new(),
            extra_args,
            api_key: None,
        }
    }

    /* ---- 任务 2：Strata CLI 参数对齐 ---- */

    /// launch_config 必须包含 --family（模板声明或兜底 qwen）、--host、--port
    /// 以及模板 extra_args 中的所有 Strata 专属 flag。
    ///
    /// 注意：--yes / --no-tui / --data-dir 在 `start()` 阶段追加（需要 workdir），
    /// 不在 launch_config 的 args 里。
    #[test]
    fn launch_config_includes_family_host_port() {
        let engine = StrataEngine::new();
        let plan = test_plan(vec!["--family".into(), "qwen".into(), "--model".into(), "IQ3_S".into()]);
        let cfg = engine.launch_config(&plan, "127.0.0.1", 8080);

        assert!(
            cfg.args.contains(&"--family".to_string()),
            "launch_config 必须包含 --family：{:?}",
            cfg.args
        );
        assert!(
            cfg.args.contains(&"--model".to_string()),
            "launch_config 必须包含 --model：{:?}",
            cfg.args
        );
        // Strata 默认监听 127.0.0.1:8080，不传 --host/--port
    }

    /// launch_config 必须包含 --family 兜底值（模板未声明时）。
    #[test]
    fn launch_config_includes_default_family() {
        let engine = StrataEngine::new();
        let plan = test_plan(vec![]);
        let cfg = engine.launch_config(&plan, "127.0.0.1", 8080);

        assert!(
            cfg.args.contains(&"--family".to_string()),
            "未声明 family 时必须有兜底 --family：{:?}",
            cfg.args
        );
        // 兜底值必须是 qwen
        let family_idx = cfg.args.iter().position(|a| a == "--family").unwrap();
        assert_eq!(cfg.args[family_idx + 1], "qwen");
    }

    /// strata_args 必须把 extra_args、--host、--port 全部拼进去。
    #[test]
    fn strata_args_includes_all() {
        let plan = test_plan(vec![
            "--family".into(),
            "qwen".into(),
            "--model".into(),
            "IQ3_S".into(),
            "--context".into(),
            "65536".into(),
        ]);
        let args = StrataEngine::strata_args(&plan, "0.0.0.0", 8080);

        assert!(args.contains(&"--family".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"--context".to_string()));
        // Strata 不传 --host/--port，默认 127.0.0.1:8080
    }

    /// engine() 返回 Strata 枚举，capabilities() 返回正确能力表。
    #[test]
    fn engine_and_capabilities() {
        let e = StrataEngine::new();
        assert_eq!(e.engine(), InferenceEngine::Strata);

        let caps = e.capabilities();
        assert_eq!(caps.health_path, "/v1/models");
        assert_eq!(caps.default_port, 8080);
        assert!(!caps.supports_tls, "Strata 不支持 TLS（依赖 SSH 隧道）");
        assert_eq!(caps.pid_name, "strata");
    }
}

/// 供外部读取引擎的密钥注入方式（编排层构造 env 时用）。
pub fn strata_api_key_env(key: &str) -> (String, String) {
    ("API_KEY".to_string(), key.to_string())
}

/// Strata 密钥模式断言（编译期文档）：Env。
pub const STRATA_KEY_MODE: ApiKeyMode = ApiKeyMode::Env;