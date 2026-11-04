//! 列出全部实例（无过滤），带 ChargeType/IsSpot，排查实例被回收原因。
use mangosteen_provider::compshare::client::Scope;
use mangosteen_provider::compshare::params::Params;
use mangosteen_provider::compshare::{CompshareClient, Credential, CurlTransport};

fn main() {
    let pk = std::env::var("COMPSHARE_PUBLIC_KEY").unwrap();
    let sk = std::env::var("COMPSHARE_PRIVATE_KEY").unwrap();
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
    p.set("Limit", "20");
    p.set("Offset", "0");
    let v = client
        .call("DescribeCompShareInstance", p, Scope::RegionAndZone)
        .unwrap();
    let s = serde_json::to_string(&v).unwrap();
    std::fs::write("/tmp/inst_all.json", &s).unwrap();
    println!("{}", s.len());
}
