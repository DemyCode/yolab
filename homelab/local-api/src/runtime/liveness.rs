use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::time::Instant;

pub const SLACK: Duration = Duration::from_secs(60);

const UNARMED_PERIOD: Duration = Duration::from_secs(20);

const FAR: Duration = Duration::from_secs(365 * 24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Healthy,
    Unhealthy(String),
}

#[derive(Default)]
pub struct Liveness {
    due: Mutex<BTreeMap<&'static str, Instant>>,
    wedged: Mutex<Option<String>>,
}

impl Liveness {
    pub fn expect(&self, name: &'static str, within: Duration) {
        let now = Instant::now();
        let by = now
            .checked_add(within.saturating_add(SLACK))
            .unwrap_or(now + FAR);
        self.due
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name, by);
    }

    pub fn wedge(&self, why: String) {
        let mut w = self
            .wedged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if w.is_none() {
            *w = Some(why);
        }
    }

    pub fn verdict(&self, now: Instant) -> Verdict {
        if let Some(why) = self
            .wedged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Verdict::Unhealthy(why);
        }
        let due = self
            .due
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let late: Vec<String> = due
            .iter()
            .filter(|(_, by)| now > **by)
            .map(|(name, by)| format!("{name} ({}s late)", (now - *by).as_secs()))
            .collect();
        if late.is_empty() {
            Verdict::Healthy
        } else {
            Verdict::Unhealthy(format!(
                "these loops stopped reporting: {}",
                late.join(", ")
            ))
        }
    }
}

pub fn global() -> &'static Liveness {
    static L: OnceLock<Liveness> = OnceLock::new();
    L.get_or_init(Liveness::default)
}

pub fn expect(name: &'static str, within: Duration) {
    global().expect(name, within);
}

pub fn wedge(why: String) {
    tracing::error!("liveness: {why}");
    global().wedge(why);
}

pub fn verdict() -> Verdict {
    global().verdict(Instant::now())
}

pub fn notify(msg: &str) -> std::io::Result<bool> {
    match std::env::var_os("NOTIFY_SOCKET") {
        Some(path) => notify_to(&path, msg).map(|()| true),
        None => Ok(false),
    }
}

fn notify_to(path: &OsStr, msg: &str) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let bytes = path.as_bytes();
    let addr = match bytes.strip_prefix(b"@") {
        Some(name) => {
            use std::os::linux::net::SocketAddrExt;
            SocketAddr::from_abstract_name(name)?
        }
        None => SocketAddr::from_pathname(std::path::Path::new(path))?,
    };
    UnixDatagram::unbound()?.send_to_addr(msg.as_bytes(), &addr)?;
    Ok(())
}

fn ping_period(watchdog_usec: Option<&str>) -> Result<Option<Duration>, String> {
    let Some(raw) = watchdog_usec else {
        return Ok(None);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => Ok(None),
        Ok(usec) => Ok(Some(Duration::from_micros(usec) / 3)),
        Err(e) => Err(format!("WATCHDOG_USEC={raw:?} is not a number: {e}")),
    }
}

pub fn start() {
    match notify("READY=1") {
        Ok(true) => tracing::info!("liveness: told systemd this service is ready"),
        Ok(false) => tracing::info!("liveness: not started by systemd with a notify socket"),
        Err(e) => tracing::error!("liveness: could not tell systemd this service is ready: {e}"),
    }
    let usec = std::env::var("WATCHDOG_USEC").ok();
    let armed = match ping_period(usec.as_deref()) {
        Ok(p) => p,
        Err(why) => {
            tracing::error!("liveness: {why}; the systemd watchdog is not pinged");
            None
        }
    };
    if armed.is_none() {
        tracing::warn!(
            "liveness: no systemd watchdog is armed, so a stuck loop is logged but never restarted"
        );
    }
    tokio::spawn(async move {
        let mut reported: Option<String> = None;
        loop {
            match verdict() {
                Verdict::Healthy => {
                    if reported.take().is_some() {
                        tracing::info!("liveness: every loop is reporting again");
                    }
                    if armed.is_some() {
                        if let Err(e) = notify("WATCHDOG=1") {
                            tracing::warn!("liveness: could not ping the systemd watchdog: {e}");
                        }
                    }
                }
                Verdict::Unhealthy(why) => {
                    if reported.as_deref() != Some(why.as_str()) {
                        tracing::error!(
                            "liveness: {why} — the systemd watchdog is no longer pinged, so \
                             this service is restarted"
                        );
                        if let Err(e) = notify(&format!("STATUS={why}")) {
                            tracing::warn!("liveness: could not tell systemd why: {e}");
                        }
                        reported = Some(why);
                    }
                }
            }
            tokio::time::sleep(armed.unwrap_or(UNARMED_PERIOD)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_loop_that_keeps_its_promise_is_healthy() {
        let l = Liveness::default();
        l.expect("disks", Duration::from_secs(30));
        tokio::time::advance(Duration::from_secs(30) + SLACK).await;
        assert_eq!(l.verdict(Instant::now()), Verdict::Healthy);
    }

    #[tokio::test(start_paused = true)]
    async fn a_loop_that_misses_its_promise_is_named() {
        let l = Liveness::default();
        l.expect("disks", Duration::from_secs(30));
        l.expect("leader", Duration::from_secs(3600));
        tokio::time::advance(Duration::from_secs(31) + SLACK).await;
        match l.verdict(Instant::now()) {
            Verdict::Unhealthy(why) => {
                assert!(why.contains("disks"), "{why}");
                assert!(!why.contains("leader"), "{why}");
            }
            Verdict::Healthy => panic!("a late loop went unnoticed"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_fresh_promise_replaces_the_old_one() {
        let l = Liveness::default();
        l.expect("heal", Duration::from_secs(10));
        tokio::time::advance(Duration::from_secs(5)).await;
        l.expect("heal", Duration::from_secs(600));
        tokio::time::advance(Duration::from_secs(300)).await;
        assert_eq!(l.verdict(Instant::now()), Verdict::Healthy);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wedged_loop_stays_unhealthy_however_well_the_others_report() {
        let l = Liveness::default();
        l.wedge("containerd-store did not stop after it was cancelled".into());
        l.wedge("a second reason".into());
        l.expect("disks", Duration::from_secs(30));
        match l.verdict(Instant::now()) {
            Verdict::Unhealthy(why) => assert!(why.contains("containerd-store"), "{why}"),
            Verdict::Healthy => panic!("a wedged loop was forgiven"),
        }
    }

    #[test]
    fn a_promise_far_in_the_future_does_not_overflow() {
        let l = Liveness::default();
        l.expect("backup-check", Duration::MAX);
        assert_eq!(l.verdict(Instant::now()), Verdict::Healthy);
    }

    #[test]
    fn the_watchdog_is_pinged_three_times_per_period() {
        assert_eq!(ping_period(None), Ok(None));
        assert_eq!(ping_period(Some("0")), Ok(None));
        assert_eq!(
            ping_period(Some("120000000")),
            Ok(Some(Duration::from_secs(40)))
        );
        assert!(ping_period(Some("soon")).is_err());
    }

    #[test]
    fn a_message_reaches_a_socket_named_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let server = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        notify_to(path.as_os_str(), "WATCHDOG=1").unwrap();
        let mut buf = [0u8; 64];
        let n = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"WATCHDOG=1");
    }

    #[test]
    fn a_message_reaches_an_abstract_socket() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixDatagram};
        let name = format!("yolab-liveness-test-{}", std::process::id());
        let server =
            UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
                .unwrap();
        notify_to(OsStr::new(&format!("@{name}")), "READY=1").unwrap();
        let mut buf = [0u8; 64];
        let n = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
    }
}
