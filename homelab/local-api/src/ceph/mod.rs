//! Ceph knowledge that is not a subprocess runner: the typed shape of what Ceph
//! answers (`model`) and the single door to commands that destroy data
//! (`destructive`). The runner itself is `crate::ceph_cli`.
pub mod destructive;
pub mod model;
