//! 打印实例 describe 关键字段（Password/State/SshLoginCommand）并落盘原始 JSON。
use mangosteen_provider::compshare::client::Scope;
use mangosteen_provider::compshare::params::Params;
use mangosteen_provider::compshare::{CompshareClient, Credential, CurlTransport};

fn main() {
    let pk = std::env::var("COMPSHARE_PUBLIC_KEY").unwrap();
    let sk = std::env::var("COMPSHARE_PRIVATE_KEY").unwrap();
    let id = std::env::var("PROBE_ID").unwrap();
    let cred = Credential::new(pk, sk).unwrap();
    let client = CompshareClient::new(
        "https://api.compshare.cn".to_string(),
        cred,
        CurlTransport::new(300),
        "cn-wlcb",
        "cn-wlcb-01",
    )
    .unwrap();
    let mut p = Params::new();
    p.set("UHostIds.0", id);
    let v = client
        .call("DescribeCompShareInstance", p, Scope::RegionAndZone)
        .unwrap();
    let s = serde_json::to_string(&v).unwrap();
    std::fs::write("/tmp/inst_probe.json", &s).unwrap();
    println!("written {} bytes", s.len());
    for key in ["Password", "State", "SshLoginCommand"] {
        let needle = format!("\"{key}\":");
        match s.find(&needle) {
            Some(pos) => {
                let start = pos + needle.len();
                let end = s[start..].find(',').map(|e| start + e).unwrap_or(s.len());
                let val = &s[start..end];
                let shown = &val[..val.len().min(80)];
                println!("{key} = {shown}");
            }
            None => println!("{key} = <字段不存在>"),
        }
    }
}
