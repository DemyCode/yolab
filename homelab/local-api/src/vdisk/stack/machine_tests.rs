use super::*;
use crate::host::fake::FakeHost;

const KEY: &str =
    "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ==";

fn rec(id: i64) -> VdiskRecord {
    VdiskRecord {
        id,
        node: "node1".into(),
        box_type: "bx11".into(),
        size_gb: 1,
        host: "u1.your-storagebox.de".into(),
        username: "u1".into(),
        password: "secret".into(),
        osd_id: None,
    }
}

fn touch(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"").unwrap();
}

fn dev(root: &Path, dev: &str) -> PathBuf {
    root.join(dev.trim_start_matches('/'))
}

fn appears(path: PathBuf) -> impl Fn(&str, &[&str]) + Send + Sync + 'static {
    move |_, _| touch(&path)
}

type Step = (&'static str, Result<String, &'static str>);

fn ok(out: &str) -> Result<String, &'static str> {
    Ok(out.to_string())
}

fn script(steps: &[Step], failing: &str) -> FakeHost {
    steps
        .iter()
        .fold(FakeHost::new(), |h, (prefix, answer)| match answer {
            _ if *prefix == failing => h.fail(prefix, "failed"),
            Ok(out) => h.ok(prefix, out),
            Err(e) => h.fail(prefix, e),
        })
}

const NO_KEY: &str = "Error ENOENT: no such key";

fn size_of(id: i64) -> String {
    rec(id).file_bytes().to_string()
}

#[test]
fn a_stored_key_that_is_not_base64_is_an_error() {
    assert_eq!(
        decode_key("!!!").unwrap_err().to_string(),
        "the stored virtual disk key is not base64"
    );
    assert_eq!(decode_key(KEY).unwrap(), vec![1u8; 64]);
}

#[tokio::test]
async fn a_command_that_cannot_start_names_the_program() {
    let host = FakeHost::new().unstartable("truncate");
    let err = checked(&host, "truncate", &["--size=1", "x"], STEP_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "run truncate");
}

#[tokio::test]
async fn a_mount_check_that_cannot_run_is_an_error_not_unmounted() {
    let host = FakeHost::new().unstartable("mountpoint");
    assert_eq!(
        is_mounted(&host, Path::new("/mnt"))
            .await
            .unwrap_err()
            .to_string(),
        "run mountpoint"
    );
}

#[tokio::test]
async fn the_image_size_is_read_from_stat_and_garbage_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let img = image(root.path(), 1);
    let sized = FakeHost::new().ok("stat", "1048576\n");
    assert_eq!(image_size(&sized, &img).await.unwrap(), Some(1_048_576));

    let garbled = FakeHost::new().ok("stat", "lots");
    let err = image_size(&garbled, &img).await.unwrap_err();
    assert!(
        err.to_string().starts_with("stat printed \"lots\""),
        "{err}"
    );

    let unstartable = FakeHost::new().unstartable("stat");
    assert_eq!(
        image_size(&unstartable, &img)
            .await
            .unwrap_err()
            .to_string(),
        "run stat"
    );
}

#[tokio::test]
async fn a_disk_is_attached_once_its_osd_device_exists() {
    let root = tempfile::tempdir().unwrap();
    assert!(!is_attached(root.path(), 2));
    touch(&dev(root.path(), &record::osd_dev(2)));
    assert!(is_attached(root.path(), 2));
}

#[tokio::test]
async fn a_box_already_mounted_is_not_mounted_again() {
    let root = tempfile::tempdir().unwrap();
    let host = FakeHost::new().ok("mountpoint", "");
    mount(&host, root.path(), &rec(1)).await.unwrap();
    assert!(!host.ran("mount.cifs"));
    assert!(!credentials(root.path(), 1).exists());
}

#[tokio::test]
async fn a_mount_fails_when_its_folder_its_check_its_credentials_or_cifs_fail() {
    let blocked = tempfile::tempdir().unwrap();
    touch(&run_dir(blocked.path(), 1));
    let err = mount(&FakeHost::new(), blocked.path(), &rec(1))
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("create "), "{err}");

    let root = tempfile::tempdir().unwrap();
    let unchecked = FakeHost::new().unstartable("mountpoint");
    assert!(mount(&unchecked, root.path(), &rec(1)).await.is_err());

    std::fs::create_dir_all(credentials(root.path(), 1)).unwrap();
    let no_creds = FakeHost::new().fail("mountpoint", "");
    assert!(mount(&no_creds, root.path(), &rec(1)).await.is_err());
    assert!(!no_creds.ran("mount.cifs"));
    std::fs::remove_dir(credentials(root.path(), 1)).unwrap();

    let refused = FakeHost::new()
        .fail("mountpoint", "")
        .fail("mount.cifs", "Permission denied");
    assert_eq!(
        mount(&refused, root.path(), &rec(1))
            .await
            .unwrap_err()
            .to_string(),
        "mount the Storage Box"
    );
}

#[tokio::test]
async fn the_image_is_kept_when_it_has_the_planned_size_and_made_when_it_is_missing() {
    let root = tempfile::tempdir().unwrap();
    let right = FakeHost::new().ok("stat", &size_of(1));
    ensure_image(&right, root.path(), &rec(1), false)
        .await
        .unwrap();
    assert!(!right.ran("truncate"));

    let missing = FakeHost::new()
        .fail("stat", "stat: cannot statx: No such file or directory")
        .ok("truncate", "");
    ensure_image(&missing, root.path(), &rec(1), false)
        .await
        .unwrap();
    assert!(missing.ran(&format!("truncate --size={}", size_of(1))));

    let refused = FakeHost::new()
        .fail("stat", "stat: cannot statx: No such file or directory")
        .fail("truncate", "No space left on device");
    assert_eq!(
        ensure_image(&refused, root.path(), &rec(1), false)
            .await
            .unwrap_err()
            .to_string(),
        "create the virtual disk's file on the Storage Box"
    );

    let down = FakeHost::new().fail("stat", "Host is down");
    assert!(ensure_image(&down, root.path(), &rec(1), false)
        .await
        .is_err());
}

#[tokio::test]
async fn an_existing_loop_device_is_reused_and_recorded() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(run_dir(root.path(), 1)).unwrap();
    let host = FakeHost::new().ok(
        "losetup --associated",
        "/dev/loop4: []: (/run/yolab/vdisks/1/mnt/disk.img)\n",
    );
    assert_eq!(
        ensure_loop(&host, root.path(), 1).await.unwrap(),
        "/dev/loop4"
    );
    assert!(!host.ran("losetup --find"));
    assert_eq!(
        std::fs::read_to_string(loop_file(root.path(), 1)).unwrap(),
        "/dev/loop4"
    );
}

#[tokio::test]
async fn a_new_loop_device_is_made_and_anything_but_a_loop_device_is_refused() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(run_dir(root.path(), 1)).unwrap();
    let fresh = FakeHost::new()
        .ok("losetup --associated", "")
        .ok("losetup --find", "/dev/loop9\n");
    assert_eq!(
        ensure_loop(&fresh, root.path(), 1).await.unwrap(),
        "/dev/loop9"
    );

    let odd = FakeHost::new()
        .ok("losetup --associated", "")
        .ok("losetup --find", "/dev/sda\n");
    assert_eq!(
        ensure_loop(&odd, root.path(), 1)
            .await
            .unwrap_err()
            .to_string(),
        "losetup returned \"/dev/sda\", not a loop device"
    );

    let full = FakeHost::new()
        .ok("losetup --associated", "")
        .fail("losetup --find", "could not find any free loop device");
    assert_eq!(
        ensure_loop(&full, root.path(), 1)
            .await
            .unwrap_err()
            .to_string(),
        "attach the virtual disk's file as a block device"
    );

    let unlisted = FakeHost::new().fail("losetup --associated", "boom");
    assert!(ensure_loop(&unlisted, root.path(), 1).await.is_err());
}

#[tokio::test]
async fn a_loop_device_that_cannot_be_recorded_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let host = FakeHost::new().ok("losetup --associated", "/dev/loop4: x\n");
    assert!(ensure_loop(&host, root.path(), 1).await.is_err());
}

#[tokio::test]
async fn a_stored_key_is_used_and_a_new_one_is_kept_only_once_it_reads_back() {
    let stored = FakeHost::new().ok("ceph config-key get yolab/vdisk-keys/1", KEY);
    assert_eq!(ensure_key(&stored, 1, true).await.unwrap(), vec![1u8; 64]);

    let gone = FakeHost::new().fail(
        "ceph config-key get yolab/vdisk-keys/1",
        "Error ENOENT: no such key",
    );
    assert!(ensure_key(&gone, 1, true)
        .await
        .unwrap_err()
        .to_string()
        .starts_with("the encryption key of this virtual disk is gone"));

    let lost = FakeHost::new()
        .fail(
            "ceph config-key get yolab/vdisk-keys/1",
            "Error ENOENT: no such key",
        )
        .ok("ceph config-key set yolab/vdisk-keys/1", "");
    assert_eq!(
        ensure_key(&lost, 1, false).await.unwrap_err().to_string(),
        "the new virtual disk key did not read back from the cluster"
    );

    let unreadable = FakeHost::new().fail("ceph config-key get", "timed out");
    assert!(ensure_key(&unreadable, 1, false).await.is_err());

    let unwritable = FakeHost::new()
        .fail(
            "ceph config-key get yolab/vdisk-keys/1",
            "Error ENOENT: no such key",
        )
        .fail("ceph config-key set", "EACCES");
    assert!(ensure_key(&unwritable, 1, false).await.is_err());

    let flaky = FakeHost::new()
        .fail(
            "ceph config-key get yolab/vdisk-keys/1",
            "Error ENOENT: no such key",
        )
        .fail("ceph config-key get yolab/vdisk-keys/1", "timed out")
        .ok("ceph config-key set yolab/vdisk-keys/1", "");
    assert!(ensure_key(&flaky, 1, false).await.is_err());
}

#[tokio::test]
async fn a_new_key_that_reads_back_is_used() {
    let host = FakeHost::new()
        .fail("ceph config-key get yolab/vdisk-keys/1", NO_KEY)
        .ok("ceph config-key set yolab/vdisk-keys/1", "");
    let cluster = host.clone();
    let host = host.effect("ceph config-key set yolab/vdisk-keys/1", move |_, args| {
        cluster.now_answers("ceph config-key get yolab/vdisk-keys/1", args[3]);
    });
    assert_eq!(ensure_key(&host, 1, false).await.unwrap().len(), 64);
}

#[tokio::test]
async fn an_open_encryption_layer_is_not_opened_twice() {
    let root = tempfile::tempdir().unwrap();
    touch(&dev(root.path(), &record::crypt_dev(1)));
    let host = FakeHost::new();
    open_crypt(&host, root.path(), 1, "/dev/loop4", &[1u8; 64])
        .await
        .unwrap();
    assert_eq!(host.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn a_key_file_that_cannot_be_written_or_removed_stops_the_open() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(key_file(root.path(), 1)).unwrap();
    let host = FakeHost::new().ok("cryptsetup open", "");
    assert!(open_crypt(&host, root.path(), 1, "/dev/loop4", &[1u8; 64])
        .await
        .is_err());
    assert!(!host.ran("cryptsetup"));

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(run_dir(root.path(), 1)).unwrap();
    let kf = key_file(root.path(), 1);
    let host = FakeHost::new()
        .ok("cryptsetup open", "")
        .effect("cryptsetup open", move |_, _| {
            std::fs::remove_file(&kf).unwrap();
            std::fs::create_dir(&kf).unwrap();
        });
    let err = open_crypt(&host, root.path(), 1, "/dev/loop4", &[1u8; 64])
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("remove "), "{err}");
}

#[tokio::test]
async fn the_content_type_is_read_with_blkid() {
    let lvm = FakeHost::new().ok("blkid", "LVM2_member\n");
    assert_eq!(content_type(&lvm, "/dev/x").await.unwrap(), "LVM2_member");

    let broken = FakeHost::new().fail("blkid", "Input/output error");
    assert_eq!(
        content_type(&broken, "/dev/x")
            .await
            .unwrap_err()
            .to_string(),
        "blkid could not read /dev/x"
    );

    let missing = FakeHost::new().unstartable("blkid");
    assert_eq!(
        content_type(&missing, "/dev/x")
            .await
            .unwrap_err()
            .to_string(),
        "run blkid"
    );
}

fn formats(failing: &str) -> FakeHost {
    script(
        &[
            ("dmsetup remove", ok("")),
            ("pvcreate", ok("")),
            ("vgcreate", ok("")),
            ("lvcreate", ok("")),
            ("ceph config-key set yolab/vdisk-formatted/1 1", ok("")),
        ],
        failing,
    )
}

#[tokio::test]
async fn formatting_removes_a_stale_osd_mapping_then_builds_the_volume_and_records_it() {
    let root = tempfile::tempdir().unwrap();
    touch(&dev(root.path(), &record::osd_dev(1)));
    let host = formats("");
    format(&host, root.path(), 1).await.unwrap();
    let order: Vec<Option<usize>> = [
        "dmsetup remove --retry yolab_vdisk_1-osd",
        "pvcreate -ff -y /dev/mapper/yolab-vdisk-1",
        "vgcreate -y yolab_vdisk_1 /dev/mapper/yolab-vdisk-1",
        "lvcreate -y -l 100%FREE -n osd yolab_vdisk_1",
        "ceph config-key set yolab/vdisk-formatted/1 1",
    ]
    .iter()
    .map(|c| host.position(c))
    .collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted);
    assert!(order.iter().all(Option::is_some));
}

#[tokio::test]
async fn formatting_stops_at_the_first_step_that_fails() {
    let root = tempfile::tempdir().unwrap();
    touch(&dev(root.path(), &record::osd_dev(1)));
    for (step, later) in [
        ("dmsetup remove", "pvcreate"),
        ("pvcreate", "vgcreate"),
        ("vgcreate", "lvcreate"),
        ("lvcreate", "config-key set"),
        ("ceph config-key set yolab/vdisk-formatted/1 1", "never"),
    ] {
        let host = formats(step);
        assert!(format(&host, root.path(), 1).await.is_err(), "{step}");
        assert!(!host.ran(later), "{step} then {later}");
    }
}

pub(in crate::vdisk) fn attaches(
    root: &Path,
    formatted: bool,
    content: &str,
    failing: &str,
) -> FakeHost {
    let osd = dev(root, &record::osd_dev(1));
    let formatted = if formatted { ok("1") } else { Err(NO_KEY) };
    script(
        &[
            ("ceph config-key get yolab/vdisk-formatted/1", formatted),
            ("mountpoint", Err("")),
            ("mount.cifs", ok("")),
            ("stat", Ok(size_of(1))),
            ("losetup --associated", ok("/dev/loop4: x\n")),
            ("ceph config-key get yolab/vdisk-keys/1", ok(KEY)),
            ("cryptsetup open", ok("")),
            ("cryptsetup close", ok("")),
            ("blkid", ok(content)),
            ("pvcreate", ok("")),
            ("vgcreate", ok("")),
            ("lvcreate", ok("")),
            ("ceph config-key set", ok("")),
            ("vgchange", ok("")),
        ],
        failing,
    )
    .effect("vgchange", appears(osd))
}

#[tokio::test]
async fn a_fresh_box_is_formatted_activated_and_attached() {
    let root = tempfile::tempdir().unwrap();
    let host = attaches(root.path(), false, "", "");
    attach(&host, root.path(), &rec(1)).await.unwrap();
    assert!(host.ran("lvcreate"));
    assert!(host.ran("vgchange -ay yolab_vdisk_1"));
    assert!(is_attached(root.path(), 1));
}

#[tokio::test]
async fn a_formatted_box_is_activated_without_being_formatted_again() {
    let root = tempfile::tempdir().unwrap();
    touch(&dev(root.path(), &record::osd_dev(1)));
    let host = attaches(root.path(), true, "LVM2_member", "");
    attach(&host, root.path(), &rec(1)).await.unwrap();
    assert!(!host.ran("pvcreate") && !host.ran("vgchange"));
}

#[tokio::test]
async fn a_box_that_does_not_decrypt_is_closed_and_refused() {
    let root = tempfile::tempdir().unwrap();
    let host = attaches(root.path(), true, "xfs", "");
    let err = attach(&host, root.path(), &rec(1)).await.unwrap_err();
    assert!(
        err.to_string()
            .starts_with("the virtual disk does not decrypt"),
        "{err}"
    );
    assert!(host.ran("cryptsetup close yolab-vdisk-1"));

    let stuck = attaches(root.path(), true, "xfs", "cryptsetup close");
    assert_eq!(
        attach(&stuck, root.path(), &rec(1))
            .await
            .unwrap_err()
            .to_string(),
        "close the encryption layer that did not decrypt"
    );
}

#[tokio::test]
async fn an_attach_stops_at_the_first_layer_that_fails() {
    let root = tempfile::tempdir().unwrap();
    let mut unprovisioned = rec(1);
    unprovisioned.password.clear();
    assert_eq!(
        attach(&FakeHost::new(), root.path(), &unprovisioned)
            .await
            .unwrap_err()
            .to_string(),
        "the Storage Box is not ready yet"
    );

    for (step, later) in [
        ("ceph config-key get yolab/vdisk-formatted/1", "mountpoint"),
        ("mount.cifs", "stat"),
        ("losetup --associated", "cryptsetup"),
        ("ceph config-key get yolab/vdisk-keys/1", "cryptsetup"),
        ("cryptsetup open", "blkid"),
        ("blkid", "pvcreate"),
        ("pvcreate", "vgchange"),
        ("vgchange", "never"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let host = attaches(root.path(), false, "", step);
        assert!(attach(&host, root.path(), &rec(1)).await.is_err(), "{step}");
        assert!(!host.ran(later), "{step} then {later}");
    }
}

#[tokio::test]
async fn an_osd_device_that_never_appears_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let host = FakeHost::new()
        .ok("ceph config-key get yolab/vdisk-formatted/1", "1")
        .ok("mountpoint", "")
        .ok("stat", &size_of(1))
        .ok("losetup --associated", "/dev/loop4: x\n")
        .ok("ceph config-key get yolab/vdisk-keys/1", KEY)
        .ok("cryptsetup open", "")
        .ok("blkid", "LVM2_member")
        .ok("vgchange", "");
    std::fs::create_dir_all(run_dir(root.path(), 1)).unwrap();
    assert_eq!(
        attach(&host, root.path(), &rec(1))
            .await
            .unwrap_err()
            .to_string(),
        "/dev/mapper/yolab_vdisk_1-osd did not appear after activating it"
    );
}

#[test]
fn the_recorded_loop_device_is_read_back_and_an_empty_record_is_none() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(recorded_loop(root.path(), 1).unwrap(), None);
    touch(&loop_file(root.path(), 1));
    assert_eq!(recorded_loop(root.path(), 1).unwrap(), None);
    std::fs::write(loop_file(root.path(), 1), "/dev/loop2\n").unwrap();
    assert_eq!(
        recorded_loop(root.path(), 1).unwrap(),
        Some("/dev/loop2".to_string())
    );
    std::fs::remove_file(loop_file(root.path(), 1)).unwrap();
    std::fs::create_dir_all(loop_file(root.path(), 1)).unwrap();
    assert_eq!(
        recorded_loop(root.path(), 1).unwrap_err().to_string(),
        "read the recorded loop device"
    );
}

#[test]
fn a_loop_device_is_in_use_while_the_kernel_lists_its_backing_file() {
    let root = tempfile::tempdir().unwrap();
    assert!(!loop_in_use(root.path(), "/dev/loop2"));
    std::fs::create_dir_all(root.path().join("sys/block/loop2/loop")).unwrap();
    assert!(loop_in_use(root.path(), "/dev/loop2"));
}

fn fully_attached(root: &Path, id: i64) {
    touch(&dev(root, &record::osd_dev(id)));
    touch(&dev(root, &record::crypt_dev(id)));
    std::fs::create_dir_all(mount_point(root, id)).unwrap();
    std::fs::write(loop_file(root, id), "/dev/loop2\n").unwrap();
    std::fs::create_dir_all(root.join("sys/block/loop2/loop")).unwrap();
    touch(&credentials(root, id));
    touch(&key_file(root, id));
    let osd = root.join("var/lib/ceph/osd/ceph-12");
    std::fs::create_dir_all(&osd).unwrap();
    std::os::unix::fs::symlink(record::osd_dev(id), osd.join("block")).unwrap();
}

fn detaches(failing: &str) -> FakeHost {
    script(
        &[
            ("systemctl stop", ok("")),
            ("dmsetup remove", ok("")),
            ("cryptsetup close", ok("")),
            ("losetup --detach", ok("")),
            ("mountpoint", ok("")),
            ("umount", ok("")),
        ],
        failing,
    )
}

#[tokio::test]
async fn a_detach_takes_every_layer_down_from_the_top_and_cleans_up() {
    let root = tempfile::tempdir().unwrap();
    fully_attached(root.path(), 1);
    let host = detaches("");

    detach(&host, root.path(), 1).await.unwrap();

    let order: Vec<Option<usize>> = [
        "systemctl stop yolab-ceph-osd@12.service",
        "dmsetup remove --retry yolab_vdisk_1-osd",
        "cryptsetup close yolab-vdisk-1",
        "losetup --detach /dev/loop2",
        "umount --force --lazy",
    ]
    .iter()
    .map(|c| host.position(c))
    .collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted);
    assert!(order.iter().all(Option::is_some));
    assert!(!loop_file(root.path(), 1).exists());
    assert!(!credentials(root.path(), 1).exists());
    assert!(!key_file(root.path(), 1).exists());
}

#[tokio::test]
async fn a_detach_of_a_loop_device_the_kernel_already_freed_only_forgets_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(run_dir(root.path(), 1)).unwrap();
    std::fs::write(loop_file(root.path(), 1), "/dev/loop2\n").unwrap();
    let host = FakeHost::new();
    detach(&host, root.path(), 1).await.unwrap();
    assert!(!host.ran("losetup"));
    assert!(!loop_file(root.path(), 1).exists());
}

#[tokio::test]
async fn a_detach_stops_at_the_first_layer_that_will_not_come_down() {
    for (step, later) in [
        ("systemctl stop", "dmsetup"),
        ("dmsetup remove", "cryptsetup"),
        ("cryptsetup close", "losetup"),
        ("losetup --detach", "umount"),
        ("umount", "never"),
    ] {
        let root = tempfile::tempdir().unwrap();
        fully_attached(root.path(), 1);
        let host = detaches(step);
        assert!(detach(&host, root.path(), 1).await.is_err(), "{step}");
        assert!(!host.ran(later), "{step} then {later}");
    }

    let root = tempfile::tempdir().unwrap();
    fully_attached(root.path(), 1);
    let no_systemctl = detaches("").unstartable("systemctl");
    assert!(detach(&no_systemctl, root.path(), 1).await.is_err());

    let root = tempfile::tempdir().unwrap();
    fully_attached(root.path(), 1);
    let no_mountpoint = detaches("").unstartable("mountpoint");
    assert!(detach(&no_mountpoint, root.path(), 1).await.is_err());
    assert!(!no_mountpoint.ran("umount"));
}

#[tokio::test]
async fn a_detach_whose_records_cannot_be_removed_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    fully_attached(root.path(), 1);
    let lf = loop_file(root.path(), 1);
    let host = detaches("").effect("losetup --detach", move |_, _| {
        std::fs::remove_file(&lf).unwrap();
        std::fs::create_dir(&lf).unwrap();
    });
    assert!(detach(&host, root.path(), 1).await.is_err());
    assert!(!host.ran("umount"));

    let root = tempfile::tempdir().unwrap();
    fully_attached(root.path(), 1);
    std::fs::remove_file(loop_file(root.path(), 1)).unwrap();
    std::fs::remove_file(credentials(root.path(), 1)).unwrap();
    std::fs::create_dir_all(credentials(root.path(), 1)).unwrap();
    assert!(detach(&detaches(""), root.path(), 1).await.is_err());

    let root = tempfile::tempdir().unwrap();
    fully_attached(root.path(), 1);
    std::fs::remove_file(loop_file(root.path(), 1)).unwrap();
    std::fs::remove_file(key_file(root.path(), 1)).unwrap();
    std::fs::create_dir_all(key_file(root.path(), 1)).unwrap();
    assert!(detach(&detaches(""), root.path(), 1).await.is_err());

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(loop_file(root.path(), 1)).unwrap();
    assert!(detach(&FakeHost::new(), root.path(), 1).await.is_err());
}

#[test]
fn the_disks_attached_here_are_the_numbered_run_folders() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(attached_here(root.path()).unwrap(), Vec::<i64>::new());
    for d in ["3", "1", "notes"] {
        std::fs::create_dir_all(root.path().join("run/yolab/vdisks").join(d)).unwrap();
    }
    assert_eq!(attached_here(root.path()).unwrap(), vec![1, 3]);

    let blocked = tempfile::tempdir().unwrap();
    touch(&blocked.path().join("run/yolab/vdisks"));
    let err = attached_here(blocked.path()).unwrap_err();
    assert!(err.to_string().starts_with("read "), "{err}");
}
