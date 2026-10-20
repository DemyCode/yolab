use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::host::Host;
use crate::mesh::candidates::is_physical;

const POLL_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    V4,
    V6,
}

impl Family {
    fn ip_flag(self) -> &'static str {
        match self {
            Family::V4 => "-4",
            Family::V6 => "-6",
        }
    }

    fn ipset_family(self) -> &'static str {
        match self {
            Family::V4 => "inet",
            Family::V6 => "inet6",
        }
    }
}

fn is_lan_destination(dst: &str) -> bool {
    let d = dst.to_ascii_lowercase();
    !(d == "default"
        || d.starts_with("fe80:")
        || d.starts_with("ff")
        || d.starts_with("169.254.")
        || d.starts_with("127.")
        || d.starts_with("::1")
        || d.split('.').next().and_then(|o| o.parse::<u8>().ok()).is_some_and(|o| o >= 224))
}

fn parse_on_link_subnets(json: &str) -> Result<BTreeSet<String>> {
    if json.trim().is_empty() {
        return Ok(BTreeSet::new());
    }
    let routes: serde_json::Value = serde_json::from_str(json).context("parse ip -j route")?;
    let Some(routes) = routes.as_array() else {
        bail!("ip -j route did not print a list: {json}");
    };
    Ok(routes
        .iter()
        .filter(|r| r.get("gateway").is_none())
        .filter(|r| r["dev"].as_str().is_some_and(is_physical))
        .filter_map(|r| r["dst"].as_str())
        .filter(|dst| is_lan_destination(dst))
        .map(str::to_string)
        .collect())
}

async fn on_link_subnets<H: Host>(host: &H, family: Family) -> Result<BTreeSet<String>> {
    let out = host
        .run_checked("ip", &["-j", family.ip_flag(), "route", "show", "table", "main"])
        .await
        .context("read the routing table")?;
    parse_on_link_subnets(&out)
}

async fn replace_set<H: Host>(
    host: &H,
    name: &str,
    family: Family,
    subnets: &BTreeSet<String>,
) -> Result<()> {
    let next = format!("{name}-next");
    let fam = family.ipset_family();
    host.run_checked("ipset", &["create", name, "hash:net", "family", fam, "-exist"])
        .await?;
    host.run_checked("ipset", &["create", &next, "hash:net", "family", fam, "-exist"])
        .await?;
    host.run_checked("ipset", &["flush", &next]).await?;
    for subnet in subnets {
        host.run_checked("ipset", &["add", &next, subnet, "-exist"])
            .await?;
    }
    host.run_checked("ipset", &["swap", &next, name]).await?;
    host.run_checked("ipset", &["destroy", &next]).await?;
    Ok(())
}

async fn sync_family<H: Host>(
    host: &H,
    name: &str,
    family: Family,
    last: &mut Option<BTreeSet<String>>,
) -> Result<()> {
    let subnets = on_link_subnets(host, family).await?;
    if last.as_ref() == Some(&subnets) {
        return Ok(());
    }
    replace_set(host, name, family, &subnets).await?;
    tracing::info!("{name}: LAN subnets now {subnets:?}");
    *last = Some(subnets);
    Ok(())
}

pub async fn run<H: Host>(host: &H, set4: &str, set6: &str) -> Result<()> {
    if set4.is_empty() || set6.is_empty() {
        bail!("YOLAB_LAN_SET4 and YOLAB_LAN_SET6 must name the firewall's ipsets");
    }
    let mut last4 = None;
    let mut last6 = None;
    loop {
        sync_family(host, set4, Family::V4, &mut last4).await?;
        sync_family(host, set6, Family::V6, &mut last6).await?;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    const WIRED_V4: &str = r#"[
        {"dst":"default","gateway":"192.168.1.254","dev":"enp5s0","protocol":"dhcp","metric":100,"flags":[]},
        {"dst":"10.42.0.0/24","dev":"cni0","protocol":"kernel","scope":"link","prefsrc":"10.42.0.1","flags":[]},
        {"dst":"10.42.1.0/24","gateway":"10.42.1.0","dev":"flannel.1","flags":["onlink"]},
        {"dst":"192.168.1.0/24","dev":"enp5s0","protocol":"kernel","scope":"link","prefsrc":"192.168.1.15","metric":100,"flags":[]}
    ]"#;

    const WIRED_V6: &str = r#"[
        {"dst":"2a01:e0a:80a:ba90::/64","dev":"enp5s0","protocol":"ra","metric":100,"flags":[],"pref":"medium"},
        {"dst":"fd00:42::/64","dev":"cni0","protocol":"kernel","metric":256,"flags":[],"pref":"medium"},
        {"dst":"fd00:42:0:1::/64","gateway":"fd00:42:0:1::","dev":"flannel-v6.1","metric":1024,"flags":["onlink"],"pref":"medium"},
        {"dst":"fd00:cafe::9","dev":"wg1","protocol":"boot","metric":1024,"flags":[],"pref":"medium"},
        {"dst":"fe80::/64","dev":"enp5s0","protocol":"kernel","metric":256,"flags":[],"pref":"medium"},
        {"dst":"default","gateway":"fe80::1","dev":"enp5s0","protocol":"ra","metric":100,"flags":[],"pref":"medium"}
    ]"#;

    const WIFI_V4: &str = r#"[
        {"dst":"default","gateway":"10.0.0.1","dev":"wlp2s0","protocol":"dhcp","metric":600,"flags":[]},
        {"dst":"10.0.0.0/24","dev":"wlp2s0","protocol":"kernel","scope":"link","prefsrc":"10.0.0.7","metric":600,"flags":[]}
    ]"#;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn scripted(v4: &str, v6: &str) -> FakeHost {
        FakeHost::new()
            .ok("ip -j -4 route show table main", v4)
            .ok("ip -j -6 route show table main", v6)
            .ok("ipset", "")
    }

    #[test]
    fn only_the_wired_lan_is_trusted_not_pods_mesh_or_the_internet() {
        assert_eq!(
            parse_on_link_subnets(WIRED_V4).unwrap(),
            set(&["192.168.1.0/24"])
        );
        assert_eq!(
            parse_on_link_subnets(WIRED_V6).unwrap(),
            set(&["2a01:e0a:80a:ba90::/64"])
        );
    }

    #[test]
    fn a_wifi_lan_is_trusted_the_same_as_a_wired_one() {
        assert_eq!(parse_on_link_subnets(WIFI_V4).unwrap(), set(&["10.0.0.0/24"]));
    }

    #[test]
    fn link_local_multicast_and_loopback_are_never_trusted() {
        for dst in [
            "fe80::/64",
            "ff00::/8",
            "169.254.0.0/16",
            "127.0.0.0/8",
            "224.0.0.0/4",
            "239.255.255.250",
            "default",
        ] {
            assert!(!is_lan_destination(dst), "{dst}");
        }
    }

    #[test]
    fn a_machine_with_no_network_trusts_nothing() {
        assert!(parse_on_link_subnets("").unwrap().is_empty());
        assert!(parse_on_link_subnets("[]").unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_routing_table_is_an_error_not_an_empty_lan() {
        assert!(parse_on_link_subnets("{not json").is_err());
        assert!(parse_on_link_subnets(r#"{"dst":"x"}"#).is_err());
    }

    #[tokio::test]
    async fn the_set_is_rebuilt_aside_then_swapped_in_atomically() {
        let host = scripted(WIRED_V4, WIRED_V6);
        let mut last = None;

        sync_family(&host, "yolab-lan6", Family::V6, &mut last).await.unwrap();

        let calls = host.calls();
        let ipset: Vec<&str> = calls
            .iter()
            .map(String::as_str)
            .filter(|c| c.starts_with("ipset"))
            .collect();
        assert_eq!(
            ipset,
            [
                "ipset create yolab-lan6 hash:net family inet6 -exist",
                "ipset create yolab-lan6-next hash:net family inet6 -exist",
                "ipset flush yolab-lan6-next",
                "ipset add yolab-lan6-next 2a01:e0a:80a:ba90::/64 -exist",
                "ipset swap yolab-lan6-next yolab-lan6",
                "ipset destroy yolab-lan6-next",
            ]
        );
        assert_eq!(last, Some(set(&["2a01:e0a:80a:ba90::/64"])));
    }

    #[tokio::test]
    async fn an_unchanged_lan_does_not_touch_the_firewall() {
        let host = scripted(WIRED_V4, WIRED_V6);
        let mut last = Some(set(&["192.168.1.0/24"]));

        sync_family(&host, "yolab-lan4", Family::V4, &mut last).await.unwrap();

        assert!(!host.ran("ipset"));
    }

    #[tokio::test]
    async fn moving_to_another_network_replaces_the_old_lan() {
        let host = scripted(WIFI_V4, WIRED_V6);
        let mut last = Some(set(&["192.168.1.0/24"]));

        sync_family(&host, "yolab-lan4", Family::V4, &mut last).await.unwrap();

        assert!(host.ran("ipset add yolab-lan4-next 10.0.0.0/24"));
        assert!(!host.ran("ipset add yolab-lan4-next 192.168.1.0/24"));
        assert!(host.ran("ipset create yolab-lan4 hash:net family inet -exist"));
    }

    #[tokio::test]
    async fn a_failed_swap_is_an_error_and_is_retried_next_time() {
        let host = FakeHost::new()
            .ok("ip -j -4 route show table main", WIRED_V4)
            .ok("ipset", "")
            .fail("ipset swap", "The set with the given name does not exist");
        let mut last = None;

        let err = sync_family(&host, "yolab-lan4", Family::V4, &mut last).await;

        assert!(err.is_err());
        assert_eq!(last, None);
    }

    #[tokio::test]
    async fn an_unreadable_routing_table_stops_the_service_instead_of_emptying_the_set() {
        let host = FakeHost::new()
            .fail("ip -j -4 route show table main", "Cannot talk to rtnetlink")
            .ok("ipset", "");
        let mut last = Some(set(&["192.168.1.0/24"]));

        let err = sync_family(&host, "yolab-lan4", Family::V4, &mut last).await;

        assert!(err.is_err());
        assert!(!host.ran("ipset"));
    }

    #[tokio::test]
    async fn refuses_to_run_without_the_set_names() {
        let host = scripted(WIRED_V4, WIRED_V6);
        assert!(run(&host, "", "yolab-lan6").await.is_err());
        assert!(run(&host, "yolab-lan4", "").await.is_err());
        assert!(host.calls().is_empty());
    }
}
