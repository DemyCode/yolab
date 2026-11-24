use crate::app::{AppEvent, DiskInfo};
use crate::install::{self, InstallParams};
use crate::seam::{Events, Machine, Method, Request};
use crate::wireguard::PLATFORM_API;

pub const NETWORK_PROBE: &str = "api.yolab.io:443";

#[derive(Debug, PartialEq)]
pub enum Effect {
    CreateAccount { referral: String },
    VerifyToken { token: String },
    FetchJoinInfo { url: String, password: String },
    DetectDisks,
    GenSshKey,
    Install(InstallParams),
    Power(&'static str),
}

pub async fn wait_for_network<M: Machine>(m: &M, tx: &Events) {
    while !m.resolves(NETWORK_PROBE).await {
        m.pause().await;
    }
    let _ = tx.send(AppEvent::NetworkReady);
}

pub async fn perform<M: Machine>(m: &M, effect: Effect, tx: &Events) {
    let outcome = match effect {
        Effect::CreateAccount { referral } => do_create_account(m, &referral)
            .await
            .map(|t| vec![AppEvent::AccountCreated(t)]),
        Effect::VerifyToken { token } => do_verify_token(m, &token)
            .await
            .map(|()| vec![AppEvent::AccountVerified]),
        Effect::FetchJoinInfo { url, password } => do_fetch_join_info(m, &url, &password)
            .await
            .map(|(server_addr, k3s_token, account_token, ceph_fsid)| {
                account_token
                    .map(AppEvent::AccountCreated)
                    .into_iter()
                    .chain([AppEvent::JoinConnected {
                        server_addr,
                        k3s_token,
                        ceph_fsid,
                    }])
                    .collect()
            }),
        Effect::DetectDisks => detect_disks(m)
            .await
            .map(|d| vec![AppEvent::DisksLoaded(d)]),
        Effect::GenSshKey => do_gen_ssh_key(m).await.map(|(private_key, public_key)| {
            vec![AppEvent::SshKeyGenerated {
                private_key,
                public_key,
            }]
        }),
        Effect::Install(params) => {
            install::run_install(m, params, tx).await;
            Ok(vec![])
        }
        Effect::Power(action) => m
            .spawn_detached("systemctl", &[action])
            .map(|()| vec![])
            .map_err(|e| anyhow::anyhow!("could not {action}: {e}")),
    };
    let events = outcome.unwrap_or_else(|e| vec![AppEvent::Failed(e.to_string())]);
    for ev in events {
        let _ = tx.send(ev);
    }
}

async fn do_create_account<M: Machine>(m: &M, referral: &str) -> anyhow::Result<String> {
    let reply = m
        .http(
            Request::new(Method::Post, format!("{PLATFORM_API}/users"))
                .json(create_account_body(referral)),
        )
        .await?;
    let body = serde_json::from_str(&reply.text).unwrap_or(serde_json::Value::Null);
    account_token_from(reply.ok(), &body)
}

fn create_account_body(referral: &str) -> serde_json::Value {
    let code = referral.trim();
    if code.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({ "referral_code": code })
    }
}

fn account_token_from(ok: bool, body: &serde_json::Value) -> anyhow::Result<String> {
    if !ok {
        anyhow::bail!(
            "{}",
            body["detail"]
                .as_str()
                .unwrap_or("The account could not be created")
        );
    }
    body["account_token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing account_token in response"))
}

async fn do_verify_token<M: Machine>(m: &M, token: &str) -> anyhow::Result<()> {
    let reply = m
        .http(Request::new(Method::Get, format!("{PLATFORM_API}/tunnels")).bearer(token))
        .await?;
    anyhow::ensure!(reply.ok(), "invalid account token");
    Ok(())
}

async fn do_fetch_join_info<M: Machine>(
    m: &M,
    url: &str,
    password: &str,
) -> anyhow::Result<(String, String, Option<String>, String)> {
    let url = url.trim_end_matches('/');

    let login = m
        .http(
            Request::new(Method::Post, format!("{url}/api/login"))
                .json(serde_json::json!({ "password": password })),
        )
        .await?;
    anyhow::ensure!(login.ok(), "authentication failed");

    let cookie = login
        .set_cookie
        .as_deref()
        .and_then(|s| s.split(';').next())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no session cookie"))?;

    let info = m
        .http(Request::new(Method::Get, format!("{url}/api/cluster/join-info")).cookie(cookie))
        .await?;
    let info: serde_json::Value = serde_json::from_str(&info.text)?;

    parse_join_response(&info)
}

fn parse_join_response(
    info: &serde_json::Value,
) -> anyhow::Result<(String, String, Option<String>, String)> {
    if let Some(e) = info.get("error").and_then(|v| v.as_str()) {
        anyhow::bail!("the existing node could not describe its cluster: {e}");
    }

    let field = |k: &str| -> anyhow::Result<String> {
        info.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("the existing node sent no {k}"))
    };

    let server_addr = field("server_addr")?;
    let k3s_token = field("k3s_token")?;
    let account_token = info["account_token"].as_str().map(|s| s.to_string());
    let ceph_fsid = field("ceph_fsid").map_err(|_| {
        anyhow::anyhow!(
            "the existing node reported no Ceph cluster id — update it first, \
             then run this installer again"
        )
    })?;

    Ok((server_addr, k3s_token, account_token, ceph_fsid))
}

pub const LSBLK_ARGS: [&str; 4] = ["-J", "-b", "-o", "NAME,SIZE,TRAN,TYPE,MOUNTPOINT"];

async fn detect_disks<M: Machine>(m: &M) -> anyhow::Result<Vec<DiskInfo>> {
    let out = m.run("lsblk", &LSBLK_ARGS, None).await?;
    anyhow::ensure!(out.success, "lsblk failed: {}", out.stderr.trim());
    let data: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    Ok(disks_from_lsblk(&data))
}

fn disks_from_lsblk(data: &serde_json::Value) -> Vec<DiskInfo> {
    let empty = vec![];
    let devices = data["blockdevices"].as_array().unwrap_or(&empty);

    let mut disks: Vec<DiskInfo> = devices
        .iter()
        .filter(|d| d["type"].as_str() == Some("disk"))
        .filter_map(|d| {
            let raw = d["name"].as_str()?;
            let tran = d["tran"].as_str().unwrap_or("").to_string();
            Some(DiskInfo {
                name: format!("/dev/{raw}"),
                size: fmt_bytes(d["size"].as_u64().unwrap_or(0)),
                is_usb: tran == "usb",
                tran,
                mounted: disk_has_mount(d),
                recommended: false,
            })
        })
        .collect();

    let rec = disks
        .iter()
        .enumerate()
        .filter(|(_, d)| !d.is_usb && !d.mounted)
        .max_by_key(|(_, d)| parse_gb(&d.size))
        .map(|(i, _)| i);
    if let Some(i) = rec {
        disks[i].recommended = true;
    }

    disks
}

fn disk_has_mount(d: &serde_json::Value) -> bool {
    if d["mountpoint"].as_str().is_some_and(|s| !s.is_empty()) {
        return true;
    }
    d["children"]
        .as_array()
        .is_some_and(|ch| ch.iter().any(disk_has_mount))
}

fn fmt_bytes(b: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const TB: u64 = 1_000 * GB;
    if b >= TB {
        format!("{:.1} TB", b as f64 / TB as f64)
    } else {
        format!("{:.0} GB", b as f64 / GB as f64)
    }
}

fn parse_gb(s: &str) -> u64 {
    let n: f64 = s
        .split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    if s.contains("TB") {
        (n * 1000.0) as u64
    } else {
        n as u64
    }
}

async fn do_gen_ssh_key<M: Machine>(m: &M) -> anyhow::Result<(String, String)> {
    let scratch = m.scratch_dir()?;
    let key = format!("{}/installer_key", scratch.path);
    let path = key.as_str();

    let out = m
        .run(
            "ssh-keygen",
            &["-t", "ed25519", "-f", path, "-N", "", "-q"],
            None,
        )
        .await?;
    anyhow::ensure!(out.success, "ssh-keygen failed: {}", out.stderr.trim());

    let private_key = m.read_file(path).await?;
    let public_key = m.read_file(&format!("{path}.pub")).await?;
    Ok((
        private_key.trim().to_string(),
        public_key.trim().to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{events, FakeMachine};
    use crate::seam::Reply;
    use serde_json::json;
    use tokio::sync::mpsc;

    #[test]
    fn sizes_are_formatted_in_the_units_printed_on_the_box() {
        assert_eq!(fmt_bytes(500_000_000_000), "500 GB");
        assert_eq!(fmt_bytes(256_000_000_000), "256 GB");
        assert_eq!(fmt_bytes(1_000_000_000_000), "1.0 TB");
        assert_eq!(fmt_bytes(2_000_000_000_000), "2.0 TB");
    }

    #[test]
    fn terabyte_sizes_keep_one_decimal() {
        assert_eq!(fmt_bytes(1_500_000_000_000), "1.5 TB");
    }

    #[test]
    fn small_and_zero_sizes_do_not_panic() {
        assert_eq!(fmt_bytes(0), "0 GB");
        assert_eq!(fmt_bytes(1), "0 GB");
    }

    #[test]
    fn parse_gb_round_trips_what_fmt_bytes_produces() {
        assert_eq!(parse_gb(&fmt_bytes(500_000_000_000)), 500);
        assert_eq!(parse_gb(&fmt_bytes(2_000_000_000_000)), 2000);
        assert_eq!(parse_gb(&fmt_bytes(1_500_000_000_000)), 1500);
    }

    #[test]
    fn parse_gb_orders_terabyte_disks_above_gigabyte_disks() {
        assert!(parse_gb("1.0 TB") > parse_gb("999 GB"));
    }

    #[test]
    fn parse_gb_reads_unparseable_sizes_as_zero() {
        assert_eq!(parse_gb(""), 0);
        assert_eq!(parse_gb("unknown"), 0);
        assert_eq!(parse_gb("GB"), 0);
    }

    #[test]
    fn a_disk_mounted_at_its_top_level_counts_as_mounted() {
        assert!(disk_has_mount(&json!({"mountpoint": "/"})));
    }

    #[test]
    fn a_disk_is_mounted_when_any_partition_below_it_is() {
        let disk = json!({
            "name": "sda", "mountpoint": null,
            "children": [
                {"name": "sda1", "mountpoint": null},
                {"name": "sda2", "mountpoint": "/iso"},
            ],
        });
        assert!(disk_has_mount(&disk));
    }

    #[test]
    fn nesting_deeper_than_one_level_is_still_detected() {
        let disk = json!({
            "children": [{"children": [{"mountpoint": "/nix/store"}]}],
        });
        assert!(disk_has_mount(&disk));
    }

    #[test]
    fn an_unmounted_disk_is_not_reported_as_mounted() {
        assert!(!disk_has_mount(&json!({"mountpoint": null})));
        assert!(!disk_has_mount(&json!({"mountpoint": ""})));
        assert!(!disk_has_mount(&json!({})));
        assert!(!disk_has_mount(&json!({
            "children": [{"mountpoint": null}, {"mountpoint": ""}]
        })));
    }

    fn lsblk(devices: serde_json::Value) -> serde_json::Value {
        json!({"blockdevices": devices})
    }

    fn disk(name: &str, size: u64, tran: &str, mountpoint: Option<&str>) -> serde_json::Value {
        json!({
            "name": name, "size": size, "tran": tran, "type": "disk",
            "mountpoint": mountpoint,
        })
    }

    #[test]
    fn disks_are_listed_with_full_device_paths() {
        let disks = disks_from_lsblk(&lsblk(json!([disk(
            "nvme0n1",
            512_000_000_000,
            "nvme",
            None
        )])));
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "/dev/nvme0n1");
        assert_eq!(disks[0].size, "512 GB");
        assert_eq!(disks[0].tran, "nvme");
    }

    #[test]
    fn partitions_and_loop_devices_are_not_offered_as_install_targets() {
        let data = lsblk(json!([
            disk("sda", 500_000_000_000, "sata", None),
            {"name": "sda1", "size": 1_000_000, "type": "part"},
            {"name": "loop0", "size": 1_000_000, "type": "loop"},
            {"name": "sr0", "size": 1_000_000, "type": "rom"},
        ]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "/dev/sda");
    }

    #[test]
    fn the_largest_internal_disk_is_recommended() {
        let data = lsblk(json!([
            disk("sda", 250_000_000_000, "sata", None),
            disk("nvme0n1", 1_000_000_000_000, "nvme", None),
            disk("sdb", 500_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        let rec: Vec<&str> = disks
            .iter()
            .filter(|d| d.recommended)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(rec, vec!["/dev/nvme0n1"]);
    }

    #[test]
    fn the_usb_stick_is_never_recommended_even_when_it_is_the_largest() {
        let data = lsblk(json!([
            disk("sdb", 2_000_000_000_000, "usb", None),
            disk("sda", 250_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        let rec: Vec<&str> = disks
            .iter()
            .filter(|d| d.recommended)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(rec, vec!["/dev/sda"]);
        assert_eq!(disks.len(), 2);
        assert!(disks.iter().any(|d| d.name == "/dev/sdb" && d.is_usb));
    }

    #[test]
    fn a_mounted_disk_is_never_recommended() {
        let data = lsblk(json!([
            json!({
                "name": "sdb", "size": 2_000_000_000_000u64, "tran": "sata", "type": "disk",
                "children": [{"name": "sdb1", "mountpoint": "/iso"}],
            }),
            disk("sda", 250_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        let rec: Vec<&str> = disks
            .iter()
            .filter(|d| d.recommended)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(rec, vec!["/dev/sda"]);
    }

    #[test]
    fn nothing_is_recommended_when_every_disk_is_usb_or_mounted() {
        let data = lsblk(json!([
            disk("sdb", 2_000_000_000_000, "usb", None),
            disk("sdc", 1_000_000_000_000, "usb", Some("/iso")),
        ]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.len(), 2);
        assert!(!disks.iter().any(|d| d.recommended));
    }

    #[test]
    fn exactly_one_disk_is_ever_recommended() {
        let data = lsblk(json!([
            disk("sda", 500_000_000_000, "sata", None),
            disk("sdb", 500_000_000_000, "sata", None),
            disk("sdc", 500_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.iter().filter(|d| d.recommended).count(), 1);
    }

    #[test]
    fn a_machine_with_no_disks_yields_an_empty_list() {
        assert!(disks_from_lsblk(&lsblk(json!([]))).is_empty());
        assert!(disks_from_lsblk(&json!({})).is_empty());
        assert!(disks_from_lsblk(&json!({"blockdevices": "unexpected"})).is_empty());
    }

    #[test]
    fn a_disk_with_no_name_is_skipped_rather_than_listed_as_dev() {
        let data = lsblk(json!([
            {"size": 500_000_000_000u64, "type": "disk"},
            disk("sda", 250_000_000_000, "sata", None),
        ]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].name, "/dev/sda");
    }

    #[test]
    fn a_disk_with_no_reported_size_still_appears() {
        let data = lsblk(json!([{"name": "vda", "type": "disk"}]));
        let disks = disks_from_lsblk(&data);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].size, "0 GB");
        assert_eq!(disks[0].tran, "");
    }

    fn join_reply() -> serde_json::Value {
        serde_json::json!({
            "server_addr": "https://[fd00:cafe::1]:6443",
            "k3s_token": "K10abc::server:secret",
            "account_token": "tok-1",
            "ceph_fsid": "11111111-2222-4333-8444-555555555555",
        })
    }

    #[test]
    fn a_complete_reply_parses() {
        let (addr, tok, acct, fsid) = super::parse_join_response(&join_reply()).unwrap();
        assert_eq!(addr, "https://[fd00:cafe::1]:6443");
        assert_eq!(tok, "K10abc::server:secret");
        assert_eq!(acct.as_deref(), Some("tok-1"));
        assert_eq!(fsid, "11111111-2222-4333-8444-555555555555");
    }

    #[test]
    fn an_error_body_is_a_failure_not_a_connection() {
        let v = serde_json::json!({"error": "missing node.k3s.token"});
        let e = super::parse_join_response(&v).unwrap_err().to_string();
        assert!(e.contains("missing node.k3s.token"), "{e}");
    }

    #[test]
    fn an_empty_server_address_is_refused_rather_than_meaning_first_node() {
        let mut v = join_reply();
        v["server_addr"] = serde_json::json!("");
        assert!(super::parse_join_response(&v).is_err());
        v["server_addr"] = serde_json::Value::Null;
        assert!(super::parse_join_response(&v).is_err());
    }

    #[test]
    fn a_missing_k3s_token_is_refused() {
        let mut v = join_reply();
        v.as_object_mut().unwrap().remove("k3s_token");
        assert!(super::parse_join_response(&v).is_err());
    }

    #[test]
    fn an_empty_ceph_fsid_is_refused_and_says_what_to_do() {
        let mut v = join_reply();
        v["ceph_fsid"] = serde_json::json!("");
        let e = super::parse_join_response(&v).unwrap_err().to_string();
        assert!(e.contains("update it first"), "{e}");
    }

    #[test]
    fn an_absent_ceph_fsid_is_refused_too() {
        let mut v = join_reply();
        v.as_object_mut().unwrap().remove("ceph_fsid");
        assert!(super::parse_join_response(&v).is_err());
    }

    #[test]
    fn an_absent_account_token_is_allowed() {
        let mut v = join_reply();
        v.as_object_mut().unwrap().remove("account_token");
        assert_eq!(super::parse_join_response(&v).unwrap().2, None);
    }

    #[test]
    fn without_a_referral_code_the_account_request_body_is_empty() {
        assert_eq!(create_account_body(""), serde_json::json!({}));
        assert_eq!(create_account_body("   "), serde_json::json!({}));
    }

    #[test]
    fn a_typed_referral_code_is_sent_trimmed() {
        assert_eq!(
            create_account_body("  AB3K9XYZ "),
            serde_json::json!({ "referral_code": "AB3K9XYZ" })
        );
    }

    #[test]
    fn a_refused_referral_code_shows_the_platforms_reason() {
        let body = serde_json::json!({ "detail": "This referral code does not exist" });
        let err = account_token_from(false, &body).unwrap_err();
        assert_eq!(err.to_string(), "This referral code does not exist");
    }

    #[test]
    fn a_failure_without_a_reason_still_says_what_failed() {
        let err = account_token_from(false, &serde_json::Value::Null).unwrap_err();
        assert_eq!(err.to_string(), "The account could not be created");
    }

    #[test]
    fn a_created_account_without_a_token_is_an_error() {
        let err = account_token_from(true, &serde_json::json!({})).unwrap_err();
        assert_eq!(err.to_string(), "missing account_token in response");
    }

    #[test]
    fn a_created_account_yields_its_token() {
        let body = serde_json::json!({ "account_token": "tok-1" });
        assert_eq!(account_token_from(true, &body).unwrap(), "tok-1");
    }

    async fn run(m: &FakeMachine, effect: Effect) -> Vec<AppEvent> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        perform(m, effect, &tx).await;
        events(&mut rx)
    }

    fn failed(msg: &str) -> Vec<AppEvent> {
        vec![AppEvent::Failed(msg.to_string())]
    }

    const USERS: &str = "https://api.yolab.io/users";
    const TUNNELS: &str = "https://api.yolab.io/tunnels";

    #[tokio::test]
    async fn the_installer_waits_for_dns_before_saying_the_network_is_ready() {
        let m = FakeMachine::default();
        m.state().resolves.extend([false, false, true]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        wait_for_network(&m, &tx).await;
        assert_eq!(events(&mut rx), vec![AppEvent::NetworkReady]);
        assert_eq!(m.state().pauses, 2);
        assert_eq!(m.calls(), vec!["resolve api.yolab.io:443"; 3]);
    }

    #[tokio::test]
    async fn creating_an_account_sends_the_referral_code_and_yields_the_token() {
        let m = FakeMachine::default();
        m.reply(Method::Post, USERS, 201, r#"{"account_token":"tok-9"}"#);
        let evs = run(
            &m,
            Effect::CreateAccount {
                referral: " AB3 ".into(),
            },
        )
        .await;
        assert_eq!(evs, vec![AppEvent::AccountCreated("tok-9".into())]);
        assert_eq!(
            m.state().requests[0].json,
            Some(json!({"referral_code": "AB3"}))
        );
    }

    #[tokio::test]
    async fn a_refused_account_shows_the_platforms_reason() {
        let m = FakeMachine::default();
        m.reply(
            Method::Post,
            USERS,
            400,
            r#"{"detail":"Unknown referral code"}"#,
        );
        let evs = run(
            &m,
            Effect::CreateAccount {
                referral: String::new(),
            },
        )
        .await;
        assert_eq!(evs, failed("Unknown referral code"));
    }

    #[tokio::test]
    async fn a_refusal_that_is_not_json_still_says_what_failed() {
        let m = FakeMachine::default();
        m.reply(Method::Post, USERS, 502, "<html>Bad gateway</html>");
        let evs = run(
            &m,
            Effect::CreateAccount {
                referral: String::new(),
            },
        )
        .await;
        assert_eq!(evs, failed("The account could not be created"));
    }

    #[tokio::test]
    async fn an_unreachable_platform_is_a_failure_not_a_hang() {
        let m = FakeMachine::default();
        m.reply_with(Method::Post, USERS, Err("connection refused".into()));
        let evs = run(
            &m,
            Effect::CreateAccount {
                referral: String::new(),
            },
        )
        .await;
        assert_eq!(evs, failed("connection refused"));
    }

    #[tokio::test]
    async fn a_token_is_verified_by_listing_tunnels_with_it() {
        let m = FakeMachine::default();
        m.reply(Method::Get, TUNNELS, 200, "[]");
        let evs = run(
            &m,
            Effect::VerifyToken {
                token: "tok-1".into(),
            },
        )
        .await;
        assert_eq!(evs, vec![AppEvent::AccountVerified]);
        assert_eq!(m.state().requests[0].bearer.as_deref(), Some("tok-1"));
    }

    #[tokio::test]
    async fn a_token_the_platform_refuses_is_invalid() {
        let m = FakeMachine::default();
        m.reply(Method::Get, TUNNELS, 401, "");
        let evs = run(
            &m,
            Effect::VerifyToken {
                token: "bad".into(),
            },
        )
        .await;
        assert_eq!(evs, failed("invalid account token"));
    }

    #[tokio::test]
    async fn a_token_that_cannot_be_checked_is_a_failure() {
        let m = FakeMachine::default();
        m.reply_with(Method::Get, TUNNELS, Err("timed out".into()));
        let evs = run(&m, Effect::VerifyToken { token: "t".into() }).await;
        assert_eq!(evs, failed("timed out"));
    }

    fn login_ok(m: &FakeMachine, cookie: Option<&str>) {
        m.reply_with(
            Method::Post,
            "https://node1.example/api/login",
            Ok(Reply {
                status: 200,
                text: String::new(),
                set_cookie: cookie.map(str::to_string),
            }),
        );
    }

    fn join(password: &str) -> Effect {
        Effect::FetchJoinInfo {
            url: "https://node1.example/".into(),
            password: password.into(),
        }
    }

    const JOIN_INFO: &str = "https://node1.example/api/cluster/join-info";

    #[tokio::test]
    async fn joining_signs_in_with_the_password_then_reads_the_cluster_with_the_session() {
        let m = FakeMachine::default();
        login_ok(&m, Some("session=abc; Path=/; HttpOnly"));
        m.reply(Method::Get, JOIN_INFO, 200, &join_reply().to_string());
        let evs = run(&m, join("hunter2")).await;
        assert_eq!(
            evs,
            vec![
                AppEvent::AccountCreated("tok-1".into()),
                AppEvent::JoinConnected {
                    server_addr: "https://[fd00:cafe::1]:6443".into(),
                    k3s_token: "K10abc::server:secret".into(),
                    ceph_fsid: "11111111-2222-4333-8444-555555555555".into(),
                },
            ]
        );
        let s = m.state();
        assert_eq!(s.requests[0].json, Some(json!({"password": "hunter2"})));
        assert_eq!(s.requests[1].cookie.as_deref(), Some("session=abc"));
    }

    #[tokio::test]
    async fn a_node_that_sends_no_account_token_only_connects() {
        let m = FakeMachine::default();
        login_ok(&m, Some("session=abc"));
        let mut info = join_reply();
        info.as_object_mut().unwrap().remove("account_token");
        m.reply(Method::Get, JOIN_INFO, 200, &info.to_string());
        let evs = run(&m, join("p")).await;
        assert!(
            matches!(evs.as_slice(), [AppEvent::JoinConnected { .. }]),
            "{evs:?}"
        );
    }

    #[tokio::test]
    async fn a_wrong_password_fails_authentication() {
        let m = FakeMachine::default();
        m.reply(Method::Post, "https://node1.example/api/login", 401, "");
        assert_eq!(
            run(&m, join("wrong")).await,
            failed("authentication failed")
        );
    }

    #[tokio::test]
    async fn a_login_without_a_session_cookie_fails() {
        for cookie in [None, Some(""), Some("; Path=/")] {
            let m = FakeMachine::default();
            login_ok(&m, cookie);
            assert_eq!(
                run(&m, join("p")).await,
                failed("no session cookie"),
                "{cookie:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_join_info_reply_that_is_not_json_fails() {
        let m = FakeMachine::default();
        login_ok(&m, Some("s=1"));
        m.reply(Method::Get, JOIN_INFO, 200, "not json");
        let evs = run(&m, join("p")).await;
        assert!(matches!(evs.as_slice(), [AppEvent::Failed(_)]), "{evs:?}");
    }

    #[tokio::test]
    async fn an_unreachable_node_fails_at_login_or_at_join_info() {
        let m = FakeMachine::default();
        m.reply_with(
            Method::Post,
            "https://node1.example/api/login",
            Err("refused".into()),
        );
        assert_eq!(run(&m, join("p")).await, failed("refused"));

        let m = FakeMachine::default();
        login_ok(&m, Some("s=1"));
        m.reply_with(Method::Get, JOIN_INFO, Err("reset".into()));
        assert_eq!(run(&m, join("p")).await, failed("reset"));
    }

    #[tokio::test]
    async fn disks_are_read_from_lsblk() {
        let m = FakeMachine::default();
        let data = lsblk(json!([disk("sda", 500_000_000_000, "sata", None)]));
        m.output("lsblk", true, &data.to_string());
        let evs = run(&m, Effect::DetectDisks).await;
        assert_eq!(evs, vec![AppEvent::DisksLoaded(disks_from_lsblk(&data))]);
        assert!(disks_from_lsblk(&data)[0].recommended);
        assert_eq!(
            m.calls(),
            vec![format!("run lsblk {}", LSBLK_ARGS.join(" "))]
        );
    }

    #[tokio::test]
    async fn a_failing_lsblk_is_reported_instead_of_showing_no_disks() {
        let m = FakeMachine::default();
        m.output("lsblk", false, "");
        assert_eq!(
            run(&m, Effect::DetectDisks).await,
            failed("lsblk failed: lsblk broke")
        );

        let m = FakeMachine::default();
        m.output("lsblk", true, "garbage");
        let evs = run(&m, Effect::DetectDisks).await;
        assert!(matches!(evs.as_slice(), [AppEvent::Failed(_)]), "{evs:?}");

        let m = FakeMachine::default();
        assert_eq!(
            run(&m, Effect::DetectDisks).await,
            failed("no output scripted for lsblk")
        );
    }

    fn keygen_writes(m: &FakeMachine, files: &[(&str, &str)]) {
        m.state().writes_after_f.insert(
            "ssh-keygen".into(),
            files
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        );
    }

    #[tokio::test]
    async fn a_generated_ssh_key_returns_both_halves_trimmed() {
        let m = FakeMachine::default();
        keygen_writes(&m, &[("", "PRIVATE\n"), (".pub", "ssh-ed25519 AAAA\n")]);
        m.output("ssh-keygen", true, "");
        let evs = run(&m, Effect::GenSshKey).await;
        assert_eq!(
            evs,
            vec![AppEvent::SshKeyGenerated {
                private_key: "PRIVATE".into(),
                public_key: "ssh-ed25519 AAAA".into(),
            }]
        );
        assert_eq!(
            m.calls(),
            vec!["run ssh-keygen -t ed25519 -f /scratch/installer_key -N  -q"]
        );
    }

    #[tokio::test]
    async fn a_failed_ssh_keygen_is_reported() {
        let m = FakeMachine::default();
        m.output("ssh-keygen", false, "");
        assert_eq!(
            run(&m, Effect::GenSshKey).await,
            failed("ssh-keygen failed: ssh-keygen broke")
        );
    }

    #[tokio::test]
    async fn a_key_without_its_public_half_is_reported() {
        let m = FakeMachine::default();
        keygen_writes(&m, &[("", "PRIVATE")]);
        m.output("ssh-keygen", true, "");
        assert_eq!(
            run(&m, Effect::GenSshKey).await,
            failed("read /scratch/installer_key.pub: no such file")
        );
    }

    #[tokio::test]
    async fn a_key_without_its_private_half_is_reported() {
        let m = FakeMachine::default();
        m.output("ssh-keygen", true, "");
        assert_eq!(
            run(&m, Effect::GenSshKey).await,
            failed("read /scratch/installer_key: no such file")
        );
    }

    #[tokio::test]
    async fn no_temporary_directory_means_no_key() {
        let m = FakeMachine::default();
        m.state().no_scratch = true;
        assert_eq!(
            run(&m, Effect::GenSshKey).await,
            failed("no temporary directory")
        );

        let m = FakeMachine::default();
        m.output_with("ssh-keygen", Err("not installed".into()));
        assert_eq!(run(&m, Effect::GenSshKey).await, failed("not installed"));
    }

    #[tokio::test]
    async fn reboot_and_power_off_go_through_systemctl() {
        let m = FakeMachine::default();
        assert!(run(&m, Effect::Power("reboot")).await.is_empty());
        assert!(run(&m, Effect::Power("poweroff")).await.is_empty());
        assert_eq!(
            m.calls(),
            vec!["spawn systemctl reboot", "spawn systemctl poweroff"]
        );
    }

    #[tokio::test]
    async fn a_power_action_that_cannot_start_says_which_one() {
        let m = FakeMachine::default();
        m.state()
            .detached
            .insert("systemctl".into(), "no such file".into());
        assert_eq!(
            run(&m, Effect::Power("reboot")).await,
            failed("could not reboot: no such file")
        );
    }

    #[tokio::test]
    async fn the_install_effect_runs_the_install() {
        let m = FakeMachine::default();
        let evs = run(&m, Effect::Install(crate::install::tests::params())).await;
        assert!(matches!(evs.last(), Some(AppEvent::Failed(_))), "{evs:?}");
        assert!(m.calls()[0].starts_with("http Get https://api.yolab.io/tunnels"));
    }
}
