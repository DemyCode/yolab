//! Small pieces shared by more than one storage subcommand: the mon address
//! form Ceph expects, and the per-host mon store path. Kept out of any one
//! subcommand's file so bootstrap.rs and mon_member.rs (both of which touch
//! the monmap) cannot drift apart on the address format.
//!
//! `hostname()` used to live here too; it moved to `crate::system` once
//! `boot::*` (not just `storage::*`) needed it.

use std::path::{Path, PathBuf};

/// Ceph's address form: one bracketed group per mon, v2 and v1 inside it.
/// Mirrors `addrvec` in homelab/nixos/ceph/default.nix — the two must never
/// disagree, since the join path compares what this produces against what a
/// running mon already advertises.
pub fn addrvec(addr: &str) -> String {
    format!("[v2:[{addr}]:3300,v1:[{addr}]:6789]")
}

/// Where this node's mon store lives. `root` is `/` in production and a
/// tempdir in tests, so file-writing logic can be exercised without touching
/// the real filesystem.
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
