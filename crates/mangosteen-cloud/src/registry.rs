//! 驱动注册表构建：配置驱动的动态加载点。
//!
//! 归属：**算力执行层 · 多云统一适配层**。
//!
//! ## 加载规则
//!
//! 遍历 `config.providers`，对每个启用的服务商：
//!
//! 0. 若 `driver` 是**协议形态特殊**的服务商（当前为 `compshare`：
//!    UCloud RPC + SHA1 签名，无法用字段映射表达）→ 转交
//!    `mangosteen-backend` 构造专用驱动；
//! 1. 若配置带了 `profile` 段 → 用它构造 [`RestDriver`]（**配置路径**）；
//! 2. 否则查内置 profile（autodl / ucloud / aliyun）→ 构造 [`RestDriver`]；
//! 3. 都没有 → 记入 `skipped`，**不中断**其它服务商的加载。
//!
//! 「部分失败不中断」是关键：某个服务商密钥没配好，不该导致整个客户端不可用。
//! 这与「部署失败自动降级重试」的容错哲学一致。
//!
//! ## 为什么第 0 步要排在最前
//!
//! `compshare` 若走通用 REST 路径会构造出一个「看起来注册成功、实际调不通」
//! 的驱动：签名算法无法用 `AuthStyle` 表达。必须先分流，避免用户拿到
//! 一个假就绪的服务商。

use std::sync::Arc;

use mangosteen_core::cloud::{DriverRegistry, ProviderDriver};
use mangosteen_core::config::{Config, ProviderConfig, SecretRef};
use mangosteen_core::{MnError, MnResult};

use super::drivers::{
    builtin_profile, Credentials, CurlTransport, RestDriverProfile, Transport,
};
use super::drivers::rest::SharedRestDriver;

/// 构建结果。
pub struct RegistryBuild {
    /// 已注册驱动
    pub registry: DriverRegistry,
    /// 跳过的服务商及原因
    pub skipped: Vec<(String, String)>,
}

/// 从配置构建驱动注册表。
///
/// 使用系统 curl 作为传输后端。
pub fn build_registry(config: &Config) -> RegistryBuild {
    build_registry_with(config, || {
        Arc::new(CurlTransport::new(30)) as Arc<dyn Transport + Send + Sync>
    })
}

/// 从配置构建，传输层由调用方提供（测试注入 mock）。
pub fn build_registry_with<F>(config: &Config, make_transport: F) -> RegistryBuild
where
    F: Fn() -> Arc<dyn Transport + Send + Sync>,
{
    let mut registry = DriverRegistry::new();
    let mut skipped = Vec::new();

    for (name, pc) in &config.providers {
        if !pc.enabled {
            skipped.push((name.clone(), "未启用（enabled = false）".to_string()));
            continue;
        }
        match build_one(name, pc, &make_transport) {
            Ok(driver) => registry.register(driver),
            Err(e) => skipped.push((name.clone(), e.to_string())),
        }
    }

    RegistryBuild { registry, skipped }
}

/// 构建单个驱动。
fn build_one<F>(
    name: &str,
    pc: &ProviderConfig,
    make_transport: &F,
) -> MnResult<Box<dyn ProviderDriver>>
where
    F: Fn() -> Arc<dyn Transport + Send + Sync>,
{
    // 0. 协议形态特殊的服务商：转交 mangosteen-backend
    //    · compshare：UCloud RPC + SHA1 签名（通用 REST profile 表达不了）
    //    · autodl_pro：REST + Bearer，但库存/规格/SSH 语义都与通用 profile 不同
    if mangosteen_backend::autodl::handles(&pc.driver) {
        return build_autodl(name, pc, make_transport);
    }
    if mangosteen_backend::handles(&pc.driver) {
        return build_compshare(name, pc, make_transport);
    }

    // 1. 解析 profile：内置优先，配置可覆盖
    let profile = resolve_profile(name, pc)?;

    // 2. 解析凭证
    let creds = resolve_credentials(name, pc)?;

    // 3. 构造驱动
    let transport = make_transport();
    let driver = SharedRestDriver::from_shared(profile, creds, transport)?;
    Ok(Box::new(driver))
}

/// 把本层的 [`Transport`] 适配成 `mangosteen-backend` 的 `Transport`。
///
/// 两个 crate 各自定义了同形状的传输抽象（都不愿为对方引入依赖），
/// 这里做一次零成本桥接：只转换请求/响应结构，不复制任何逻辑。
/// 好处是 `compshare` 驱动与其它驱动**共用同一套 curl 调用设施**。
struct BackendTransport(Arc<dyn Transport + Send + Sync>);

impl mangosteen_backend::Transport for BackendTransport {
    fn execute(
        &self,
        req: &mangosteen_backend::HttpRequest,
    ) -> MnResult<mangosteen_backend::HttpResponse> {
        let converted = super::drivers::HttpRequest {
            method: req.method.clone(),
            url: req.url.clone(),
            headers: req.headers.clone(),
            body: req.body.clone(),
        };
        let resp = self.0.execute(&converted)?;
        Ok(mangosteen_backend::HttpResponse::new(resp.status, resp.body))
    }
}

/// 构造 AutoDL Pro 驱动。
///
/// 凭证只有一段：开发者 Token，走 `secret`（推荐 `{ file = "..." }`），
/// 与优云的「公钥 + 私钥」两段不同。
fn build_autodl<F>(
    name: &str,
    pc: &ProviderConfig,
    make_transport: &F,
) -> MnResult<Box<dyn ProviderDriver>>
where
    F: Fn() -> Arc<dyn Transport + Send + Sync>,
{
    if pc.secret.is_none() {
        return Err(MnError::config(
            format!("providers.{name}.secret"),
            "AutoDL 需要开发者 Token。推荐写成 secret = { file = \"...\" }，\
             或 secret = { env = \"AUTODL_TOKEN\" }（控制台 → 设置 → 开发者 Token）",
        ));
    }
    let token = pc.secret.resolve(mangosteen_platform::paths::real_env)?;
    if token.trim().is_empty() {
        return Err(MnError::config(
            format!("providers.{name}.secret"),
            "开发者 Token 为空",
        ));
    }
    let config = mangosteen_backend::autodl::AutodlConfig::from_options(&pc.options)?;
    let transport: Arc<dyn mangosteen_backend::Transport + Send + Sync> =
        Arc::new(BackendTransport(make_transport()));
    let driver = mangosteen_backend::autodl::make_driver(name, config, &token, transport)?;
    Ok(Box::new(driver))
}

/// 构造优云智算驱动。
///
/// 凭证分两段：`PublicKey` 放在 `options.public_key`（公开标识），
/// `PrivateKey` 走 `secret`（可加密来源）。这一点与 REST 服务商不同，
/// 所以单独一条路径。
fn build_compshare<F>(
    name: &str,
    pc: &ProviderConfig,
    make_transport: &F,
) -> MnResult<Box<dyn ProviderDriver>>
where
    F: Fn() -> Arc<dyn Transport + Send + Sync>,
{
    let private_key = resolve_private_key(name, pc)?;
    let transport: Arc<dyn mangosteen_backend::Transport + Send + Sync> =
        Arc::new(BackendTransport(make_transport()));
    let driver =
        mangosteen_backend::build_from_options(name, &pc.options, &private_key, transport)?;
    Ok(Box::new(driver))
}

/// 解析私钥（`secret` 字段）。
fn resolve_private_key(name: &str, pc: &ProviderConfig) -> MnResult<String> {
    if pc.secret.is_none() {
        return Err(MnError::config(
            format!("providers.{name}.secret"),
            "优云智算需要 API 私钥（AccessKey Secret）。\
             推荐写成 secret = { env = \"COMPSHARE_PRIVATE_KEY\" }，\
             公钥填在 options.public_key",
        ));
    }
    let key = pc.secret.resolve(mangosteen_platform::paths::real_env)?;
    if key.trim().is_empty() {
        return Err(MnError::config(
            format!("providers.{name}.secret"),
            "API 私钥为空",
        ));
    }
    Ok(key)
}

/// 解析 profile。
fn resolve_profile(name: &str, pc: &ProviderConfig) -> MnResult<RestDriverProfile> {
    // 优先用配置里覆盖的 profile（用户自定义或厂商改版后的修正）
    if let Some(overridden) = pc.profile.as_ref() {
        let mut p: RestDriverProfile =
            serde_json::from_value(overridden.clone()).map_err(|e| {
                MnError::config(
                    format!("providers.{name}.profile"),
                    format!("profile 结构非法：{e}"),
                )
            })?;
        if p.name.trim().is_empty() {
            p.name = name.to_string();
        }
        p.validate()?;
        return Ok(p);
    }
    // 退回内置
    builtin_profile(&pc.driver).ok_or_else(|| {
        MnError::DriverUnavailable {
            provider: format!(
                "{}（驱动 `{}` 既无内置 profile，配置里也没有 profile 段）",
                name, pc.driver
            ),
        }
    })
}

/// 解析凭证。
///
/// 双凭证只在驱动**确实使用** `DoubleHeader` 认证时才成立。
/// 不能凭「配置里有没有 access_key_id」判断——那会把单 Token 服务商误判成双凭证。
fn resolve_credentials(name: &str, pc: &ProviderConfig) -> MnResult<Credentials> {
    let secret = &pc.secret;
    if secret.is_none() {
        return Err(MnError::config(
            format!("providers.{name}.secret"),
            "未配置凭证。云端算力必须提供 API Token（推荐用 env 形式避免明文落盘）",
        ));
    }
    let id = secret.resolve(mangosteen_platform::paths::real_env)?;

    // 只有 profile 声明了 DoubleHeader，才去 options 里取第二段凭证
    let needs_second = pc
        .profile
        .as_ref()
        .map(profile_uses_double_header)
        .unwrap_or(false)
        || pc
            .profile
            .is_none()
            .then(|| builtin_profile(&pc.driver))
            .flatten()
            .map(|p| matches!(p.auth, super::drivers::AuthStyle::DoubleHeader { .. }))
            .unwrap_or(false);

    if !needs_second {
        return Ok(Credentials::token(id));
    }

    let second = pc
        .options
        .get("access_key_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            MnError::config(
                format!("providers.{name}.options.access_key_id"),
                "该服务商使用双凭证认证，必须提供 access_key_id",
            )
        })?;
    Ok(Credentials::pair(id, second))
}

/// 判断配置里的 profile 是否使用双凭证认证。
fn profile_uses_double_header(profile: &serde_json::Value) -> bool {
    profile
        .get("auth")
        .and_then(|a| a.get("type"))
        .and_then(|t| t.as_str())
        .map(|t| t == "double_header")
        .unwrap_or(false)
}

/// 检查配置能否构成可用的云端能力，返回人类可读的诊断。
///
/// 供 `mangosteen doctor` 使用。
pub fn diagnose(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let enabled: Vec<&String> = config
        .providers
        .iter()
        .filter(|(_, p)| p.enabled)
        .map(|(k, _)| k)
        .collect();

    if enabled.is_empty() {
        out.push("未启用任何云服务商：本地资源不足时无法降级到云端".to_string());
        return out;
    }

    for name in enabled {
        let pc = &config.providers[name];

        // 协议特殊的服务商有单独的配置要求，单独诊断。
        //
        // 这两个驱动由 `mangosteen-backend` 提供，**没有** REST profile——
        // 漏掉这个分流会让诊断输出「无内置 profile，将被跳过」这种
        // 与事实相反的警告（实际能正常注册）。
        if mangosteen_backend::autodl::handles(&pc.driver)
            || mangosteen_backend::handles(&pc.driver)
        {
            let built = if mangosteen_backend::autodl::handles(&pc.driver) {
                build_autodl(name, pc, &|| {
                    Arc::new(CurlTransport::new(30)) as Arc<dyn Transport + Send + Sync>
                })
            } else {
                build_compshare(name, pc, &|| {
                    Arc::new(CurlTransport::new(30)) as Arc<dyn Transport + Send + Sync>
                })
            };
            if let Err(e) = built {
                out.push(format!("服务商 `{name}`：{e}"));
            }
            continue;
        }

        match resolve_credentials(name, pc) {
            Ok(_) => {
                if builtin_profile(&pc.driver).is_none() && pc.profile.is_none() {
                    out.push(format!(
                        "服务商 `{name}`：驱动 `{}` 无内置 profile 且未提供 profile 段，将被跳过",
                        pc.driver
                    ));
                }
            }
            Err(e) => out.push(format!("服务商 `{name}`：{e}")),
        }
    }

    if let SecretRef::Inline(_) = config
        .providers
        .values()
        .find(|p| p.enabled)
        .map(|p| p.secret.clone())
        .unwrap_or(SecretRef::None)
    {
        out.push(
            "检测到内联明文密钥：建议改用 secret = { env = \"VAR_NAME\" } 形式".to_string(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transport::MockTransport;
    use mangosteen_core::config::SchedulerConfig;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn base_config() -> Config {
        Config {
            cloud: Default::default(),
            config_version: 1,
            gateway: Default::default(),
            scheduler: SchedulerConfig::default(),
            local: Default::default(),
            storage: mangosteen_core::config::StorageConfig {
                default_source: "modelscope".to_string(),
                models_dir: PathBuf::from("/m"),
                cache_dir: PathBuf::from("/c"),
                allowed_read_roots: vec![],
                verify_hash: false,
                max_model_bytes: 1 << 30,
            },
            providers: BTreeMap::new(),
            model_sources: BTreeMap::new(),
        }
    }

    fn provider(driver: &str, secret: SecretRef, enabled: bool) -> ProviderConfig {
        ProviderConfig {
            driver: driver.to_string(),
            enabled,
            display_name: None,
            secret,
            options: BTreeMap::new(),
            priority: 1.0,
            profile: None,
        }
    }

    fn mock_factory() -> impl Fn() -> Arc<dyn Transport + Send + Sync> {
        || {
            Arc::new(MockTransport::always(
                200,
                r#"{"code":"Success","data":{"list":[]}}"#,
            )) as Arc<dyn Transport + Send + Sync>
        }
    }

    #[test]
    fn skips_disabled_providers() {
        let mut c = base_config();
        c.providers.insert(
            "autodl".into(),
            provider("autodl", SecretRef::Inline("tok".into()), false),
        );
        let r = build_registry_with(&c, mock_factory());
        assert!(r.registry.is_empty());
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].1.contains("未启用"));
    }

    #[test]
    fn registers_enabled_builtin_provider() {
        let mut c = base_config();
        c.providers.insert(
            "autodl".into(),
            provider("autodl", SecretRef::Inline("tok".into()), true),
        );
        let r = build_registry_with(&c, mock_factory());
        assert_eq!(r.registry.len(), 1);
        assert!(r.registry.contains("autodl"));
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn multiple_providers_all_registered() {
        let mut c = base_config();
        for name in ["autodl", "ucloud"] {
            c.providers.insert(
                name.into(),
                provider(name, SecretRef::Inline("tok".into()), true),
            );
        }
        // 阿里云使用双凭证认证，需同时提供 access_key_id
        let mut ali = provider("aliyun", SecretRef::Inline("secret".into()), true);
        ali.options
            .insert("access_key_id".into(), "LTAI-test".into());
        c.providers.insert("aliyun".into(), ali);
        let r = build_registry_with(&c, mock_factory());
        assert_eq!(r.registry.len(), 3);
        assert_eq!(r.registry.names(), vec!["aliyun", "autodl", "ucloud"]);
    }

    #[test]
    fn missing_secret_is_skipped_not_fatal() {
        let mut c = base_config();
        c.providers.insert(
            "autodl".into(),
            provider("autodl", SecretRef::None, true),
        );
        c.providers.insert(
            "ucloud".into(),
            provider("ucloud", SecretRef::Inline("tok".into()), true),
        );
        let r = build_registry_with(&c, mock_factory());
        // 一个失败不应影响另一个（容错要求）
        assert_eq!(r.registry.len(), 1);
        assert!(r.registry.contains("ucloud"));
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(r.skipped[0].0, "autodl");
        assert!(r.skipped[0].1.contains("凭证"));
    }

    #[test]
    fn unknown_driver_is_skipped_with_reason() {
        let mut c = base_config();
        c.providers.insert(
            "mystery".into(),
            provider("mystery-cloud", SecretRef::Inline("tok".into()), true),
        );
        let r = build_registry_with(&c, mock_factory());
        assert!(r.registry.is_empty());
        assert!(r.skipped[0].1.contains("mystery-cloud"));
    }

    #[test]
    fn env_secret_resolution() {
        // 用真实过程环境：设置后应能解析
        std::env::set_var("MANGOSTEEN_TEST_TOKEN", "env-token-value");
        let mut c = base_config();
        c.providers.insert(
            "autodl".into(),
            provider(
                "autodl",
                SecretRef::Env("MANGOSTEEN_TEST_TOKEN".into()),
                true,
            ),
        );
        let r = build_registry_with(&c, mock_factory());
        assert_eq!(r.registry.len(), 1, "skipped: {:?}", r.skipped);
        std::env::remove_var("MANGOSTEEN_TEST_TOKEN");
    }

    #[test]
    fn missing_env_secret_is_reported() {
        let mut c = base_config();
        c.providers.insert(
            "autodl".into(),
            provider(
                "autodl",
                SecretRef::Env("MANGOSTEEN_ABSENT_TOKEN_XYZ".into()),
                true,
            ),
        );
        let r = build_registry_with(&c, mock_factory());
        assert!(r.registry.is_empty());
        assert!(r.skipped[0].1.contains("环境变量"), "got {:?}", r.skipped);
    }

    #[test]
    fn diagnose_flags_no_providers() {
        let c = base_config();
        let d = diagnose(&c);
        assert!(d[0].contains("未启用任何云服务商"));
    }

    #[test]
    fn diagnose_flags_inline_secret() {
        let mut c = base_config();
        c.providers.insert(
            "autodl".into(),
            provider("autodl", SecretRef::Inline("plain-text-key".into()), true),
        );
        let d = diagnose(&c);
        assert!(d.iter().any(|s| s.contains("内联明文密钥")), "{d:?}");
    }

    #[test]
    fn diagnose_flags_missing_credentials() {
        let mut c = base_config();
        c.providers.insert(
            "autodl".into(),
            provider("autodl", SecretRef::None, true),
        );
        let d = diagnose(&c);
        assert!(d.iter().any(|s| s.contains("未配置")), "{d:?}");
    }

    #[test]
    fn registry_drivers_are_usable() {
        let mut c = base_config();
        c.providers.insert(
            "autodl".into(),
            provider("autodl", SecretRef::Inline("tok".into()), true),
        );
        let r = build_registry_with(&c, mock_factory());
        let driver = r.registry.get("autodl").unwrap();
        // mock 返回空列表
        assert!(driver.list_available_specs().unwrap().is_empty());
        assert_eq!(driver.display_name(), "AutoDL");
    }

    // ---------------------------------------------------------- compshare 分流

    /// 构造一个 compshare 服务商配置。
    fn compshare_provider(secret: SecretRef, with_public_key: bool) -> ProviderConfig {
        let mut options = BTreeMap::new();
        options.insert("image_id".to_string(), "img-1".to_string());
        options.insert("region".to_string(), "cn-wlcb".to_string());
        options.insert("zone".to_string(), "cn-wlcb-01".to_string());
        if with_public_key {
            options.insert("public_key".to_string(), "pk-test".to_string());
        }
        ProviderConfig {
            driver: "compshare".to_string(),
            enabled: true,
            display_name: None,
            secret,
            options,
            priority: 1.0,
            profile: None,
        }
    }

    #[test]
    fn compshare_is_registered_via_backend() {
        let mut c = base_config();
        c.providers.insert(
            "compshare".into(),
            compshare_provider(SecretRef::Inline("sk-test".into()), true),
        );
        let r = build_registry_with(&c, mock_factory());
        assert_eq!(r.registry.len(), 1, "skipped: {:?}", r.skipped);
        let d = r.registry.get("compshare").unwrap();
        assert_eq!(d.display_name(), "优云智算（Compshare）");
        // 能力必须如实声明
        let caps = d.capabilities();
        assert!(caps.on_demand_create);
        assert!(caps.stop_start);
    }

    #[test]
    fn compshare_without_public_key_is_skipped_with_hint() {
        let mut c = base_config();
        c.providers.insert(
            "compshare".into(),
            compshare_provider(SecretRef::Inline("sk-test".into()), false),
        );
        let r = build_registry_with(&c, mock_factory());
        assert!(r.registry.is_empty());
        assert_eq!(r.skipped.len(), 1);
        // 错误必须告诉用户公钥该填哪
        assert!(r.skipped[0].1.contains("public_key"), "{:?}", r.skipped);
    }

    #[test]
    fn compshare_without_secret_is_skipped() {
        let mut c = base_config();
        c.providers.insert(
            "compshare".into(),
            compshare_provider(SecretRef::None, true),
        );
        let r = build_registry_with(&c, mock_factory());
        assert!(r.registry.is_empty());
        assert!(r.skipped[0].1.contains("私钥"), "{:?}", r.skipped);
    }

    #[test]
    fn compshare_without_image_id_is_skipped() {
        let mut c = base_config();
        let mut pc = compshare_provider(SecretRef::Inline("sk".into()), true);
        pc.options.remove("image_id");
        c.providers.insert("compshare".into(), pc);
        let r = build_registry_with(&c, mock_factory());
        assert!(r.registry.is_empty());
        assert!(r.skipped[0].1.contains("image_id"), "{:?}", r.skipped);
    }

    #[test]
    fn compshare_failure_does_not_break_other_providers() {
        // 容错要求：一个服务商配置错误，其它仍应可用
        let mut c = base_config();
        c.providers.insert(
            "compshare".into(),
            compshare_provider(SecretRef::None, true), // 缺私钥 → 失败
        );
        c.providers.insert(
            "autodl".into(),
            provider("autodl", SecretRef::Inline("tok".into()), true),
        );
        let r = build_registry_with(&c, mock_factory());
        assert_eq!(r.registry.len(), 1);
        assert!(r.registry.contains("autodl"));
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(r.skipped[0].0, "compshare");
    }

    #[test]
    fn compshare_driver_name_is_not_confused_with_ucloud_profile() {
        // `ucloud` 走通用 REST profile，`compshare` 走 backend，两者不能混
        assert!(mangosteen_backend::handles("compshare"));
        assert!(!mangosteen_backend::handles("ucloud"));
        assert!(builtin_profile("ucloud").is_some());
    }

    #[test]
    fn diagnose_reports_compshare_config_problem() {
        let mut c = base_config();
        c.providers.insert(
            "compshare".into(),
            compshare_provider(SecretRef::None, true),
        );
        let d = diagnose(&c);
        assert!(
            d.iter().any(|s| s.contains("compshare") && s.contains("私钥")),
            "{d:?}"
        );
    }

    #[test]
    fn diagnose_is_silent_for_valid_compshare() {
        // 用 env 形式提供私钥，避免触发「内联明文密钥」告警——
        // 那是有意保留的既有提示，不属于 compshare 配置问题
        std::env::set_var("MANGOSTEEN_TEST_COMPSHARE_SK", "sk-test");
        let mut c = base_config();
        c.providers.insert(
            "compshare".into(),
            compshare_provider(
                SecretRef::Env("MANGOSTEEN_TEST_COMPSHARE_SK".into()),
                true,
            ),
        );
        let d = diagnose(&c);
        assert!(d.is_empty(), "合法配置不应有告警：{d:?}");
        std::env::remove_var("MANGOSTEEN_TEST_COMPSHARE_SK");
    }
}
