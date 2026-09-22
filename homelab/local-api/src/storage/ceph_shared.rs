
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const POOL_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

pub fn addrvec(addr: &str) -> String {
    format!("[v2:[{addr}]:3300,v1:[{addr}]:6789]")
}

pub fn mon_dir(root: &Path, node: &str) -> PathBuf {
    root.join(format!("var/lib/ceph/mon/ceph-{node}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addrvec_carries_both_msgr_versions() {
        assert_eq!(
            addrvec("fd00:cafe::1"),
            "[v2:[fd00:cafe::1]:3300,v1:[fd00:cafe::1]:6789]"
        );
    }

    #[test]
    fn mon_dir_is_rooted_and_scoped_to_the_host() {
        assert_eq!(
            mon_dir(Path::new("/"), "yolab-n1"),
            PathBuf::from("/var/lib/ceph/mon/ceph-yolab-n1")
        );
        assert_eq!(
            mon_dir(Path::new("/tmp/test-root"), "yolab-n1"),
            PathBuf::from("/tmp/test-root/var/lib/ceph/mon/ceph-yolab-n1")
        );
    }
}
