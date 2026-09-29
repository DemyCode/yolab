use std::path::Path;

pub fn hostname() -> String {
    hostname_in(Path::new("/"))
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn hostname_in(root: &Path) -> String {
    std::fs::read_to_string(root.join("etc/hostname"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hostname_is_the_trimmed_first_line_of_etc_hostname() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(dir.path().join("etc/hostname"), "node1\n").unwrap();
        assert_eq!(hostname_in(dir.path()), "node1");
    }

    #[test]
    fn the_clock_reads_unix_seconds() {
        let after_this_code_was_written = 1_788_000_000;
        assert!(now_secs() > after_this_code_was_written);
    }

    #[test]
    fn a_machine_without_etc_hostname_has_an_empty_name_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(hostname_in(dir.path()), "");
    }
}
