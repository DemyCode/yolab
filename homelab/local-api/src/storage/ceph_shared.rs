//! Small pieces shared by more than one storage subcommand: the mon address
//! form Ceph expects, and the per-host mon store path. Kept out of any one
//! subcommand's file so bootstrap.rs and mon_member.rs (both of which touch
//! the monmap) cannot drift apart on the address format.
//!
//! `hostname()` used to live here too; it moved to `crate::system` once
//! `boot::*` (not just `storage::*`) needed it.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a pool gets to prove it can still serve a read.
///
/// DELIBERATELY FAR SHORTER THAN `host::RUN_CMD_TIMEOUT`, and the gap is the point.
///
/// `rbd ls` reads one small directory object out of the pool. Against a pool that
/// works it answers in well under a second. Against a pool whose PGs are `down` it
/// does not answer at all, ever: Ceph blocks rather than fails a read it cannot
/// serve, and the client retries for as long as it is allowed to. So "can this pool
/// serve reads right now?" is fully answered, either way, within seconds — and every
/// additional second of budget past that buys nothing but a longer outage.
///
/// The 600s default turned that distinction into 23 hours of downtime on 2026-09-10,
/// and then into a second outage the next morning. Both units that probe the images
/// pool have to share this bound, which is why it lives here rather than in either
/// one:
///
///   - `containerd_store` blocked the full ten minutes per tick while the data-root
///     sat unreadable. Journal, once per timer tick all night: `Consumed 663ms CPU
///     time over 10min 520ms wall clock`.
///   - `images_rbd` is ordered BEFORE `containerd_store`, so bounding only the
///     latter fixed nothing on a cold boot: on node2 (2026-09-11, 10:08:50) this
///     unit held `start` while `yolab-containerd-store`, `k3s` and
///     `yolab-local-api` all sat in `waiting` behind it, with zero failed units.
///     A node cannot rejoin the cluster while its k3s is queued behind a pool that
///     will never answer.
pub const POOL_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

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
