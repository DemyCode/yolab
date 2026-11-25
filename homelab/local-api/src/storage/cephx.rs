use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::host::Host;

pub const AES256K: &str = "aes256k";
pub const IMAGES_ID: &str = "yolab-images";
const IMAGES_ENTITY: &str = "client.yolab-images";

pub fn keyring_key(text: &str, entity: &str) -> Option<String> {
    let header = format!("[{entity}]");
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == header;
            continue;
        }
        if inside {
            if let Some(value) = t
                .strip_prefix("key")
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix('='))
            {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

async fn write_keyring<H: Host>(host: &H, path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new("")))?;
    let staged = path.with_extension("yolab-new");
    std::fs::write(&staged, text).with_context(|| format!("write {}", staged.display()))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
    let staged_s = staged.to_string_lossy().into_owned();
    let owned = host
        .run_cmd("chown", &["ceph:ceph", staged_s.as_str()])
        .await?;
    if !owned.success {
        return Err(anyhow!(
            "chown ceph:ceph {staged_s}: {}",
            owned.stderr.trim()
        ));
    }
    std::fs::rename(&staged, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub fn images_keyring_path(root: &Path) -> PathBuf {
    root.join(format!("etc/ceph/ceph.{IMAGES_ENTITY}.keyring"))
}

pub async fn images_identity<H: Host>(host: &H, root: &Path, pool: &str) -> Result<PathBuf> {
    let path = images_keyring_path(root);
    match std::fs::read_to_string(&path) {
        Ok(text) if keyring_key(&text, IMAGES_ENTITY).is_some() => return Ok(path),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let caps = format!("profile rbd pool={pool}");
    let minted = host
        .ceph(&[
            "auth",
            "get-or-create",
            IMAGES_ENTITY,
            "mon",
            "profile rbd",
            "osd",
            caps.as_str(),
            "--key_type",
            AES256K,
        ])
        .await
        .with_context(|| format!("create {IMAGES_ENTITY}"))?;
    let key = keyring_key(&minted, IMAGES_ENTITY)
        .ok_or_else(|| anyhow!("ceph printed no key for {IMAGES_ENTITY}"))?;
    write_keyring(host, &path, &format!("[{IMAGES_ENTITY}]\n\tkey = {key}\n")).await?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;

    #[test]
    fn a_keyring_key_is_read_only_from_the_named_entity() {
        let text = "[mon.]\n\tkey = MON\n\tcaps mon = \"allow *\"\n[client.admin]\n\tkey=ADMIN\n";
        assert_eq!(keyring_key(text, "mon."), Some("MON".into()));
        assert_eq!(keyring_key(text, "client.admin"), Some("ADMIN".into()));
        assert_eq!(keyring_key(text, "client.other"), None);
    }

    #[tokio::test]
    async fn the_images_disk_is_mapped_with_its_own_aes256k_key() {
        let root = tempfile::tempdir().unwrap();
        let host = FakeHost::new()
            .ok(
                "ceph auth get-or-create client.yolab-images",
                "[client.yolab-images]\n\tkey = K\n",
            )
            .ok("chown", "");

        let path = images_identity(&host, root.path(), "images").await.unwrap();

        assert!(host.ran("auth get-or-create client.yolab-images mon profile rbd osd profile rbd pool=images --key_type aes256k"));
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(keyring_key(&text, "client.yolab-images"), Some("K".into()));
    }

    #[tokio::test]
    async fn an_images_key_already_on_disk_is_reused_without_asking_ceph() {
        let root = tempfile::tempdir().unwrap();
        let path = images_keyring_path(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[client.yolab-images]\n\tkey = K\n").unwrap();
        let host = FakeHost::new();
        assert_eq!(
            images_identity(&host, root.path(), "images").await.unwrap(),
            path
        );
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn a_refused_images_key_is_an_error_not_a_quiet_admin_mapping() {
        let root = tempfile::tempdir().unwrap();
        let host = FakeHost::new().fail("ceph auth get-or-create client.yolab-images", "EACCES");
        assert!(images_identity(&host, root.path(), "images").await.is_err());
    }

    #[test]
    fn lines_before_the_key_in_the_named_entity_are_skipped() {
        let text = "[client.x]\n\tcaps mon = \"allow r\"\n\tkey = X\n";
        assert_eq!(keyring_key(text, "client.x"), Some("X".into()));
    }

    #[tokio::test]
    async fn a_keyring_file_without_a_key_is_replaced_by_a_minted_one() {
        let root = tempfile::tempdir().unwrap();
        let path = images_keyring_path(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[client.yolab-images]\n\tcaps mon = \"profile rbd\"\n",
        )
        .unwrap();
        let host = FakeHost::new()
            .ok(
                "ceph auth get-or-create client.yolab-images",
                "[client.yolab-images]\n\tkey = K\n",
            )
            .ok("chown", "");
        assert_eq!(
            images_identity(&host, root.path(), "images").await.unwrap(),
            path
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(keyring_key(&text, IMAGES_ENTITY), Some("K".into()));
    }

    #[tokio::test]
    async fn a_keyring_path_that_cannot_be_read_is_an_error_not_a_missing_key() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(images_keyring_path(root.path())).unwrap();
        let host = FakeHost::new();
        let err = images_identity(&host, root.path(), "images")
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("read "), "{err}");
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn ceph_printing_no_key_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let host = FakeHost::new().ok("ceph auth get-or-create client.yolab-images", "");
        let err = images_identity(&host, root.path(), "images")
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "ceph printed no key for client.yolab-images"
        );
    }

    #[tokio::test]
    async fn a_minted_key_that_cannot_be_handed_to_ceph_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let host = FakeHost::new()
            .ok(
                "ceph auth get-or-create client.yolab-images",
                "[client.yolab-images]\n\tkey = K\n",
            )
            .fail("chown", "EPERM");
        let err = images_identity(&host, root.path(), "images")
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("chown ceph:ceph "), "{err}");
        assert!(!images_keyring_path(root.path()).exists());
    }

    #[tokio::test]
    async fn a_chown_that_cannot_start_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let host = FakeHost::new().unstartable("chown");
        let path = root.path().join("k.keyring");
        assert!(write_keyring(&host, &path, "k").await.is_err());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_keyring_folder_that_cannot_be_made_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, "").unwrap();
        let host = FakeHost::new();
        assert!(write_keyring(&host, &file.join("k.keyring"), "k")
            .await
            .is_err());
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn a_staged_keyring_that_cannot_be_written_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("k.keyring");
        std::fs::create_dir_all(path.with_extension("yolab-new")).unwrap();
        let host = FakeHost::new();
        let err = write_keyring(&host, &path, "k").await.unwrap_err();
        assert!(err.to_string().starts_with("write "), "{err}");
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn a_staged_keyring_whose_mode_cannot_be_set_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("k.keyring");
        std::os::unix::fs::symlink("/proc/thread-self/comm", path.with_extension("yolab-new"))
            .unwrap();
        let host = FakeHost::new();
        assert!(write_keyring(&host, &path, "yolab-test").await.is_err());
        assert!(host.calls().is_empty());
    }

    #[tokio::test]
    async fn a_keyring_that_cannot_replace_the_old_one_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("k.keyring");
        std::fs::create_dir_all(path.join("inside")).unwrap();
        let host = FakeHost::new().ok("chown", "");
        let err = write_keyring(&host, &path, "k").await.unwrap_err();
        assert!(err.to_string().starts_with("replace "), "{err}");
    }
}
