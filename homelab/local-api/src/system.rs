//! Small host-identity helpers shared across subcommand modules — both
//! `storage::*` and `boot::*` need this node's hostname, and it has to be the
//! same read in both places rather than two copies that could drift.

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
