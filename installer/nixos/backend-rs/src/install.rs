use anyhow::Context;
use serde::Serialize;

use crate::app::AppEvent;
use crate::seam::{Events, Machine};

const GIT_REMOTE: &str = "https://github.com/DemyCode/yolab.git";
const CODE_DIR: &str = "/tmp/yolab-install";
const CLONE_MACHINE_DIR: &str = "homelab/machine";
const MACHINE_DIR: &str = "/var/lib/yolab/machine";
const MACHINE_FILES: [&str; 2] = ["config.toml", "hardware-configuration.nix"];

const HARDWARE_REPORT: &str = "facter.json";

fn machine_files(report_written: bool) -> Vec<&'static str> {
    let mut files = MACHINE_FILES.to_vec();
    if report_written {
        files.push(HARDWARE_REPORT);
    }
    files
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstallParams {
    pub disk: String,
    pub timezone: String,
    pub password: String,
    pub root_ssh_key: String,
    pub account_token: String,
    pub server_addr: Option<String>,
    pub k3s_token: Option<String>,
    pub ceph_fsid: Option<String>,
    pub boot_mode: String,
}

#[derive(Serialize)]
struct ConfigToml {
    homelab: HomelabSection,
    disk: DiskSection,
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel: Option<TunnelSection>,
    node: NodeSection,
    ceph: CephSection,
}

#[derive(Serialize)]
struct HomelabSection {
    hostname: String,
    timezone: String,
    locale: String,
    ssh_port: u16,
    root_ssh_key: String,
    git_remote: String,
    allowed_ssh_keys: Vec<String>,
    homelab_password_hash: String,
    boot_mode: String,
    system: String,
}

#[derive(Serialize)]
struct DiskSection {
    device: String,
    esp_size: String,
}

#[derive(Serialize)]
struct TunnelSection {
    enabled: bool,
    platform_api_url: String,
    account_token: String,
    tunnel_id: String,
    wg_private_key: String,
    wg_public_key: String,
    sub_ipv6: String,
    dns_url: String,
    wg_server_endpoint: String,
    wg_server_public_key: String,
}

#[derive(Serialize)]
struct NodeSection {
    node_id: String,
    wg_private_key: String,
    wg_public_key: String,
    sub_ipv6_private: String,
    sub_ipv6_private_subnet: String,
    wg_server_endpoint: String,
    wg_server_public_key: String,
    k3s: K3sSection,
}

#[derive(Serialize)]
struct K3sSection {
    token: String,
    server_addr: String,
}

#[derive(Serialize)]
struct CephSection {
    fsid: String,
}

fn gen_ceph_fsid() -> String {
    use rand::Rng;
    uuid_v4(rand::thread_rng().gen())
}

fn uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = 0x40 + (b[6] & 0x0f);
    b[8] = 0x80 + (b[8] & 0x3f);
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

fn gen_k3s_token() -> String {
    use rand::Rng;
    let bytes: Vec<u8> = (0..32).map(|_| rand::thread_rng().gen()).collect();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn build_config(
    req: &InstallParams,
    tunnel: &crate::wireguard::TunnelResult,
    service_name: &str,
    password_hash: &str,
) -> ConfigToml {
    ConfigToml {
        homelab: HomelabSection {
            hostname: service_name.to_string(),
            timezone: req.timezone.clone(),
            locale: "en_US.UTF-8".into(),
            ssh_port: 22,
            root_ssh_key: req.root_ssh_key.clone(),
            git_remote: GIT_REMOTE.into(),
            allowed_ssh_keys: vec![],
            homelab_password_hash: password_hash.to_string(),
            boot_mode: req.boot_mode.clone(),
            system: nix_system(std::env::consts::ARCH),
        },
        disk: DiskSection {
            device: req.disk.clone(),
            esp_size: "500M".into(),
        },
        tunnel: Some(TunnelSection {
            enabled: tunnel.enabled,
            platform_api_url: tunnel.platform_api_url.clone(),
            account_token: tunnel.account_token.clone(),
            tunnel_id: tunnel.tunnel_id.clone(),
            wg_private_key: tunnel.wg_private_key.clone(),
            wg_public_key: tunnel.wg_public_key.clone(),
            sub_ipv6: tunnel.sub_ipv6.clone(),
            dns_url: tunnel.dns_url.clone(),
            wg_server_endpoint: tunnel.wg_server_endpoint.clone(),
            wg_server_public_key: tunnel.wg_server_public_key.clone(),
        }),
        node: NodeSection {
            node_id: tunnel.node_id.clone(),
            wg_private_key: tunnel.node_wg_private_key.clone(),
            wg_public_key: tunnel.node_wg_public_key.clone(),
            sub_ipv6_private: tunnel.sub_ipv6_private.clone(),
            sub_ipv6_private_subnet: tunnel.sub_ipv6_private_subnet.clone(),
            wg_server_endpoint: tunnel.node_wg_server_endpoint.clone(),
            wg_server_public_key: tunnel.node_wg_server_public_key.clone(),
            k3s: K3sSection {
                token: req.k3s_token.clone().unwrap_or_else(gen_k3s_token),
                server_addr: req.server_addr.clone().unwrap_or_default(),
            },
        },
        ceph: CephSection {
            fsid: req.ceph_fsid.clone().unwrap_or_else(gen_ceph_fsid),
        },
    }
}

fn nix_system(arch: &str) -> String {
    format!("{arch}-linux")
}

fn render_config_toml(
    req: &InstallParams,
    tunnel: &crate::wireguard::TunnelResult,
    service_name: &str,
    password_hash: &str,
) -> String {
    toml::to_string(&build_config(req, tunnel, service_name, password_hash))
        .expect("the config structs encode as TOML")
}

async fn hash_password<M: Machine>(m: &M, password: &str) -> anyhow::Result<String> {
    let out = m
        .run("openssl", &["passwd", "-6", "-stdin"], Some(password))
        .await?;
    anyhow::ensure!(out.success, "openssl passwd failed");
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

fn announce(program: &str, args: &[&str], tx: &Events) {
    let _ = tx.send(AppEvent::Log(format!("$ {program} {}", args.join(" "))));
}

async fn stream_command<M: Machine>(
    m: &M,
    program: &str,
    args: &[&str],
    tx: &Events,
) -> anyhow::Result<()> {
    announce(program, args, tx);
    m.stream(program, args, tx).await
}

async fn capture_command<M: Machine>(
    m: &M,
    program: &str,
    args: &[&str],
    tx: &Events,
) -> anyhow::Result<Vec<u8>> {
    announce(program, args, tx);
    m.capture(program, args, tx).await
}

fn disko_args(flake_ref: &str) -> [&str; 5] {
    [
        "--yes-wipe-all-disks",
        "--mode",
        "destroy,format,mount",
        "--flake",
        flake_ref,
    ]
}

pub async fn run_install<M: Machine>(m: &M, req: InstallParams, tx: &Events) {
    let event = match do_install(m, &req, tx).await {
        Ok(url) => AppEvent::InstallComplete { url },
        Err(e) => {
            let _ = tx.send(AppEvent::Log(format!("ERROR: {e:#}")));
            AppEvent::Failed(format!("{e:#}"))
        }
    };
    let _ = tx.send(event);
}

fn log(tx: &Events, line: impl Into<String>) {
    let _ = tx.send(AppEvent::Log(line.into()));
}

async fn do_install<M: Machine>(m: &M, req: &InstallParams, tx: &Events) -> anyhow::Result<String> {
    log(tx, "Registering WireGuard tunnel…");
    let service_name = crate::wireguard::next_node_name(m, &req.account_token)
        .await
        .context("next_node_name")?;
    log(tx, format!("This machine will be: {service_name}"));

    let tunnel =
        crate::wireguard::register_and_bring_up_tunnel(m, &req.account_token, &service_name)
            .await
            .context("register tunnel")?;
    log(tx, format!("✓ Tunnel up — {}", tunnel.dns_url));

    if let Err(e) = partition_and_install(m, req, tx, &service_name, &tunnel).await {
        log(tx, "Install failed — deregistering the tunnel and node peer so the name and DNS record aren't stranded…");
        crate::wireguard::deregister(m, &req.account_token, &tunnel, tx).await;
        return Err(e);
    }

    log(
        tx,
        "Copying this machine's configuration to the installed system…",
    );
    install_machine_files(m, tx).await?;
    log(tx, "✓ Complete — remove the USB and reboot");

    Ok(tunnel.dns_url)
}

async fn install_machine_files<M: Machine>(m: &M, tx: &Events) -> anyhow::Result<()> {
    let target = format!("/mnt{MACHINE_DIR}");
    stream_command(m, "install", &["-d", "-m", "0700", &target], tx).await?;
    let report_written = m
        .exists(&format!("{CODE_DIR}/{CLONE_MACHINE_DIR}/{HARDWARE_REPORT}"))
        .await;
    for file in machine_files(report_written) {
        let from = format!("{CODE_DIR}/{CLONE_MACHINE_DIR}/{file}");
        let to = format!("{target}/{file}");
        stream_command(m, "install", &["-m", "0600", &from, &to], tx).await?;
        m.remove_file(&from)
            .await
            .with_context(|| format!("remove {from} from the install clone"))?;
    }
    Ok(())
}

async fn partition_and_install<M: Machine>(
    m: &M,
    req: &InstallParams,
    tx: &Events,
    service_name: &str,
    tunnel: &crate::wireguard::TunnelResult,
) -> anyhow::Result<()> {
    log(tx, "Hashing password…");
    let password_hash = hash_password(m, &req.password)
        .await
        .context("hash_password")?;

    if m.exists(CODE_DIR).await {
        m.remove_dir_all(CODE_DIR).await?;
    }
    log(tx, "Cloning repository…");
    stream_command(m, "git", &["clone", GIT_REMOTE, CODE_DIR], tx).await?;
    log(tx, "✓ Repository cloned");

    log(tx, "Writing config.toml…");
    let machine_dir = format!("{CODE_DIR}/{CLONE_MACHINE_DIR}");
    m.create_dir_all(&machine_dir).await?;
    let toml_str = render_config_toml(req, tunnel, service_name, &password_hash);
    m.write_file(
        &format!("{machine_dir}/config.toml"),
        toml_str.as_bytes(),
        None,
    )
    .await?;
    log(tx, "✓ Config written");

    log(tx, "Generating hardware configuration…");
    let hw_nix = capture_command(
        m,
        "nixos-generate-config",
        &["--no-filesystems", "--show-hardware-config"],
        tx,
    )
    .await
    .context("nixos-generate-config")?;
    m.write_file(
        &format!("{machine_dir}/hardware-configuration.nix"),
        &hw_nix,
        None,
    )
    .await?;
    log(tx, "✓ Hardware config generated");

    log(tx, "Recording this machine's hardware…");
    let report = format!("{machine_dir}/{HARDWARE_REPORT}");
    match stream_command(m, "nixos-facter", &["-o", &report], tx).await {
        Ok(()) => log(tx, "✓ Hardware recorded"),
        Err(e) => {
            if let Err(rm) = m.remove_file(&report).await {
                if rm.kind() != std::io::ErrorKind::NotFound {
                    log(
                        tx,
                        format!("Could not remove the partial hardware report {report}: {rm}"),
                    );
                }
            }
            log(tx, format!("Could not record the hardware ({e:#}); graphics cards stay unused until the machine records it itself"));
        }
    }

    let flake_ref = format!("path:{CODE_DIR}#yolab");

    log(tx, format!("Partitioning {} with disko…", req.disk));
    stream_command(m, "disko", &disko_args(&flake_ref), tx).await?;
    log(tx, "✓ Disk partitioned and mounted");

    log(tx, "Installing NixOS — this takes several minutes…");
    stream_command(m, "nixos-install", &nixos_install_args(&flake_ref), tx).await?;
    log(tx, "✓ NixOS installed");

    Ok(())
}

fn nixos_install_args(flake_ref: &str) -> [&str; 6] {
    [
        "--flake",
        flake_ref,
        "--no-root-password",
        "--log-format",
        "raw",
        "-v",
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::wireguard::TunnelResult;

    pub(crate) fn tunnel() -> TunnelResult {
        TunnelResult {
            enabled: true,
            platform_api_url: "https://api.yolab.io".into(),
            account_token: "acct-tok-123".into(),
            tunnel_id: "42".into(),
            wg_private_key: "tunnel-priv".into(),
            wg_public_key: "tunnel-pub".into(),
            sub_ipv6: "2001:db8::1".into(),
            dns_url: "https://node1.yolab.io".into(),
            wg_server_endpoint: "1.2.3.4:51820".into(),
            wg_server_public_key: "server-pub".into(),
            node_id: "7".into(),
            node_wg_private_key: "node-priv".into(),
            node_wg_public_key: "node-pub".into(),
            sub_ipv6_private: "fd00:42::5".into(),
            sub_ipv6_private_subnet: "fd00:42::/112".into(),
            node_wg_server_endpoint: "1.2.3.4:51821".into(),
            node_wg_server_public_key: "node-server-pub".into(),
        }
    }

    pub(crate) fn params() -> InstallParams {
        InstallParams {
            disk: "/dev/nvme0n1".into(),
            timezone: "Europe/Paris".into(),
            password: "secret".into(),
            root_ssh_key: "ssh-ed25519 AAAA... user@host".into(),
            account_token: "acct-tok-123".into(),
            server_addr: None,
            k3s_token: None,
            ceph_fsid: None,
            boot_mode: "uefi".into(),
        }
    }

    fn rendered(req: &InstallParams) -> toml::Table {
        let text = render_config_toml(req, &tunnel(), "node1", "$6$salt$hash");
        toml::from_str(&text).expect("installer must emit parseable TOML")
    }

    #[test]
    fn the_hardware_report_ships_with_the_machine_only_when_it_was_recorded() {
        assert_eq!(
            machine_files(true),
            vec!["config.toml", "hardware-configuration.nix", "facter.json"]
        );
        assert_eq!(
            machine_files(false),
            vec!["config.toml", "hardware-configuration.nix"]
        );
    }

    #[test]
    fn a_generated_k3s_token_is_256_bits_of_hex() {
        let t = gen_k3s_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generated_k3s_tokens_differ() {
        assert_ne!(gen_k3s_token(), gen_k3s_token());
    }

    #[test]
    fn a_generated_ceph_fsid_is_a_v4_uuid() {
        let f = gen_ceph_fsid();
        let parts: Vec<&str> = f.split('-').collect();
        assert_eq!(parts.len(), 5, "{f}");
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{f}"
        );
        assert!(
            parts
                .iter()
                .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit())),
            "{f}"
        );
        assert!(parts[2].starts_with('4'), "version nibble must be 4: {f}");
        assert!(
            ['8', '9', 'a', 'b'].contains(&parts[3].chars().next().unwrap()),
            "RFC 4122 variant nibble must be 8/9/a/b: {f}"
        );
    }

    #[test]
    fn the_version_and_variant_bits_are_forced_whatever_the_random_bytes() {
        assert_eq!(uuid_v4([0xff; 16]), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(uuid_v4([0x00; 16]), "00000000-0000-4000-8000-000000000000");
    }

    #[test]
    fn generated_ceph_fsids_differ() {
        assert_ne!(gen_ceph_fsid(), gen_ceph_fsid());
    }

    #[test]
    fn config_toml_carries_a_ceph_fsid() {
        let t = rendered(&params());
        let fsid = t["ceph"]["fsid"]
            .as_str()
            .expect("[ceph] fsid must be written");
        assert!(!fsid.is_empty());
    }

    #[test]
    fn the_rendered_config_has_every_section_the_nixos_modules_read() {
        let cfg = rendered(&params());
        for section in ["homelab", "disk", "tunnel", "node"] {
            assert!(cfg.contains_key(section), "missing [{section}]");
        }
        assert!(cfg["node"].as_table().unwrap().contains_key("k3s"));
    }

    #[test]
    fn the_rendered_config_carries_no_section_nothing_reads() {
        let cfg = rendered(&params());
        for dead in ["swarm", "docker", "velero", "wifi"] {
            assert!(
                !cfg.contains_key(dead),
                "[{dead}] is written into every machine's config.toml and no NixOS \
                 module or local-api reader consumes it"
            );
        }
        assert!(
            !cfg["disk"].as_table().unwrap().contains_key("swap_size"),
            "disk.swap_size is dead: there is no swap partition, services.swapspace \
             manages swap on the root filesystem"
        );
    }

    #[test]
    fn the_hostname_is_the_name_assigned_by_the_platform() {
        assert_eq!(
            rendered(&params())["homelab"]["hostname"].as_str(),
            Some("node1")
        );
    }

    #[test]
    fn the_machine_records_the_processor_the_installer_runs_on() {
        assert_eq!(nix_system("x86_64"), "x86_64-linux");
        assert_eq!(nix_system("aarch64"), "aarch64-linux");
        assert_eq!(
            rendered(&params())["homelab"]["system"].as_str(),
            Some(nix_system(std::env::consts::ARCH).as_str())
        );
    }

    #[test]
    fn the_users_choices_reach_the_config_verbatim() {
        let cfg = rendered(&params());
        assert_eq!(cfg["homelab"]["timezone"].as_str(), Some("Europe/Paris"));
        assert_eq!(cfg["homelab"]["boot_mode"].as_str(), Some("uefi"));
        assert_eq!(
            cfg["homelab"]["root_ssh_key"].as_str(),
            Some("ssh-ed25519 AAAA... user@host")
        );
        assert_eq!(cfg["disk"]["device"].as_str(), Some("/dev/nvme0n1"));
    }

    #[test]
    fn only_the_password_hash_is_written_never_the_password() {
        let text = render_config_toml(&params(), &tunnel(), "node1", "$6$salt$hash");
        assert!(text.contains("$6$salt$hash"));
        assert!(
            !text.contains("secret"),
            "the plaintext password must not be written to config.toml"
        );
    }

    #[test]
    fn the_account_token_lands_where_local_api_looks_for_it() {
        let cfg = rendered(&params());
        assert_eq!(
            cfg["tunnel"]["account_token"].as_str(),
            Some("acct-tok-123")
        );
    }

    #[test]
    fn both_wireguard_keypairs_are_recorded_without_being_swapped() {
        let cfg = rendered(&params());
        assert_eq!(
            cfg["tunnel"]["wg_private_key"].as_str(),
            Some("tunnel-priv")
        );
        assert_eq!(cfg["tunnel"]["wg_public_key"].as_str(), Some("tunnel-pub"));
        assert_eq!(cfg["tunnel"]["sub_ipv6"].as_str(), Some("2001:db8::1"));
        assert_eq!(cfg["node"]["wg_private_key"].as_str(), Some("node-priv"));
        assert_eq!(cfg["node"]["wg_public_key"].as_str(), Some("node-pub"));
        assert_eq!(cfg["node"]["sub_ipv6_private"].as_str(), Some("fd00:42::5"));
        assert_eq!(
            cfg["node"]["wg_server_endpoint"].as_str(),
            Some("1.2.3.4:51821")
        );
    }

    #[test]
    fn the_first_node_generates_its_own_k3s_token_and_no_server_address() {
        let cfg = rendered(&params());
        let k3s = &cfg["node"]["k3s"];
        assert_eq!(k3s["server_addr"].as_str(), Some(""));
        assert_eq!(k3s["token"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn a_joining_node_keeps_the_token_it_was_given() {
        let mut req = params();
        req.k3s_token = Some("existing-cluster-token".into());
        req.server_addr = Some("https://[fd00:42::1]:6443".into());

        let cfg = rendered(&req);
        let k3s = &cfg["node"]["k3s"];
        assert_eq!(k3s["token"].as_str(), Some("existing-cluster-token"));
        assert_eq!(
            k3s["server_addr"].as_str(),
            Some("https://[fd00:42::1]:6443")
        );
    }

    #[test]
    fn two_installs_of_the_first_node_never_share_a_k3s_token() {
        let a = rendered(&params());
        let b = rendered(&params());
        assert_ne!(
            a["node"]["k3s"]["token"].as_str(),
            b["node"]["k3s"]["token"].as_str()
        );
    }

    #[test]
    fn defaults_that_the_ui_never_asks_about_are_still_set() {
        let cfg = rendered(&params());
        assert_eq!(cfg["homelab"]["locale"].as_str(), Some("en_US.UTF-8"));
        assert_eq!(cfg["homelab"]["ssh_port"].as_integer(), Some(22));
        assert_eq!(cfg["disk"]["esp_size"].as_str(), Some("500M"));
        assert_eq!(cfg["tunnel"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn disko_partitions_via_the_flake_not_the_bare_module() {
        let args = disko_args("path:/tmp/yolab-install#yolab");
        assert!(args.contains(&"--flake"));
        assert!(args.iter().any(|a| a.ends_with("#yolab")));
        assert!(
            !args.iter().any(|a| a.ends_with(".nix")),
            "passing a module file drops specialArgs and fails on yolabConfigPath"
        );
    }

    #[test]
    fn the_flake_ref_is_path_so_gitignored_config_is_visible() {
        assert!(disko_args("path:/tmp/yolab-install#yolab")[4].starts_with("path:"));
    }

    #[test]
    fn a_bios_install_records_bios_not_the_uefi_default() {
        let mut req = params();
        req.boot_mode = "bios".into();
        assert_eq!(
            rendered(&req)["homelab"]["boot_mode"].as_str(),
            Some("bios")
        );
    }

    #[test]
    fn an_empty_ssh_key_is_still_written_as_a_field() {
        let mut req = params();
        req.root_ssh_key = String::new();
        let cfg = rendered(&req);
        assert_eq!(cfg["homelab"]["root_ssh_key"].as_str(), Some(""));
        assert!(cfg["homelab"]["allowed_ssh_keys"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn values_containing_awkward_characters_survive_the_round_trip() {
        let mut req = params();
        req.root_ssh_key = "ssh-ed25519 AAAA\nsecond line \"quoted\" \\ backslash".into();
        req.timezone = "America/Argentina/Buenos_Aires".into();

        let cfg = rendered(&req);
        assert_eq!(
            cfg["homelab"]["root_ssh_key"].as_str(),
            Some("ssh-ed25519 AAAA\nsecond line \"quoted\" \\ backslash")
        );
        assert_eq!(
            cfg["homelab"]["timezone"].as_str(),
            Some("America/Argentina/Buenos_Aires")
        );
    }

    use crate::fake::{events, logs, FakeMachine};
    use crate::seam::{Method, Output};
    use tokio::sync::mpsc;

    const API: &str = "https://api.yolab.io";
    const CLONE: &str = "/tmp/yolab-install/homelab/machine";

    pub(crate) fn tunnel_for(fqdn: &str, account_token: &str) -> TunnelResult {
        TunnelResult {
            dns_url: format!("https://{fqdn}"),
            account_token: account_token.into(),
            ..tunnel()
        }
    }

    fn ready() -> FakeMachine {
        let m = FakeMachine::default();
        m.reply(
            Method::Get,
            &format!("{API}/tunnels"),
            200,
            r#"[{"dns_records":[{"name":"node2"}]}]"#,
        );
        crate::wireguard::tests::script_tunnel(&m);
        m.output("openssl", true, "$6$salt$hash\n");
        m.capture_bytes("nixos-generate-config", Ok(b"{ hardware }"));
        m.reply(Method::Delete, &format!("{API}/nodes/7"), 204, "");
        m.reply(Method::Delete, &format!("{API}/tunnels/42"), 204, "");
        m
    }

    async fn install(m: &FakeMachine) -> Vec<AppEvent> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_install(m, params(), &tx).await;
        events(&mut rx)
    }

    fn deregistered(m: &FakeMachine) -> bool {
        m.calls().iter().any(|c| c.starts_with("http Delete"))
    }

    fn failure(evs: &[AppEvent]) -> String {
        let msgs: Vec<String> = evs
            .iter()
            .filter_map(|e| match e {
                AppEvent::Failed(msg) => Some(msg.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(msgs.len(), 1, "{evs:?}");
        assert_eq!(evs.last(), Some(&AppEvent::Failed(msgs[0].clone())));
        msgs[0].clone()
    }

    #[tokio::test]
    async fn a_full_install_ends_with_the_machines_public_url() {
        let m = ready();
        let evs = install(&m).await;
        assert_eq!(
            evs.last(),
            Some(&AppEvent::InstallComplete {
                url: "https://node3.alice.yolab.io".into()
            })
        );
        assert!(!deregistered(&m));

        let steps: Vec<String> = m
            .calls()
            .into_iter()
            .filter(|c| {
                c.starts_with("stream") || c.starts_with("capture") || c.starts_with("run openssl")
            })
            .collect();
        assert_eq!(
            steps,
            vec![
                "run openssl passwd -6 -stdin".to_string(),
                format!("stream git clone {GIT_REMOTE} {CODE_DIR}"),
                "capture nixos-generate-config --no-filesystems --show-hardware-config".into(),
                format!("stream nixos-facter -o {CLONE}/facter.json"),
                format!("stream disko {}", disko_args("path:/tmp/yolab-install#yolab").join(" ")),
                "stream nixos-install --flake path:/tmp/yolab-install#yolab --no-root-password --log-format raw -v".into(),
                "stream install -d -m 0700 /mnt/var/lib/yolab/machine".into(),
                format!("stream install -m 0600 {CLONE}/config.toml /mnt/var/lib/yolab/machine/config.toml"),
                format!("stream install -m 0600 {CLONE}/hardware-configuration.nix /mnt/var/lib/yolab/machine/hardware-configuration.nix"),
            ]
        );
        assert_eq!(
            m.state().stdin,
            vec![
                ("wg".into(), "tunnel-priv\n".into()),
                ("wg".into(), "node-priv\n".into()),
                ("openssl".into(), "secret".into())
            ]
        );
    }

    #[tokio::test]
    async fn the_config_names_the_machine_and_holds_only_the_password_hash() {
        let m = ready();
        let (tx, _rx) = mpsc::unbounded_channel();
        partition_and_install(&m, &params(), &tx, "node3", &tunnel())
            .await
            .unwrap();
        let cfg: toml::Table =
            toml::from_str(&m.contents(&format!("{CLONE}/config.toml")).unwrap()).unwrap();
        assert_eq!(cfg["homelab"]["hostname"].as_str(), Some("node3"));
        assert_eq!(
            cfg["homelab"]["homelab_password_hash"].as_str(),
            Some("$6$salt$hash")
        );
        assert_eq!(
            m.contents(&format!("{CLONE}/hardware-configuration.nix"))
                .as_deref(),
            Some("{ hardware }")
        );
    }

    #[tokio::test]
    async fn every_command_is_shown_in_the_log_before_it_runs() {
        let m = ready();
        let lines = logs(&install(&m).await);
        let clone = lines
            .iter()
            .position(|l| *l == format!("$ git clone {GIT_REMOTE} {CODE_DIR}"))
            .unwrap();
        let cloned = lines
            .iter()
            .position(|l| l == "✓ Repository cloned")
            .unwrap();
        assert!(clone < cloned);
        assert!(lines.contains(
            &"$ nixos-generate-config --no-filesystems --show-hardware-config".to_string()
        ));
        assert!(lines.contains(&"This machine will be: node3".to_string()));
        assert!(lines.contains(&"✓ Tunnel up — https://node3.alice.yolab.io".to_string()));
        assert_eq!(
            lines.last().unwrap(),
            "✓ Complete — remove the USB and reboot"
        );
    }

    #[tokio::test]
    async fn the_output_of_a_command_reaches_the_log() {
        let m = ready();
        m.stream_lines("nixos-install", Ok(&["building…", "done"]));
        let lines = logs(&install(&m).await);
        assert!(lines.windows(2).any(|w| w == ["building…", "done"]));
    }

    #[tokio::test]
    async fn a_left_over_clone_is_removed_before_cloning_again() {
        let m = ready();
        m.state().dirs.insert(CODE_DIR.into());
        install(&m).await;
        let calls = m.calls();
        let rm = calls
            .iter()
            .position(|c| *c == format!("rmdir {CODE_DIR}"))
            .unwrap();
        let clone = calls
            .iter()
            .position(|c| c.starts_with("stream git clone"))
            .unwrap();
        assert!(rm < clone);
    }

    #[tokio::test]
    async fn a_recorded_hardware_report_ships_with_the_machine() {
        let m = ready();
        m.file(&format!("{CLONE}/facter.json"), "{}");
        install(&m).await;
        assert!(m.calls().contains(&format!(
            "stream install -m 0600 {CLONE}/facter.json /mnt/var/lib/yolab/machine/facter.json"
        )));
        assert!(m.contents(&format!("{CLONE}/facter.json")).is_none());
        assert!(m.contents(&format!("{CLONE}/config.toml")).is_none());
    }

    #[tokio::test]
    async fn a_failed_hardware_report_is_removed_and_the_install_carries_on() {
        let m = ready();
        m.file(&format!("{CLONE}/facter.json"), "{ partial");
        m.stream_lines("nixos-facter", Err("facter crashed"));
        let evs = install(&m).await;
        assert!(matches!(evs.last(), Some(AppEvent::InstallComplete { .. })));
        assert!(m.contents(&format!("{CLONE}/facter.json")).is_none());
        let lines = logs(&evs);
        assert!(lines.contains(&"Could not record the hardware (facter crashed); graphics cards stay unused until the machine records it itself".to_string()));
        assert!(!lines.iter().any(|l| l.starts_with("Could not remove")));
        assert!(!lines.contains(&"✓ Hardware recorded".to_string()));
    }

    #[tokio::test]
    async fn a_failed_report_that_never_got_written_needs_no_cleanup() {
        let m = ready();
        m.stream_lines("nixos-facter", Err("facter crashed"));
        let lines = logs(&install(&m).await);
        assert!(!lines.iter().any(|l| l.starts_with("Could not remove")));
    }

    #[tokio::test]
    async fn a_partial_report_that_cannot_be_removed_is_logged() {
        let m = ready();
        m.stream_lines("nixos-facter", Err("facter crashed"));
        m.state().remove_errors.insert(
            format!("{CLONE}/facter.json"),
            std::io::ErrorKind::PermissionDenied,
        );
        let lines = logs(&install(&m).await);
        assert!(lines.contains(&format!(
            "Could not remove the partial hardware report {CLONE}/facter.json: cannot remove {CLONE}/facter.json"
        )));
    }

    #[tokio::test]
    async fn no_name_means_no_tunnel_and_nothing_to_clean_up() {
        let m = FakeMachine::default();
        m.reply(Method::Get, &format!("{API}/tunnels"), 500, "");
        let evs = install(&m).await;
        assert_eq!(
            failure(&evs),
            "next_node_name: GET /tunnels returned HTTP 500: "
        );
        assert!(!m.ran("wg"));
        assert_eq!(
            logs(&evs).last().unwrap(),
            "ERROR: next_node_name: GET /tunnels returned HTTP 500: "
        );
    }

    #[tokio::test]
    async fn a_tunnel_that_could_not_be_registered_has_nothing_to_clean_up() {
        let m = FakeMachine::default();
        m.reply(Method::Get, &format!("{API}/tunnels"), 200, "[]");
        m.output("wg", false, "");
        let evs = install(&m).await;
        assert_eq!(failure(&evs), "register tunnel: wg genkey failed");
        assert!(!deregistered(&m));
    }

    #[tokio::test]
    async fn any_failure_after_the_tunnel_is_up_deregisters_it() {
        type Break = fn(&FakeMachine);
        let cases: [(&str, Break); 11] = [
            ("hash_password: no openssl", |m| {
                m.state()
                    .outputs
                    .insert("openssl".into(), [Err("no openssl".into())].into());
            }),
            ("hash_password: openssl passwd failed", |m| {
                m.state().outputs.insert(
                    "openssl".into(),
                    [Ok(Output {
                        success: false,
                        stdout: vec![],
                        stderr: String::new(),
                    })]
                    .into(),
                );
            }),
            (
                "hash_password: invalid utf-8 sequence of 1 bytes from index 0",
                |m| {
                    m.state().outputs.insert(
                        "openssl".into(),
                        [Ok(Output {
                            success: true,
                            stdout: vec![0xff],
                            stderr: String::new(),
                        })]
                        .into(),
                    );
                },
            ),
            ("/tmp/yolab-install is broken", |m| {
                m.state().dirs.insert(CODE_DIR.into());
                m.state().broken_paths.insert(CODE_DIR.into());
            }),
            ("git clone failed", |m| {
                m.stream_lines("git", Err("git clone failed"));
            }),
            ("/tmp/yolab-install/homelab/machine is broken", |m| {
                m.state().broken_paths.insert(CLONE.into());
            }),
            (
                "/tmp/yolab-install/homelab/machine/config.toml is broken",
                |m| {
                    m.state()
                        .broken_paths
                        .insert(format!("{CLONE}/config.toml"));
                },
            ),
            ("nixos-generate-config: no hardware", |m| {
                m.state().captures.insert(
                    "nixos-generate-config".into(),
                    [Err("no hardware".into())].into(),
                );
            }),
            (
                "/tmp/yolab-install/homelab/machine/hardware-configuration.nix is broken",
                |m| {
                    m.state()
                        .broken_paths
                        .insert(format!("{CLONE}/hardware-configuration.nix"));
                },
            ),
            ("disko failed", |m| {
                m.stream_lines("disko", Err("disko failed"));
            }),
            ("nixos-install failed", |m| {
                m.stream_lines("nixos-install", Err("nixos-install failed"));
            }),
        ];
        for (expected, break_it) in cases {
            let m = ready();
            break_it(&m);
            let evs = install(&m).await;
            assert_eq!(failure(&evs), expected);
            assert!(deregistered(&m), "{expected}");
            assert!(logs(&evs).contains(&"Install failed — deregistering the tunnel and node peer so the name and DNS record aren't stranded…".to_string()));
        }
    }

    #[tokio::test]
    async fn a_failure_copying_the_config_keeps_the_installed_tunnel() {
        let m = ready();
        m.stream_lines("install", Err("no /mnt"));
        assert_eq!(failure(&install(&m).await), "no /mnt");
        assert!(!deregistered(&m));

        let m = ready();
        m.stream_lines("install", Ok(&[]))
            .stream_lines("install", Err("disk full"));
        assert_eq!(failure(&install(&m).await), "disk full");

        let m = ready();
        m.state().remove_errors.insert(
            format!("{CLONE}/config.toml"),
            std::io::ErrorKind::PermissionDenied,
        );
        assert_eq!(
            failure(&install(&m).await),
            format!("remove {CLONE}/config.toml from the install clone: cannot remove {CLONE}/config.toml")
        );
    }
}
