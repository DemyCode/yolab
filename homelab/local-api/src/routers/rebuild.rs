use std::path::Path;

use axum::{extract::State, Json};
use serde::Serialize;

use crate::{config::Config, AppState};

#[derive(Serialize)]
pub struct RebuildLog {
    pub running: bool,
    pub log: Vec<String>,
}

fn pid_is_running(proc: &Path, pid: u32) -> bool {
    match std::fs::read_to_string(proc.join(pid.to_string()).join("status")) {
        Err(_) => false,
        Ok(s) => !s
            .lines()
            .find(|l| l.starts_with("State:"))
            .map(|l| l.contains('Z'))
            .unwrap_or(false),
    }
}

fn read_rebuild_log(cfg: &Config, proc: &Path) -> RebuildLog {
    let running = std::fs::read_to_string(&cfg.rebuild_pid)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .is_some_and(|pid| pid_is_running(proc, pid));
    let log = std::fs::read_to_string(&cfg.rebuild_log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    RebuildLog { running, log }
}

pub async fn rebuild_log(State(state): State<AppState>) -> Json<RebuildLog> {
    Json(read_rebuild_log(&state.config, Path::new("/proc")))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Machine {
        dir: tempfile::TempDir,
        cfg: Config,
    }

    impl Machine {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut cfg = Config::for_test(&dir.path().join("config.toml"));
            cfg.rebuild_log = dir.path().join("rebuild.log");
            cfg.rebuild_pid = dir.path().join("rebuild.pid");
            Self { dir, cfg }
        }

        fn proc(&self) -> std::path::PathBuf {
            self.dir.path().join("proc")
        }

        fn process(&self, pid: u32, state: &str) {
            let dir = self.proc().join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("status"),
                format!("Name:\tnixos-rebuild\nState:\t{state}\nPid:\t{pid}\n"),
            )
            .unwrap();
        }

        fn rebuilding(&self, pid: u32) {
            std::fs::write(&self.cfg.rebuild_pid, pid.to_string()).unwrap();
        }

        fn read(&self) -> RebuildLog {
            read_rebuild_log(&self.cfg, &self.proc())
        }
    }

    #[test]
    fn a_live_rebuild_is_running() {
        let m = Machine::new();
        m.process(812, "S (sleeping)");
        m.rebuilding(812);
        assert!(m.read().running);
    }

    #[test]
    fn a_zombie_rebuild_has_finished() {
        let m = Machine::new();
        m.process(812, "Z (zombie)");
        m.rebuilding(812);
        assert!(!m.read().running);
    }

    #[test]
    fn a_pid_file_left_by_a_process_that_is_gone_is_not_running() {
        let m = Machine::new();
        m.rebuilding(812);
        assert!(!m.read().running);
    }

    #[test]
    fn no_pid_file_or_a_garbled_one_is_not_running() {
        let m = Machine::new();
        m.process(812, "R (running)");
        assert!(!m.read().running);
        std::fs::write(&m.cfg.rebuild_pid, "not a pid").unwrap();
        assert!(!m.read().running);
    }

    #[test]
    fn the_log_is_returned_line_by_line_and_is_empty_before_any_rebuild() {
        let m = Machine::new();
        assert!(m.read().log.is_empty());
        std::fs::write(&m.cfg.rebuild_log, "building\nactivating\n").unwrap();
        assert_eq!(m.read().log, ["building", "activating"]);
    }
}
