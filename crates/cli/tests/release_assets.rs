//! 发布资产完整性测试。
//!
//! 发布包里必须包含的东西，以及它们必须真的能用。
//!
//! 这些不是"文档检查"，而是**交付件检查**：
//! 一个语法错误的示例配置会让用户第一次上手就失败，
//! 而这类错误在代码测试里根本发现不了。

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读不到 {rel}（{}）：{e}", p.display()))
}

// ---------------------------------------------------------------- 必备文件

#[test]
fn release_assets_exist() {
    for f in [
        "README.md",
        "LICENSE",
        ".gitignore",
        "docs/DESIGN.md",
        "docs/VERSUS-OLLAMA.md",
        "config/config.example.toml",
        "config/providers/builtin.toml",
        "scripts/build-release.sh",
        ".github/workflows/release.yml",
    ] {
        let p = repo_root().join(f);
        assert!(p.is_file(), "缺少发布资产：{f}（{}）", p.display());
    }
}

#[test]
fn release_script_is_executable() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = repo_root().join("scripts/build-release.sh");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "build-release.sh 必须有执行权限（chmod +x）"
        );
    }
}

// ---------------------------------------------------------------- 示例配置

#[test]
fn example_config_is_valid_toml_and_loads() {
    let text = read("config/config.example.toml");

    // 必须能被解析成 Config
    let cfg = mangosteen_core::config::Config::from_toml(&text)
        .expect("config.example.toml 必须能被解析为合法配置");

    // 关键默认值必须符合产品基线
    assert!(
        cfg.scheduler.local_first,
        "示例配置必须保持 local_first = true（产品基线）"
    );
    assert!(cfg.gateway.port >= 1024, "网关端口不得使用特权端口");
    assert!(
        !cfg.providers.is_empty(),
        "示例配置应至少包含一个服务商，让用户知道怎么写"
    );

    // 三个内置服务商都应出现（含被停用的，作为示范）
    for name in ["autodl", "ucloud", "aliyun"] {
        assert!(
            cfg.providers.contains_key(name),
            "示例配置应包含服务商 `{name}` 作为示范"
        );
    }

    // 凭证默认用 env 形式（不要把明文密钥写进示例）
    for (name, p) in &cfg.providers {
        assert!(
            !matches!(p.secret, mangosteen_core::config::SecretRef::Inline(_)),
            "示例配置里 `{name}` 不应使用内联明文密钥"
        );
    }
}

#[test]
fn example_config_paths_are_placeholders_not_repo_paths() {
    let text = read("config/config.example.toml");
    // 不能把开发者本机路径写进示例
    for bad in ["/Users/", "C:\\Users\\zhuzhichao", "/home/zhuzhichao"] {
        assert!(
            !text.contains(bad),
            "示例配置包含个人路径 `{bad}`，应改用通用占位路径"
        );
    }
}

#[test]
fn provider_reference_file_is_marked_as_reference() {
    let text = read("config/providers/builtin.toml");
    // 必须明确告知用户这不是可直接使用的配置
    assert!(
        text.contains("参考") && text.contains("不能直接作为 config.toml 使用"),
        "provider 参考件必须明确标注不可直接使用"
    );
    // 但仍然应当是合法 TOML（否则用户复制时会困惑）
    let parsed: Result<toml::Value, _> = toml::from_str(&text);
    assert!(parsed.is_ok(), "provider 参考件必须是合法 TOML");
}

// ---------------------------------------------------------------- 文档一致性

#[test]
fn readme_documents_all_commands() {
    let readme = read("README.md");
    for cmd in [
        "deploy", "run", "stop", "status", "ps", "models", "assess", "cloud", "config", "serve",
        "tui", "doctor",
    ] {
        assert!(
            readme.contains(cmd),
            "README 未提及命令 `{cmd}`，用户会不知道它存在"
        );
    }
}

#[test]
fn readme_states_engine_requirement() {
    let readme = read("README.md");
    assert!(
        readme.contains("llama.cpp") && readme.contains("llama-server"),
        "README 必须说明依赖 llama.cpp 的 llama-server（否则用户跑不起来）"
    );
}

#[test]
fn design_doc_covers_all_layers() {
    let design = read("docs/DESIGN.md");
    for layer in [
        "mangosteen-core",
        "mangosteen-platform",
        "mangosteen-base",
        "mangosteen-session",
        "mangosteen-local",
        "mangosteen-backend",
        "mangosteen-cloud",
        "mangosteen-gateway",
        "mangosteen-cli",
    ] {
        assert!(design.contains(layer), "设计文档未覆盖 `{layer}`");
    }
    // 关键机制必须有文档
    for topic in [
        "资源评估引擎",
        "调度决策器",
        "多云统一抽象",
        "后端节点选择器",
        "统一协议适配层",
        "文件访问控制",
        "架构约束的自动化守护",
    ] {
        assert!(design.contains(topic), "设计文档缺少 `{topic}` 章节");
    }
}

#[test]
fn versus_doc_compares_key_dimensions() {
    let doc = read("docs/VERSUS-OLLAMA.md");
    for dim in [
        "资源需求评估",
        "云端算力调度",
        "多云厂商统一抽象",
        "节点选择",
    ] {
        assert!(doc.contains(dim), "差异化分析缺少维度 `{dim}`");
    }
    // 诚实地列出"不做的事"比罗列优点更重要
    assert!(
        doc.contains("不做"),
        "差异化分析应明确说明 Mangosteen 不做什么"
    );
}

// ---------------------------------------------------------------- 版本一致性

#[test]
fn version_is_consistent_across_manifests() {
    let root = read("Cargo.toml");
    // workspace.package.version
    let ver_line = root
        .lines()
        .find(|l| l.trim_start().starts_with("version = "))
        .expect("workspace 应有 version");
    let version = ver_line
        .split('"')
        .nth(1)
        .expect("version 应是带引号的字符串");
    assert!(
        !version.is_empty() && version.contains('.'),
        "版本号格式异常：{version}"
    );

    // 所有 crate 都应继承 workspace 版本，而不是各写一份
    for c in [
        "mangosteen-core",
        "mangosteen-platform",
        "mangosteen-base",
        "mangosteen-session",
        "mangosteen-local",
        "mangosteen-backend",
        "mangosteen-cloud",
        "mangosteen-gateway",
        "mangosteen-cli",
    ] {
        // 目录去掉了 `mangosteen-` 前缀，包名保留（见 Cargo.toml 的 members）。
        let dir = c.strip_prefix("mangosteen-").unwrap_or(c);
        let m = read(&format!("crates/{dir}/Cargo.toml"));
        assert!(
            m.contains(&format!("name = \"{c}\"")),
            "`crates/{dir}/Cargo.toml` 的包名应为 `{c}`"
        );
        assert!(
            m.contains("version.workspace = true"),
            "`{c}` 应继承 workspace 版本（避免版本漂移）"
        );
    }
}

#[test]
fn cli_binary_name_is_mangosteen() {
    let m = read("crates/cli/Cargo.toml");
    assert!(
        m.contains("name = \"mangosteen\""),
        "发布件二进制名必须是 `mangosteen`"
    );
}

// ---------------------------------------------------------------- CI 配置

#[test]
fn ci_builds_all_three_platforms() {
    let ci = read(".github/workflows/release.yml");
    for target in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ] {
        assert!(
            ci.contains(target),
            "CI 未构建 `{target}`，三平台支持就无法兑现"
        );
    }
}

#[test]
fn ci_runs_architecture_constraint_tests() {
    let ci = read(".github/workflows/release.yml");
    assert!(
        ci.contains("layering"),
        "CI 必须运行架构约束测试（否则分层会腐化）"
    );
    assert!(ci.contains("cargo test --workspace"), "CI 必须运行全量测试");
}

#[test]
fn ci_verifies_before_release() {
    let ci = read(".github/workflows/release.yml");
    // build job 必须依赖 verify job
    assert!(
        ci.contains("needs: verify"),
        "构建必须在测试通过后才能开始，避免发布坏包"
    );
}

#[test]
fn release_artifacts_include_checksums() {
    let ci = read(".github/workflows/release.yml");
    assert!(
        ci.contains("sha256") || ci.contains("SHA256"),
        "发布件必须附带校验和"
    );
    let script = read("scripts/build-release.sh");
    assert!(
        script.contains("sha256") || script.contains("shasum"),
        "本地构建脚本也应生成校验和"
    );
}

#[test]
fn example_config_documents_model_sources() {
    // 新增下载能力后，示例配置必须给出模型源与默认源的说明，
    // 否则用户不知道 `models pull` 从哪下、怎么换源。
    let c = read("config/config.example.toml");
    assert!(
        c.contains("default_source"),
        "示例配置缺少 storage.default_source"
    );
    assert!(
        c.contains("model_sources"),
        "示例配置缺少 [model_sources.*] 说明"
    );
    assert!(c.contains("models pull"), "示例配置应提示 models pull 用法");
}

#[test]
fn example_config_documents_engine_library_path() {
    // 参照 Ollama 的 runner 集成方式：库路径由宿主进程注入。
    // 这两个开关必须出现在示例配置里，否则用户遇到
    // 「Library not loaded」时无从下手。
    let c = read("config/config.example.toml");
    assert!(
        c.contains("auto_library_path"),
        "示例配置缺少 local.auto_library_path"
    );
    assert!(
        c.contains("engine_env"),
        "示例配置缺少 local.engine_env 说明"
    );
}

#[test]
fn readme_documents_pull_and_engine_commands() {
    let r = read("README.md");
    for cmd in ["models pull", "models search", "models sources", "engine"] {
        assert!(
            r.contains(cmd),
            "README 未记录命令 `{cmd}`，用户无法自行发现该能力"
        );
    }
}

// ---------------------------------------------------------------- 厂商协议适配

#[test]
fn example_config_documents_compshare_provider() {
    // 优云智算走专用驱动（UCloud RPC + 签名），配置字段与 REST 服务商不同。
    // 示例配置必须给出全部必填项，否则用户配不出可用的服务商。
    let c = read("config/config.example.toml");
    for key in [
        "driver = \"compshare\"",
        "public_key",
        "image_id",
        "region",
        "zone",
        "charge_type",
        "COMPSHARE_PRIVATE_KEY",
    ] {
        assert!(
            c.contains(key),
            "示例配置缺少 compshare 的 `{key}`，用户会配不出可用服务商"
        );
    }
    // 必须说明「公钥在 options、私钥在 secret」这个易错点
    assert!(
        c.contains("公钥") && c.contains("私钥"),
        "示例配置必须说明两段凭证分别放在哪里"
    );
}

#[test]
fn provider_reference_marks_compshare_as_special() {
    // builtin.toml 里的 ucloud profile 只是字段参考，
    // 必须明确告知真实接入要用 compshare 驱动，否则用户会照着配但调不通。
    let text = read("config/providers/builtin.toml");
    assert!(
        text.contains("compshare"),
        "provider 参考件必须指出优云智算需要用 compshare 驱动"
    );
    assert!(
        text.contains("签名"),
        "provider 参考件应说明该厂商的认证是签名而非 Token 头"
    );
}

#[test]
fn readme_documents_web_ui() {
    let r = read("README.md");
    assert!(r.contains("mangosteen ui"), "README 未记录 ui 命令");
    for tab in ["对话", "部署", "设备", "设置"] {
        assert!(r.contains(tab), "README 未说明「{tab}」页签");
    }
    assert!(
        r.contains("预估时价") || r.contains("预估"),
        "README 应说明云端部署前有费用确认"
    );
}

#[test]
fn web_ui_stays_self_contained() {
    // 前端不得依赖外部资源：离线/内网环境会白屏。
    // 这条约束在 e2e 里也测了实际响应，这里再加一道静态检查，
    // 让「不小心引入 CDN」在 cargo test 阶段就失败。
    let src = read("crates/gateway/src/assets/index.html");
    // rust 源里出现这些就说明页面开始依赖外部资源
    assert!(
        !src.contains("cdn.jsdelivr")
            && !src.contains("unpkg.com")
            && !src.contains("cdnjs.cloudflare.com"),
        "webui 不得引用 CDN"
    );
    assert!(
        src.contains("mount()") && src.contains("unmount()"),
        "页签必须实现 mount/unmount，否则切换会累积定时器"
    );
}
