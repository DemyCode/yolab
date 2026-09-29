use std::future::Future;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::ceph::destructive::Door;
use crate::ceph::model::{self, OsdDump, PgBrief};
pub use crate::exec::CommandOutput;
use crate::exec::{self, CmdError};

pub const RUN_CMD_TIMEOUT: Duration = Duration::from_secs(600);

pub type HostResult<T> = Result<T, CmdError>;

#[allow(clippy::manual_async_fn)]
pub trait Host: Send + Sync + Clone {
    fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = HostResult<String>> + Send + 'a;
    fn ceph_json<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<Value>> + Send + 'a;
    fn ceph_volume<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a;
    fn systemctl<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a;
    fn run_cmd<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a;

    fn ceph_destructive<'a>(
        &self,
        _door: &Door,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        self.ceph(args)
    }

    fn ceph_volume_destructive<'a>(
        &self,
        _door: &Door,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        self.ceph_volume(args)
    }

    fn run_cmd_bounded<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        _timeout: Duration,
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        self.run_cmd(bin, args)
    }

    fn run_cmd_env<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        _env: &'a [(&'a str, &'a str)],
        _timeout: Duration,
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        async move {
            Err(CmdError::Failed {
                cmd: exec::render(bin, args),
                kind: exec::Failure::Other,
                stderr: "not supported by this host".into(),
            })
        }
    }

    fn run_lines<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        _timeout: Duration,
        _on_line: &'a (dyn Fn(String) + Send + Sync),
    ) -> impl Future<Output = HostResult<bool>> + Send + 'a {
        async move {
            Err(CmdError::Failed {
                cmd: exec::render(bin, args),
                kind: exec::Failure::Other,
                stderr: "not supported by this host".into(),
            })
        }
    }

    fn unit_is_active<'a>(&'a self, unit: &'a str) -> impl Future<Output = bool> + Send + 'a {
        async move {
            self.systemctl(&["is-active", "--quiet", unit])
                .await
                .is_ok_and(|o| o.success)
        }
    }

    fn run_checked<'a>(
        &'a self,
        bin: &'a str,
        args: &'a [&'a str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        async move { exec::into_checked(bin, args, self.run_cmd(bin, args).await?) }
    }

    fn spawn_detached<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        _log: &'a Path,
        _pid_file: &'a Path,
    ) -> impl Future<Output = HostResult<u32>> + Send + 'a {
        async move {
            Err(CmdError::Failed {
                cmd: exec::render(bin, args),
                kind: exec::Failure::Other,
                stderr: "not supported by this host".into(),
            })
        }
    }

    fn ceph_typed<'a, T: serde::de::DeserializeOwned + Send + 'a>(
        &'a self,
        args: &'a [&'a str],
    ) -> impl Future<Output = HostResult<T>> + Send + 'a {
        async move {
            let v = self.ceph_json(args).await?;
            serde_json::from_value(v).map_err(|e| CmdError::parse(exec::render("ceph", args), e))
        }
    }

    fn reachable(&self) -> impl Future<Output = bool> + Send + '_ {
        async move { self.ceph(&["-s"]).await.is_ok() }
    }

    fn cluster_fsid(&self) -> impl Future<Output = HostResult<String>> + Send + '_ {
        async move {
            let first = match self.ceph_json(&["fsid"]).await {
                Ok(v) => match v["fsid"].as_str().filter(|s| !s.is_empty()) {
                    Some(f) => return Ok(f.to_string()),
                    None => CmdError::parse("ceph fsid -f json", "no fsid field"),
                },
                Err(e) => e,
            };
            if first.is_unanswered() {
                return Err(first);
            }
            let plain = self.ceph(&["fsid"]).await?;
            let f = plain.trim();
            if f.is_empty() {
                return Err(CmdError::parse("ceph fsid", "empty output"));
            }
            Ok(f.to_string())
        }
    }

    fn osd_ids(&self) -> impl Future<Output = HostResult<Vec<i64>>> + Send + '_ {
        async move {
            let v = self.ceph_json(&["osd", "ls"]).await?;
            serde_json::from_value::<Vec<i64>>(v).map_err(|e| CmdError::parse("ceph osd ls", e))
        }
    }

    fn osd_dump(&self) -> impl Future<Output = HostResult<OsdDump>> + Send + '_ {
        async move {
            let v = self.ceph_json(&["osd", "dump"]).await?;
            serde_json::from_value(v).map_err(|e| CmdError::parse("ceph osd dump", e))
        }
    }

    fn pgs_brief(&self) -> impl Future<Output = HostResult<Vec<PgBrief>>> + Send + '_ {
        async move {
            let raw = self
                .ceph(&["pg", "dump", "pgs_brief", "-f", "json"])
                .await?;
            model::parse_pgs_brief("ceph pg dump pgs_brief", &raw)
        }
    }
}

#[derive(Clone, Default)]
pub struct RealHost;

pub static HOST: RealHost = RealHost;

#[allow(clippy::manual_async_fn)]
impl Host for RealHost {
    fn ceph<'a>(&self, args: &'a [&str]) -> impl Future<Output = HostResult<String>> + Send + 'a {
        async move { crate::ceph_cli::ceph(args).await }
    }

    fn ceph_json<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<Value>> + Send + 'a {
        async move { crate::ceph_cli::ceph_json(args).await }
    }

    fn ceph_volume<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        async move { crate::ceph_cli::ceph_volume(args).await }
    }

    fn ceph_destructive<'a>(
        &self,
        door: &Door,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        crate::ceph_cli::ceph_destructive(door, args)
    }

    fn ceph_volume_destructive<'a>(
        &self,
        door: &Door,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<String>> + Send + 'a {
        crate::ceph_cli::ceph_volume_destructive(door, args)
    }

    fn systemctl<'a>(
        &self,
        args: &'a [&str],
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        async move { exec::output("systemctl", args, RUN_CMD_TIMEOUT).await }
    }

    fn run_cmd<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        self.run_cmd_bounded(bin, args, RUN_CMD_TIMEOUT)
    }

    fn run_cmd_bounded<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        async move { exec::output(bin, args, timeout).await }
    }

    fn run_cmd_env<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        env: &'a [(&'a str, &'a str)],
        timeout: Duration,
    ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
        async move { exec::output_env_unthrottled(bin, args, env, timeout).await }
    }

    fn run_lines<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
        on_line: &'a (dyn Fn(String) + Send + Sync),
    ) -> impl Future<Output = HostResult<bool>> + Send + 'a {
        async move { exec::stream_lines(bin, args, timeout, on_line).await }
    }

    fn spawn_detached<'a>(
        &self,
        bin: &'a str,
        args: &'a [&'a str],
        log: &'a Path,
        pid_file: &'a Path,
    ) -> impl Future<Output = HostResult<u32>> + Send + 'a {
        async move { exec::spawn_detached(bin, args, log, pid_file) }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::{
        collections::VecDeque,
        future::Future,
        sync::{Arc, Mutex},
    };

    use serde_json::Value;

    use super::{CommandOutput, Door, Host, HostResult};
    use crate::ceph::destructive::is_destructive;
    use crate::exec::CmdError;

    type ScriptedAnswer = std::result::Result<String, String>;
    type Script = Vec<(String, VecDeque<ScriptedAnswer>)>;

    type Env = Vec<(String, String)>;

    #[derive(Clone, Default)]
    pub(crate) struct FakeHost {
        calls: Arc<Mutex<Vec<String>>>,
        script: Arc<Mutex<Script>>,
        envs: Arc<Mutex<Vec<(String, Env)>>>,
    }

    impl FakeHost {
        pub fn new() -> Self {
            Self::default()
        }

        fn push(&self, prefix: &str, answer: ScriptedAnswer) {
            let mut script = self.script.lock().unwrap();
            match script.iter_mut().find(|(p, _)| p == prefix) {
                Some((_, q)) => q.push_back(answer),
                None => {
                    let mut q = VecDeque::new();
                    q.push_back(answer);
                    script.push((prefix.to_string(), q));
                }
            }
        }

        pub fn ok(self, prefix: &str, out: &str) -> Self {
            self.push(prefix, Ok(out.to_string()));
            self
        }

        pub fn fail(self, prefix: &str, err: &str) -> Self {
            self.push(prefix, Err(err.to_string()));
            self
        }

        pub fn env_of(&self, needle: &str) -> Option<Env> {
            self.envs
                .lock()
                .unwrap()
                .iter()
                .find(|(cmd, _)| cmd.contains(needle))
                .map(|(_, env)| env.clone())
        }

        pub fn envs_of(&self, needle: &str) -> Vec<Env> {
            self.envs
                .lock()
                .unwrap()
                .iter()
                .filter(|(cmd, _)| cmd.contains(needle))
                .map(|(_, env)| env.clone())
                .collect()
        }

        fn answer(&self, cmd: &str) -> HostResult<String> {
            self.calls.lock().unwrap().push(cmd.to_string());
            let mut script = self.script.lock().unwrap();
            let best = script
                .iter_mut()
                .filter(|(p, _)| cmd.starts_with(p.as_str()))
                .max_by_key(|(p, _)| p.len());
            match best {
                Some((_, q)) => {
                    let out = if q.len() > 1 {
                        q.pop_front().unwrap()
                    } else {
                        q.front().unwrap().clone()
                    };
                    out.map_err(|e| CmdError::failed(cmd, e))
                }
                None => Err(CmdError::failed(cmd, format!("unscripted command: {cmd}"))),
            }
        }

        fn guarded(&self, bin: &str, args: &[&str]) -> HostResult<String> {
            let cmd = format!("{bin} {}", args.join(" "));
            if is_destructive(bin, args) {
                self.calls.lock().unwrap().push(format!("REFUSED {cmd}"));
                return Err(CmdError::Forbidden { cmd });
            }
            self.answer(&cmd)
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        pub fn ran(&self, needle: &str) -> bool {
            self.calls()
                .iter()
                .any(|c| !c.starts_with("REFUSED ") && c.contains(needle))
        }

        pub fn refused(&self, needle: &str) -> bool {
            self.calls()
                .iter()
                .any(|c| c.starts_with("REFUSED ") && c.contains(needle))
        }

        pub fn position(&self, needle: &str) -> Option<usize> {
            self.calls().iter().position(|c| c.contains(needle))
        }
    }

    fn output_of(out: HostResult<String>) -> CommandOutput {
        match out {
            Ok(stdout) => CommandOutput {
                success: true,
                stdout,
                stderr: String::new(),
            },
            Err(e) => CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: e.to_string(),
            },
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl Host for FakeHost {
        fn ceph<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let me = self.clone();
            async move { me.guarded("ceph", args) }
        }

        fn ceph_json<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<Value>> + Send + 'a {
            let me = self.clone();
            async move {
                let raw = me.guarded("ceph", args)?;
                crate::exec::parse_json(&format!("ceph {}", args.join(" ")), &raw)
            }
        }

        fn ceph_volume<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let me = self.clone();
            async move { me.guarded("ceph-volume", args) }
        }

        fn ceph_destructive<'a>(
            &self,
            _door: &Door,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("ceph {}", args.join(" "))) }
        }

        fn ceph_volume_destructive<'a>(
            &self,
            _door: &Door,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<String>> + Send + 'a {
            let me = self.clone();
            async move { me.answer(&format!("ceph-volume {}", args.join(" "))) }
        }

        fn systemctl<'a>(
            &self,
            args: &'a [&str],
        ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move {
                Ok(output_of(
                    me.answer(&format!("systemctl {}", args.join(" "))),
                ))
            }
        }

        fn run_cmd<'a>(
            &self,
            bin: &'a str,
            args: &'a [&'a str],
        ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move { Ok(output_of(me.answer(&format!("{bin} {}", args.join(" "))))) }
        }

        fn run_cmd_env<'a>(
            &self,
            bin: &'a str,
            args: &'a [&'a str],
            env: &'a [(&'a str, &'a str)],
            _timeout: std::time::Duration,
        ) -> impl Future<Output = HostResult<CommandOutput>> + Send + 'a {
            let me = self.clone();
            async move {
                let cmd = format!("{bin} {}", args.join(" "));
                me.envs.lock().unwrap().push((
                    cmd.clone(),
                    env.iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ));
                Ok(output_of(me.answer(&cmd)))
            }
        }

        fn run_lines<'a>(
            &self,
            bin: &'a str,
            args: &'a [&'a str],
            _timeout: std::time::Duration,
            on_line: &'a (dyn Fn(String) + Send + Sync),
        ) -> impl Future<Output = HostResult<bool>> + Send + 'a {
            let me = self.clone();
            async move {
                let (ok, text) = match me.answer(&format!("{bin} {}", args.join(" "))) {
                    Ok(out) => (true, out),
                    Err(e) => (false, e.to_string()),
                };
                text.lines().for_each(|l| on_line(l.to_string()));
                Ok(ok)
            }
        }

        fn spawn_detached<'a>(
            &self,
            bin: &'a str,
            args: &'a [&'a str],
            log: &'a std::path::Path,
            pid_file: &'a std::path::Path,
        ) -> impl Future<Output = HostResult<u32>> + Send + 'a {
            let me = self.clone();
            async move {
                let cmd = format!("{bin} {}", args.join(" "));
                let out = me.answer(&cmd)?;
                let io = |source| CmdError::Spawn {
                    cmd: cmd.clone(),
                    source,
                };
                std::fs::write(log, out).map_err(io)?;
                std::fs::write(pid_file, FAKE_PID.to_string()).map_err(io)?;
                Ok(FAKE_PID)
            }
        }
    }

    pub const FAKE_PID: u32 = 4242;
}

#[cfg(test)]
mod tests {
    use super::fake::FakeHost;
    use super::*;

    #[tokio::test]
    async fn osd_ids_that_are_not_a_list_are_an_error_not_an_empty_cluster() {
        let host = FakeHost::new().ok("ceph osd ls", r#"{"unexpected": true}"#);
        assert!(host.osd_ids().await.is_err());
        let host = FakeHost::new().ok("ceph osd ls", "[0, 3]");
        assert_eq!(host.osd_ids().await.unwrap(), vec![0, 3]);
    }

    #[tokio::test]
    async fn an_unreadable_fsid_is_an_error() {
        let host = FakeHost::new().fail("ceph fsid", "timed out");
        assert!(host.cluster_fsid().await.is_err());
        let host = FakeHost::new()
            .ok("ceph fsid -f json", r#"{"fsid":""}"#)
            .ok("ceph fsid", "");
        assert!(host.cluster_fsid().await.is_err());
    }

    #[tokio::test]
    async fn the_fake_keeps_the_environment_a_command_ran_with() {
        let host = FakeHost::new().ok("restic snapshots", "[]");
        host.run_cmd_env(
            "restic",
            &["snapshots", "--no-lock"],
            &[("RESTIC_REPOSITORY", "s3:x/y")],
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(
            host.env_of("restic snapshots"),
            Some(vec![("RESTIC_REPOSITORY".into(), "s3:x/y".into())])
        );
        assert!(
            host.calls().iter().all(|c| !c.contains("s3:x/y")),
            "the environment is never part of the logged command"
        );
    }
}
