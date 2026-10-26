//! 内置服务商驱动配置：AutoDL、优云智算、阿里云。
//!
//! 归属：**算力执行层 · 多云统一适配层**。
//!
//! ## 重要说明：为什么这些配置是「模板」而非「已核对的生产配置」
//!
//! 云厂商的 REST API 细节（路径、字段名、状态枚举）会随版本变化，且部分厂商
//! 的开放接口仅对签约客户提供。本模块提供的是**结构完整、可直接运行的默认
//! 配置**，字段名按各厂商公开文档的常见形态填写，并遵循两个原则：
//!
//! 1. **能力优先对齐产品基线**：优选的 GPU 型号（RTX 4090 / 5090）、
//!    按量计费、秒级释放等字段一律置为可用；
//! 2. **可覆盖**：所有字段都能被用户配置覆盖（见 `providers.<name>.profile`）。
//!    若厂商改版，用户改配置即可，**不必等新版本发布**。
//!
//! 这条设计正是「配置驱动原则」的直接收益：把易变的外部契约推出编译期。

use std::collections::BTreeMap;

use mangosteen_core::cloud::{DriverCapabilities, InstanceState};

use super::rest::{AuthStyle, EndpointProfile, FieldMap, RestDriverProfile, StateMap};

/// AutoDL（https://www.autodl.com）。
///
/// 定位：国内个人开发者最常用的 GPU 租赁平台，按量计费、4090 库存充足，
/// 与本产品「个人级 + 消费级显卡」的定位高度契合，故设为**默认首选**。
pub fn autodl_profile() -> RestDriverProfile {
    RestDriverProfile {
        name: "autodl".to_string(),
        display_name: "AutoDL".to_string(),
        auth: AuthStyle::Bearer,
        endpoints: EndpointProfile {
            base_url: "https://api.autodl.com".to_string(),
            list_specs: "/api/v1/machine/region/available".to_string(),
            create: "/api/v1/instance?snapshot_id={spec_id}".to_string(),
            start: "/api/v1/instance/{id}/power_on".to_string(),
            stop: "/api/v1/instance/{id}/power_off".to_string(),
            destroy: "/api/v1/instance/{id}".to_string(),
            describe: "/api/v1/instance/{id}".to_string(),
            list: "/api/v1/instance".to_string(),
            status_field: Some("code".to_string()),
            success_code: Some("Success".to_string()),
            error_field: Some("msg".to_string()),
        },
        fields: FieldMap {
            list_root: "data.list".to_string(),
            item_root: "data".to_string(),
            id: "instance_id".to_string(),
            state: "status".to_string(),
            gpu_model: "gpu_name".to_string(),
            gpu_count: "gpu_num".to_string(),
            vram: "gpu_memory".to_string(),
            vram_unit: "gib".to_string(),
            cpu_cores: "cpu_num".to_string(),
            memory: "memory".to_string(),
            memory_unit: "gib".to_string(),
            disk: "disk_size".to_string(),
            disk_unit: "gib".to_string(),
            image: Some("image_name".to_string()),
            price: Some("price_per_hour".to_string()),
            ssh_host: Some("ssh_host".to_string()),
            ssh_port: Some("ssh_port".to_string()),
            region: Some("region_name".to_string()),
            stock: Some("stock".to_string()),
            display_name: Some("instance_name".to_string()),
            created_at: Some("created_at".to_string()),
        },
        states: StateMap {
            mapping: BTreeMap::from([
                ("running".to_string(), InstanceState::Running),
                ("shutdown".to_string(), InstanceState::Stopped),
                ("stopped".to_string(), InstanceState::Stopped),
                ("creating".to_string(), InstanceState::Creating),
                ("starting".to_string(), InstanceState::Starting),
                ("pending".to_string(), InstanceState::Pending),
                ("error".to_string(), InstanceState::Failed),
                ("failed".to_string(), InstanceState::Failed),
            ]),
            fallback: InstanceState::Failed,
        },
        capabilities: DriverCapabilities {
            on_demand_create: true,
            stop_start: true,
            price_query: true,
            // AutoDL 的自定义服务需额外配置，默认走 SSH 隧道
            public_inference_url: false,
            custom_image: true,
            gpu_models: vec![
                "RTX 4090".to_string(),
                "RTX 5090".to_string(),
                "RTX 3090".to_string(),
                "A100".to_string(),
            ],
        },
        create_extra: BTreeMap::from([(
            "image_name".to_string(),
            serde_json::json!("base-image-cuda12.1"),
        )]),
    }
}

/// 优云智算。
///
/// 定位：国内 GPU 算力池，按量计费，4090/5090 均有供给。
///
/// ⚠ **本 profile 仅作字段映射参考**。优云智算的真实接口是 UCloud RPC 形态
/// （单一端点 + `Action` 参数 + SHA1 签名），无法用字段映射表达，实际调用
/// 由 `mangosteen-backend` 的专用驱动负责（配置里写 `driver = "compshare"`）。
/// 保留本函数是为了让「配置路径」的用户仍能拿到一份字段清单。
pub fn ucloud_profile() -> RestDriverProfile {
    RestDriverProfile {
        name: "ucloud".to_string(),
        display_name: "优云智算".to_string(),
        auth: AuthStyle::Header {
            name: "Authorization".to_string(),
        },
        endpoints: EndpointProfile {
            base_url: "https://api.ucloudai.com".to_string(),
            list_specs: "/v1/gpu/specs".to_string(),
            create: "/v1/instances?spec_id={spec_id}".to_string(),
            start: "/v1/instances/{id}/start".to_string(),
            stop: "/v1/instances/{id}/stop".to_string(),
            destroy: "/v1/instances/{id}".to_string(),
            describe: "/v1/instances/{id}".to_string(),
            list: "/v1/instances".to_string(),
            status_field: Some("ret_code".to_string()),
            success_code: Some("0".to_string()),
            error_field: Some("message".to_string()),
        },
        fields: FieldMap {
            list_root: "data.items".to_string(),
            item_root: "data".to_string(),
            id: "instance_id".to_string(),
            state: "state".to_string(),
            gpu_model: "gpu_type".to_string(),
            gpu_count: "gpu_count".to_string(),
            vram: "gpu_memory_gb".to_string(),
            vram_unit: "gib".to_string(),
            cpu_cores: "cpu".to_string(),
            memory: "memory_gb".to_string(),
            memory_unit: "gib".to_string(),
            disk: "disk_gb".to_string(),
            disk_unit: "gib".to_string(),
            image: Some("image_id".to_string()),
            price: Some("hour_price".to_string()),
            ssh_host: Some("public_ip".to_string()),
            ssh_port: Some("ssh_port".to_string()),
            region: Some("zone".to_string()),
            stock: Some("available".to_string()),
            display_name: Some("name".to_string()),
            created_at: Some("create_time".to_string()),
        },
        states: StateMap {
            mapping: BTreeMap::from([
                ("running".to_string(), InstanceState::Running),
                ("stopped".to_string(), InstanceState::Stopped),
                ("shutdown".to_string(), InstanceState::Stopped),
                ("pending".to_string(), InstanceState::Creating),
                ("deploying".to_string(), InstanceState::Starting),
                ("failed".to_string(), InstanceState::Failed),
            ]),
            fallback: InstanceState::Failed,
        },
        capabilities: DriverCapabilities {
            on_demand_create: true,
            stop_start: true,
            price_query: true,
            public_inference_url: false,
            custom_image: true,
            gpu_models: vec!["RTX 4090".to_string(), "RTX 5090".to_string()],
        },
        create_extra: BTreeMap::new(),
    }
}

/// 阿里云 ECS（GPU 规格族）。
///
/// 定位：扩展兼容，非核心优化目标。企业级 API 需要 RPC 签名，
/// 这里给出 `DoubleHeader` 形式的占位配置；实际生产使用时建议
/// 用独立驱动实现签名（[`crate::drivers`] 文档中的「代码路径」）。
pub fn aliyun_profile() -> RestDriverProfile {
    RestDriverProfile {
        name: "aliyun".to_string(),
        display_name: "阿里云".to_string(),
        auth: AuthStyle::DoubleHeader {
            id_header: "x-acs-access-key-id".to_string(),
            secret_header: "x-acs-access-key-secret".to_string(),
        },
        endpoints: EndpointProfile {
            base_url: "https://ecs.cn-hangzhou.aliyuncs.com".to_string(),
            list_specs: "/api/v1/gpu/specs".to_string(),
            create: "/api/v1/instances?instance_type={spec_id}".to_string(),
            start: "/api/v1/instances/{id}/start".to_string(),
            stop: "/api/v1/instances/{id}/stop".to_string(),
            destroy: "/api/v1/instances/{id}".to_string(),
            describe: "/api/v1/instances/{id}".to_string(),
            list: "/api/v1/instances".to_string(),
            status_field: Some("Code".to_string()),
            success_code: Some("200".to_string()),
            error_field: Some("Message".to_string()),
        },
        fields: FieldMap {
            list_root: "Instances.Instance".to_string(),
            item_root: "Instance".to_string(),
            id: "InstanceId".to_string(),
            state: "Status".to_string(),
            gpu_model: "GPU Model".to_string(),
            gpu_count: "GPUAmount".to_string(),
            vram: "GPUMemory".to_string(),
            vram_unit: "gib".to_string(),
            cpu_cores: "Cpu".to_string(),
            memory: "Memory".to_string(),
            memory_unit: "mib".to_string(),
            disk: "DiskSize".to_string(),
            disk_unit: "gib".to_string(),
            image: Some("ImageId".to_string()),
            price: Some("Price".to_string()),
            ssh_host: Some("PublicIpAddress.IpAddress[0]".to_string()),
            ssh_port: None,
            region: Some("RegionId".to_string()),
            stock: None,
            display_name: Some("InstanceName".to_string()),
            created_at: Some("CreationTime".to_string()),
        },
        states: StateMap {
            mapping: BTreeMap::from([
                ("running".to_string(), InstanceState::Running),
                ("stopped".to_string(), InstanceState::Stopped),
                ("starting".to_string(), InstanceState::Starting),
                ("stopping".to_string(), InstanceState::Stopping),
                ("pending".to_string(), InstanceState::Creating),
            ]),
            fallback: InstanceState::Failed,
        },
        capabilities: DriverCapabilities {
            on_demand_create: true,
            stop_start: true,
            price_query: false,
            public_inference_url: true,
            custom_image: true,
            gpu_models: vec!["RTX 4090".to_string(), "A10".to_string()],
        },
        create_extra: BTreeMap::new(),
    }
}

/// 按驱动名取内置配置。
pub fn builtin_profile(name: &str) -> Option<RestDriverProfile> {
    match name {
        "autodl" => Some(autodl_profile()),
        "ucloud" => Some(ucloud_profile()),
        "aliyun" => Some(aliyun_profile()),
        _ => None,
    }
}

/// 全部内置驱动名。
pub fn builtin_driver_names() -> &'static [&'static str] {
    &["autodl", "ucloud", "aliyun"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtin_profiles_are_valid() {
        for name in builtin_driver_names() {
            let p = builtin_profile(name).unwrap_or_else(|| panic!("missing {name}"));
            p.validate()
                .unwrap_or_else(|e| panic!("{name} 配置非法：{e}"));
            assert_eq!(&p.name, name);
        }
    }

    #[test]
    fn autodl_is_the_default_pick() {
        let p = autodl_profile();
        assert!(p.capabilities.gpu_models.contains(&"RTX 4090".to_string()));
        assert!(p.capabilities.gpu_models.contains(&"RTX 5090".to_string()));
        assert!(p.capabilities.on_demand_create);
        assert!(p.capabilities.stop_start);
    }

    #[test]
    fn aliyun_is_extension_only() {
        // 产品基线：阿里云为扩展兼容，不作为核心优化目标
        let p = aliyun_profile();
        assert_eq!(p.display_name, "阿里云");
        // 阿里云内存以 MiB 计，单位配置必须正确，否则显存/内存估算差 1024 倍
        assert_eq!(p.fields.memory_unit, "mib");
    }

    #[test]
    fn state_maps_cover_common_vendor_states() {
        for p in [autodl_profile(), ucloud_profile()] {
            assert_eq!(p.states.normalize("running"), InstanceState::Running);
            assert_eq!(p.states.normalize("stopped"), InstanceState::Stopped);
            // 未知状态不能误判为 Running（会导致对不可用实例发请求）
            assert_ne!(p.states.normalize("banana"), InstanceState::Running);
        }
    }

    #[test]
    fn unknown_driver_returns_none() {
        assert!(builtin_profile("digitalocean").is_none());
    }

    #[test]
    fn profiles_are_serializable_for_config_override() {
        // 内置配置必须能被序列化成 TOML，用户才能复制出来改
        for name in builtin_driver_names() {
            let p = builtin_profile(name).unwrap();
            let toml = toml::to_string(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(toml.contains("base_url"));
            let back: RestDriverProfile = toml::from_str(&toml).unwrap();
            assert_eq!(back, p);
        }
    }
}
