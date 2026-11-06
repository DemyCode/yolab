use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;

use super::record::{self, VdiskRecord};
use crate::host::Host;

const IMAGE: &str = "disk.img";
const KEY_BYTES: usize = 64;
const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const STEP_TIMEOUT: Duration = Duration::from_secs(120);
const SECTOR: &str = "4096";

pub(crate) fn run_dir(root: &Path, id: i64) -> PathBuf {
    root.join("run/yolab/vdisks").join(id.to_string())
}

fn mount_point(root: &Path, id: i64) -> PathBuf {
    run_dir(root, id).join("mnt")
}

fn credentials(root: &Path, id: i64) -> PathBuf {
    run_dir(root, id).join("credentials")
}

fn key_file(root: &Path, id: i64) -> PathBuf {
    run_dir(root, id).join("key")
}

fn loop_file(root: &Path, id: i64) -> PathBuf {
    run_dir(root, id).join("loop")
}

fn image(root: &Path, id: i64) -> PathBuf {
    mount_point(root, id).join(IMAGE)
}

fn dev_exists(root: &Path, dev: &str) -> bool {
    root.join(dev.trim_start_matches('/')).exists()
}

pub(crate) fn mount_options(credentials: &Path) -> String {
    [
        format!("credentials={}", credentials.display()),
        "vers=3.1.1".into(),
        "seal".into(),
        "cache=none".into(),
        "soft".into(),
        "echo_interval=10".into(),
        "nosharesock".into(),
        "noperm".into(),
        "uid=0".into(),
        "gid=0".into(),
        "file_mode=0600".into(),
        "dir_mode=0700".into(),
    ]
    .join(",")
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ImagePlan {
    Create,
    Use,
    Refuse(&'static str),
}

pub(crate) fn plan_image(formatted: bool, existing: Option<u64>, want: u64) -> ImagePlan {
    match (formatted, existing) {
        (true, Some(_)) => ImagePlan::Use,
        (true, None) => ImagePlan::Refuse(
            "the virtual disk's file is missing from the Storage Box — it held encrypted data, so \
             it is not recreated empty",
        ),
        (false, Some(size)) if size == want => ImagePlan::Use,
        (false, _) => ImagePlan::Create,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyPlan {
    Use(String),
    Generate,
    Refuse(&'static str),
}

pub(crate) fn plan_key(formatted: bool, stored: Option<String>) -> KeyPlan {
    match (formatted, stored) {
        (_, Some(k)) => KeyPlan::Use(k),
        (false, None) => KeyPlan::Generate,
        (true, None) => KeyPlan::Refuse(
            "the encryption key of this virtual disk is gone from the cluster — its data cannot \
             be read and is left untouched",
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ContentPlan {
    Activate,
    Format,
    Refuse(&'static str),
}

pub(crate) fn plan_content(formatted: bool, fs_type: &str) -> ContentPlan {
    match (formatted, fs_type) {
        (true, "LVM2_member") => ContentPlan::Activate,
        (true, _) => ContentPlan::Refuse(
            "the virtual disk does not decrypt to YoLab's layout — the key does not match or the \
             file on the Storage Box was changed; nothing is written to it",
        ),
        (false, _) => ContentPlan::Format,
    }
}

pub(crate) fn decode_key(stored: &str) -> Result<Vec<u8>> {
    let key = base64::engine::general_purpose::STANDARD
        .decode(stored.trim())
        .context("the stored virtual disk key is not base64")?;
    if key.len() != KEY_BYTES {
        bail!(
            "the stored virtual disk key is {} bytes, expected {KEY_BYTES}",
            key.len()
        );
    }
    Ok(key)
}

fn new_key() -> String {
    let mut key = [0u8; KEY_BYTES];
    rand::fill(&mut key[..]);
    base64::engine::general_purpose::STANDARD.encode(key)
}

pub(crate) fn parse_loop_of(out: &str) -> Option<String> {
    out.lines()
        .next()
        .and_then(|l| l.split_once(':'))
        .map(|(dev, _)| dev.trim().to_string())
        .filter(|d| d.starts_with("/dev/loop"))
}

async fn checked<H: Host>(host: &H, bin: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let out = host
        .run_cmd_bounded(bin, args, timeout)
        .await
        .with_context(|| format!("run {bin}"))?;
    if !out.success {
        bail!("{bin} {}: {}", args.join(" "), out.stderr.trim());
    }
    Ok(out.stdout)
}

pub(crate) async fn is_mounted<H: Host>(host: &H, mnt: &Path) -> Result<bool> {
    let mnt = mnt.to_string_lossy();
    let out = host
        .run_cmd_bounded("mountpoint", &["-q", &mnt], PROBE_TIMEOUT)
        .await
        .context("run mountpoint")?;
    Ok(out.success)
}

async fn image_size<H: Host>(host: &H, image: &Path) -> Result<Option<u64>> {
    let path = image.to_string_lossy();
    let out = host
        .run_cmd_bounded("stat", &["--format=%s", &path], PROBE_TIMEOUT)
        .await
        .context("run stat")?;
    if out.success {
        let size = out
            .stdout
            .trim()
            .parse::<u64>()
            .with_context(|| format!("stat printed {:?} as the size of {path}", out.stdout))?;
        return Ok(Some(size));
    }
    if out.stderr.contains("No such file") {
        return Ok(None);
    }
    bail!(
        "the Storage Box did not answer for {path}: {}",
        out.stderr.trim()
    )
}

pub(crate) async fn reachable<H: Host>(host: &H, root: &Path, id: i64) -> bool {
    image_size(host, &image(root, id)).await.is_ok()
}

pub(crate) fn is_attached(root: &Path, id: i64) -> bool {
    dev_exists(root, &record::osd_dev(id))
}

async fn mount<H: Host>(host: &H, root: &Path, rec: &VdiskRecord) -> Result<()> {
    let mnt = mount_point(root, rec.id);
    std::fs::create_dir_all(&mnt).with_context(|| format!("create {}", mnt.display()))?;
    if is_mounted(host, &mnt).await? {
        return Ok(());
    }
    let creds = credentials(root, rec.id);
    crate::config::write_private_file(
        &creds,
        format!("username={}\npassword={}\n", rec.username, rec.password).as_bytes(),
    )?;
    let share = rec.share();
    let mnt_s = mnt.to_string_lossy().to_string();
    let opts = mount_options(&creds);
    checked(
        host,
        "mount.cifs",
        &[&share, &mnt_s, "-o", &opts],
        MOUNT_TIMEOUT,
    )
    .await
    .context("mount the Storage Box")?;
    Ok(())
}

async fn ensure_image<H: Host>(
    host: &H,
    root: &Path,
    rec: &VdiskRecord,
    formatted: bool,
) -> Result<()> {
    let img = image(root, rec.id);
    let existing = image_size(host, &img).await?;
    match plan_image(formatted, existing, rec.file_bytes()) {
        ImagePlan::Use => Ok(()),
        ImagePlan::Refuse(why) => bail!("{why}"),
        ImagePlan::Create => {
            let size = format!("--size={}", rec.file_bytes());
            let img_s = img.to_string_lossy().to_string();
            checked(host, "truncate", &[&size, &img_s], STEP_TIMEOUT)
                .await
                .context("create the virtual disk's file on the Storage Box")?;
            Ok(())
        }
    }
}

async fn ensure_loop<H: Host>(host: &H, root: &Path, id: i64) -> Result<String> {
    let img_s = image(root, id).to_string_lossy().to_string();
    let listed = checked(host, "losetup", &["--associated", &img_s], STEP_TIMEOUT).await?;
    let dev = match parse_loop_of(&listed) {
        Some(dev) => dev,
        None => {
            let out = checked(
                host,
                "losetup",
                &[
                    "--find",
                    "--show",
                    "--direct-io=on",
                    "--sector-size",
                    SECTOR,
                    &img_s,
                ],
                STEP_TIMEOUT,
            )
            .await
            .context("attach the virtual disk's file as a block device")?;
            out.trim().to_string()
        }
    };
    if !dev.starts_with("/dev/loop") {
        bail!("losetup returned {dev:?}, not a loop device");
    }
    std::fs::write(loop_file(root, id), &dev)?;
    Ok(dev)
}

async fn ensure_key<H: Host>(host: &H, id: i64, formatted: bool) -> Result<Vec<u8>> {
    let stored = record::key(host, id).await?;
    match plan_key(formatted, stored) {
        KeyPlan::Use(k) => decode_key(&k),
        KeyPlan::Refuse(why) => bail!("{why}"),
        KeyPlan::Generate => {
            let k = new_key();
            record::store_key(host, id, &k).await?;
            match record::key(host, id).await? {
                Some(back) if back.trim() == k => decode_key(&k),
                _ => bail!("the new virtual disk key did not read back from the cluster"),
            }
        }
    }
}

async fn open_crypt<H: Host>(
    host: &H,
    root: &Path,
    id: i64,
    loop_dev: &str,
    key: &[u8],
) -> Result<()> {
    if dev_exists(root, &record::crypt_dev(id)) {
        return Ok(());
    }
    let kf = key_file(root, id);
    crate::config::write_private_file(&kf, key)?;
    let kf_s = kf.to_string_lossy().to_string();
    let name = record::crypt_name(id);
    let opened = checked(
        host,
        "cryptsetup",
        &[
            "open",
            "--type",
            "plain",
            "--cipher",
            "aes-xts-plain64",
            "--key-size",
            "512",
            "--key-file",
            &kf_s,
            "--keyfile-size",
            "64",
            "--sector-size",
            SECTOR,
            "--batch-mode",
            loop_dev,
            &name,
        ],
        STEP_TIMEOUT,
    )
    .await;
    crate::config::remove_if_present(&kf)?;
    opened.context("open the encryption layer")?;
    Ok(())
}

async fn content_type<H: Host>(host: &H, dev: &str) -> Result<String> {
    let out = host
        .run_cmd_bounded(
            "blkid",
            &["-p", "-s", "TYPE", "-o", "value", dev],
            STEP_TIMEOUT,
        )
        .await
        .context("run blkid")?;
    blkid_type(out.success, &out.stdout, &out.stderr)
        .with_context(|| format!("blkid could not read {dev}"))
}

pub(crate) fn blkid_type(success: bool, stdout: &str, stderr: &str) -> Result<String> {
    if success {
        return Ok(stdout.trim().to_string());
    }
    if stderr.trim().is_empty() {
        return Ok(String::new());
    }
    bail!("{}", stderr.trim())
}

async fn format<H: Host>(host: &H, root: &Path, id: i64) -> Result<()> {
    let crypt = record::crypt_dev(id);
    let vg = record::vg_name(id);
    if dev_exists(root, &record::osd_dev(id)) {
        checked(
            host,
            "dmsetup",
            &["remove", "--retry", &record::osd_dm_name(id)],
            STEP_TIMEOUT,
        )
        .await?;
    }
    checked(host, "pvcreate", &["-ff", "-y", &crypt], STEP_TIMEOUT).await?;
    checked(host, "vgcreate", &["-y", &vg, &crypt], STEP_TIMEOUT).await?;
    checked(
        host,
        "lvcreate",
        &["-y", "-l", "100%FREE", "-n", "osd", &vg],
        STEP_TIMEOUT,
    )
    .await?;
    record::mark_formatted(host, id).await?;
    Ok(())
}

pub async fn attach<H: Host>(host: &H, root: &Path, rec: &VdiskRecord) -> Result<()> {
    if !rec.is_provisioned() {
        bail!("the Storage Box is not ready yet");
    }
    let id = rec.id;
    let formatted = record::is_formatted(host, id).await?;
    mount(host, root, rec).await?;
    ensure_image(host, root, rec, formatted).await?;
    let loop_dev = ensure_loop(host, root, id).await?;
    let key = ensure_key(host, id, formatted).await?;
    open_crypt(host, root, id, &loop_dev, &key).await?;

    let crypt = record::crypt_dev(id);
    match plan_content(formatted, &content_type(host, &crypt).await?) {
        ContentPlan::Activate => {}
        ContentPlan::Format => format(host, root, id).await?,
        ContentPlan::Refuse(why) => {
            checked(
                host,
                "cryptsetup",
                &["close", &record::crypt_name(id)],
                STEP_TIMEOUT,
            )
            .await
            .context("close the encryption layer that did not decrypt")?;
            bail!("{why}");
        }
    }
    if !dev_exists(root, &record::osd_dev(id)) {
        checked(
            host,
            "vgchange",
            &["-ay", &record::vg_name(id)],
            STEP_TIMEOUT,
        )
        .await?;
    }
    if !dev_exists(root, &record::osd_dev(id)) {
        bail!("{} did not appear after activating it", record::osd_dev(id));
    }
    Ok(())
}

fn recorded_loop(root: &Path, id: i64) -> Result<Option<String>> {
    match std::fs::read_to_string(loop_file(root, id)) {
        Ok(s) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("read the recorded loop device"),
    }
}

fn loop_in_use(root: &Path, dev: &str) -> bool {
    let name = dev.trim_start_matches("/dev/");
    root.join("sys/block").join(name).join("loop").exists()
}

pub async fn detach<H: Host>(host: &H, root: &Path, id: i64) -> Result<()> {
    if let Some(osd) = super::osd_on(root, id) {
        let unit = format!("yolab-ceph-osd@{osd}.service");
        let out = host.systemctl(&["stop", &unit]).await?;
        if !out.success {
            bail!("stop {unit}: {}", out.stderr.trim());
        }
    }
    if dev_exists(root, &record::osd_dev(id)) {
        checked(
            host,
            "dmsetup",
            &["remove", "--retry", &record::osd_dm_name(id)],
            STEP_TIMEOUT,
        )
        .await?;
    }
    if dev_exists(root, &record::crypt_dev(id)) {
        checked(
            host,
            "cryptsetup",
            &["close", &record::crypt_name(id)],
            STEP_TIMEOUT,
        )
        .await?;
    }
    if let Some(dev) = recorded_loop(root, id)? {
        if loop_in_use(root, &dev) {
            checked(host, "losetup", &["--detach", &dev], STEP_TIMEOUT).await?;
        }
        crate::config::remove_if_present(&loop_file(root, id))?;
    }
    let mnt = mount_point(root, id);
    if mnt.exists() && is_mounted(host, &mnt).await? {
        let mnt_s = mnt.to_string_lossy().to_string();
        checked(host, "umount", &["--force", "--lazy", &mnt_s], STEP_TIMEOUT).await?;
    }
    crate::config::remove_if_present(&credentials(root, id))?;
    crate::config::remove_if_present(&key_file(root, id))?;
    Ok(())
}

pub fn attached_here(root: &Path) -> Result<Vec<i64>> {
    let dir = root.join("run/yolab/vdisks");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut ids: Vec<i64> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
        .collect();
    ids.sort();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[test]
    fn a_disk_that_held_data_is_never_recreated_empty() {
        assert!(matches!(plan_image(true, None, 10), ImagePlan::Refuse(_)));
        assert_eq!(plan_image(true, Some(3), 10), ImagePlan::Use);
    }

    #[test]
    fn a_new_disk_gets_its_file_created_at_the_planned_size() {
        assert_eq!(plan_image(false, None, 10), ImagePlan::Create);
        assert_eq!(plan_image(false, Some(3), 10), ImagePlan::Create);
        assert_eq!(plan_image(false, Some(10), 10), ImagePlan::Use);
    }

    #[test]
    fn a_key_is_only_ever_generated_for_a_disk_that_never_held_data() {
        assert_eq!(plan_key(false, None), KeyPlan::Generate);
        assert!(matches!(plan_key(true, None), KeyPlan::Refuse(_)));
        assert_eq!(plan_key(true, Some("k".into())), KeyPlan::Use("k".into()));
        assert_eq!(plan_key(false, Some("k".into())), KeyPlan::Use("k".into()));
    }

    #[test]
    fn a_formatted_disk_that_does_not_decrypt_to_lvm_is_never_reformatted() {
        assert_eq!(plan_content(true, "LVM2_member"), ContentPlan::Activate);
        assert!(matches!(plan_content(true, ""), ContentPlan::Refuse(_)));
        assert!(matches!(plan_content(true, "xfs"), ContentPlan::Refuse(_)));
    }

    #[test]
    fn a_fresh_disk_decrypts_to_noise_and_is_formatted_from_recorded_state_alone() {
        assert_eq!(plan_content(false, ""), ContentPlan::Format);
        assert_eq!(plan_content(false, "LVM2_member"), ContentPlan::Format);
    }

    #[test]
    fn blkid_finding_nothing_is_no_signature_but_a_read_error_is_an_error() {
        assert_eq!(
            blkid_type(true, "LVM2_member\n", "").unwrap(),
            "LVM2_member"
        );
        assert_eq!(blkid_type(false, "", "").unwrap(), "");
        assert!(blkid_type(false, "", "blkid: error: /dev/x: Input/output error").is_err());
    }

    #[test]
    fn a_generated_key_is_512_bits_and_decodes() {
        let k = new_key();
        assert_eq!(decode_key(&k).unwrap().len(), 64);
        assert_ne!(new_key(), k);
        assert!(decode_key("c2hvcnQ=").is_err());
    }

    #[test]
    fn the_box_is_mounted_encrypted_uncached_and_failing_rather_than_hanging() {
        let o = mount_options(Path::new("/run/yolab/vdisks/1/credentials"));
        for want in ["seal", "cache=none", "soft", "vers=3.1.1", "nosharesock"] {
            assert!(o.split(',').any(|x| x == want), "{want} missing from {o}");
        }
        assert!(!o.contains("password"));
        assert!(!o.split(',').any(|x| x == "hard"));
    }

    #[test]
    fn the_loop_device_is_read_from_losetups_listing() {
        assert_eq!(
            parse_loop_of("/dev/loop3: []: (/run/yolab/vdisks/1/mnt/disk.img)\n"),
            Some("/dev/loop3".into())
        );
        assert_eq!(parse_loop_of(""), None);
    }

    fn record() -> VdiskRecord {
        VdiskRecord {
            id: 7,
            node: "node1".into(),
            box_type: "bx11".into(),
            size_gb: 900,
            host: "u7.your-storagebox.de".into(),
            username: "u7".into(),
            password: "secret".into(),
            osd_id: None,
        }
    }

    #[tokio::test]
    async fn the_key_never_reaches_a_command_line_and_its_file_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let key = vec![7u8; 64];
        std::fs::create_dir_all(run_dir(root.path(), 7)).unwrap();
        let host = FakeHost::new().ok("cryptsetup open", "");
        open_crypt(&host, root.path(), 7, "/dev/loop3", &key)
            .await
            .unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&key);
        assert!(host.calls().iter().all(|c| !c.contains(&encoded)));
        assert!(host.ran("--type plain --cipher aes-xts-plain64 --key-size 512"));
        assert!(!host.ran("allow-discards"));
        assert!(!key_file(root.path(), 7).exists());
    }

    #[tokio::test]
    async fn the_key_file_is_removed_even_when_cryptsetup_fails() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(run_dir(root.path(), 7)).unwrap();
        let host = FakeHost::new().fail("cryptsetup open", "boom");
        assert!(open_crypt(&host, root.path(), 7, "/dev/loop3", &[1u8; 64])
            .await
            .is_err());
        assert!(!key_file(root.path(), 7).exists());
    }

    #[tokio::test]
    async fn a_box_that_answers_file_not_found_is_reachable_and_one_that_errors_is_not() {
        let root = tempfile::tempdir().unwrap();
        let missing = FakeHost::new().fail("stat", "stat: cannot statx: No such file or directory");
        assert!(reachable(&missing, root.path(), 7).await);
        let down = FakeHost::new().fail("stat", "stat: cannot statx: Host is down");
        assert!(!reachable(&down, root.path(), 7).await);
    }

    #[tokio::test]
    async fn a_formatted_disk_whose_file_vanished_is_refused_before_anything_is_written() {
        let root = tempfile::tempdir().unwrap();
        let host = FakeHost::new()
            .ok("ceph config-key get yolab/vdisk-formatted/7", "1")
            .ok("mountpoint", "")
            .fail("stat", "stat: cannot statx: No such file or directory");
        let err = attach(&host, root.path(), &record()).await.unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
        assert!(!host.ran("truncate"));
        assert!(!host.ran("pvcreate"));
        assert!(!host.ran("config-key set yolab/vdisk-keys"));
    }

    #[tokio::test]
    async fn the_password_is_written_to_a_private_file_not_the_mount_command() {
        let root = tempfile::tempdir().unwrap();
        let host = FakeHost::new().fail("mountpoint", "").ok("mount.cifs", "");
        mount(&host, root.path(), &record()).await.unwrap();
        assert!(host.calls().iter().all(|c| !c.contains("secret")));
        let creds = std::fs::read_to_string(credentials(root.path(), 7)).unwrap();
        assert!(creds.contains("password=secret"));
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(credentials(root.path(), 7))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
