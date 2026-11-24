use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

mod app;

pub use app::run;

pub(crate) const STORE_FILE: &str = "server.txt";
pub(crate) const SUPERSEDED: &str = "superseded";

pub(crate) const SHELL_FLAG_SCRIPT: &str = "window.__YOLAB_SHELL__ = true;";
const CHANGE_ADDRESS_URL: &str = "yolab-shell://change-address";

pub(crate) struct LaunchOnce(AtomicBool);

impl LaunchOnce {
    pub(crate) fn new() -> Self {
        Self(AtomicBool::new(true))
    }

    pub(crate) fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

pub(crate) struct Attempts(AtomicU64);

impl Attempts {
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub(crate) fn begin(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(crate) fn is_latest(&self, attempt: u64) -> bool {
        self.0.load(Ordering::SeqCst) == attempt
    }
}

pub(crate) fn read_remembered(path: &Path) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let trimmed = text.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

pub(crate) fn remember(path: &Path, url: &str) -> Result<(), String> {
    std::fs::write(path, url)
        .map_err(|e| format!("could not remember the address in {}: {e}", path.display()))
}

pub(crate) fn normalise_url(input: &str) -> Result<String, String> {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Err("Enter the address of your box.".into());
    }

    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };

    let parsed = url::Url::parse(&with_scheme).map_err(|_| "That is not a valid address.")?;

    let host = parsed.host_str().unwrap_or("");
    if host.is_empty() {
        return Err("That address has no host name.".into());
    }
    let is_local = host == "localhost" || host == "127.0.0.1" || host == "[::1]";

    match parsed.scheme() {
        "https" => {}
        "http" if is_local => {}
        "http" => {
            return Err("Use https. Over plain http your sign-in cookie is not sent, so the app would never stay signed in.".into())
        }
        other => return Err(format!("{other}:// is not an address this can open.")),
    }

    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

pub(crate) fn reach(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = parsed.host_str().unwrap_or_default();
    let shown = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let addrs = parsed
        .socket_addrs(|| None)
        .map_err(|e| format!("{shown} can't be found ({e}). Check the address."))?;
    connect_any(&addrs, &shown)
}

fn connect_any(addrs: &[SocketAddr], shown: &str) -> Result<(), String> {
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect(addr) {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    match last {
        None => Err(format!("{shown} can't be found. Check the address.")),
        Some(e) => Err(format!("Nothing answers at {shown} ({e}). Is your box on?")),
    }
}

pub(crate) fn is_change_address(url: &url::Url) -> bool {
    url.as_str() == CHANGE_ADDRESS_URL
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Load,
    ChangeAddress,
    SystemBrowser,
}

pub(crate) fn opens_outside(target: &url::Url) -> bool {
    matches!(target.scheme(), "http" | "https" | "mailto" | "tel")
}

pub(crate) fn route(box_url: &url::Url, target: &url::Url) -> Route {
    if is_change_address(target) {
        return Route::ChangeAddress;
    }
    let same_box =
        matches!(target.scheme(), "http" | "https") && target.origin() == box_url.origin();
    if same_box {
        Route::Load
    } else if opens_outside(target) {
        Route::SystemBrowser
    } else {
        Route::Load
    }
}

#[cfg(target_os = "linux")]
mod desktop_entry {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    pub const APP_ID: &str = "ovh.demycode.yolab";
    const ICON: &[u8] = include_bytes!("../icons/icon.png");

    pub fn data_home(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
        match xdg {
            Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
            _ => home.map(|home| Path::new(&home).join(".local/share")),
        }
    }

    fn icon_dir(data_home: &Path) -> PathBuf {
        data_home.join("icons")
    }

    fn icon_name() -> String {
        format!("{APP_ID}.png")
    }

    fn entry_dir(data_home: &Path) -> PathBuf {
        data_home.join("applications")
    }

    fn entry_name() -> String {
        format!("{APP_ID}.desktop")
    }

    pub fn icon_path(data_home: &Path) -> PathBuf {
        icon_dir(data_home).join(icon_name())
    }

    #[cfg(test)]
    pub fn entry_path(data_home: &Path) -> PathBuf {
        entry_dir(data_home).join(entry_name())
    }

    pub fn entry(exe: &Path, icon: &Path) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=YoLab\nExec=\"{}\"\nIcon={}\nStartupWMClass={APP_ID}\nNoDisplay=true\n",
            exe.display(),
            icon.display()
        )
    }

    fn write_if_changed(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(existing) if existing == bytes => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::create_dir_all(dir)?;
        std::fs::write(path, bytes)
    }

    pub fn install(data_home: &Path, exe: &Path) -> std::io::Result<()> {
        write_if_changed(&icon_dir(data_home), &icon_name(), ICON)?;
        let entry = entry(exe, &icon_path(data_home));
        write_if_changed(&entry_dir(data_home), &entry_name(), entry.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        connect_any, is_change_address, normalise_url, opens_outside, reach, read_remembered,
        remember, route, Attempts, LaunchOnce, Route,
    };
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch(name: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "yolab-desktop-test-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_bare_host_gets_https() {
        assert_eq!(
            normalise_url("node1.5.yolab.io").unwrap(),
            "https://node1.5.yolab.io"
        );
    }

    #[test]
    fn a_trailing_slash_is_dropped_so_the_stored_value_is_stable() {
        assert_eq!(
            normalise_url("https://box.example/").unwrap(),
            "https://box.example"
        );
    }

    #[test]
    fn plain_http_is_refused_with_the_actual_reason() {
        let err = normalise_url("http://box.example").unwrap_err();
        assert!(err.contains("https"), "{err}");
        assert!(err.contains("cookie"), "must say why, not just no: {err}");
    }

    #[test]
    fn http_is_allowed_on_loopback_for_development() {
        for local in [
            "http://localhost:3001",
            "http://127.0.0.1:3001",
            "http://[::1]:3001",
        ] {
            assert!(normalise_url(local).is_ok(), "{local}");
        }
    }

    #[test]
    fn nonsense_is_rejected_rather_than_guessed_at() {
        assert_eq!(
            normalise_url("   ").unwrap_err(),
            "Enter the address of your box."
        );
        assert_eq!(
            normalise_url("https://exa mple").unwrap_err(),
            "That is not a valid address."
        );
        for no_host in ["file:///etc/passwd", "javascript:alert(1)"] {
            assert_eq!(
                normalise_url(no_host).unwrap_err(),
                "That address has no host name.",
                "{no_host}"
            );
        }
        assert_eq!(
            normalise_url("ftp://box.example").unwrap_err(),
            "ftp:// is not an address this can open."
        );
    }

    #[test]
    fn a_name_that_does_not_exist_is_reported_by_name_before_any_window_opens() {
        let err = reach("https://cluster.6.yolab.invalid").unwrap_err();
        assert!(err.contains("cluster.6.yolab.invalid"), "{err}");
        assert!(err.contains("can't be found"), "{err}");
    }

    #[test]
    fn something_that_is_not_an_address_is_not_tried() {
        assert!(reach("not an address").is_err());
    }

    #[test]
    fn a_name_with_no_addresses_is_reported_as_not_found() {
        assert_eq!(
            connect_any(&[], "box.example").unwrap_err(),
            "box.example can't be found. Check the address."
        );
    }

    #[test]
    fn a_box_that_refuses_the_connection_is_reported_as_not_answering() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let err = reach(&format!("http://127.0.0.1:{port}")).unwrap_err();
        assert!(err.contains(&format!("127.0.0.1:{port}")), "{err}");
        assert!(err.contains("Nothing answers"), "{err}");
    }

    #[test]
    fn a_box_that_accepts_the_connection_is_opened() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        reach(&format!("http://127.0.0.1:{port}")).unwrap();
    }

    #[test]
    fn the_remembered_address_is_read_back_trimmed_and_nothing_counts_as_none() {
        let dir = scratch("remembered");
        let file = dir.join("server.txt");
        assert_eq!(read_remembered(&file).unwrap(), None);
        remember(&file, "  https://box.example\n").unwrap();
        assert_eq!(
            read_remembered(&file).unwrap().as_deref(),
            Some("https://box.example")
        );
        remember(&file, " \n").unwrap();
        assert_eq!(read_remembered(&file).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_remembered_address_that_cannot_be_read_or_written_says_where() {
        let dir = scratch("unreadable");
        let err = read_remembered(&dir).unwrap_err();
        assert!(err.contains(&dir.display().to_string()), "{err}");
        let err = remember(&dir, "https://box.example").unwrap_err();
        assert!(err.contains("could not remember the address in"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_the_change_address_link_is_caught_every_other_page_loads() {
        let caught = |u: &str| is_change_address(&url::Url::parse(u).unwrap());
        assert!(caught("yolab-shell://change-address"));
        assert!(!caught("yolab-shell://something-else"));
        assert!(!caught("https://cluster.1.yolab.io/system"));
        assert!(!caught("https://change-address"));
    }

    fn routed(target: &str) -> Route {
        let box_url = url::Url::parse("https://cluster.1.yolab.io").unwrap();
        route(&box_url, &url::Url::parse(target).unwrap())
    }

    #[test]
    fn pages_on_the_box_load_inside_the_app() {
        assert_eq!(routed("https://cluster.1.yolab.io/system"), Route::Load);
        assert_eq!(routed("https://cluster.1.yolab.io:443/add"), Route::Load);
    }

    #[test]
    fn other_websites_open_in_the_system_browser() {
        for other in [
            "https://github.com/transmission/transmission",
            "https://transmission.1.yolab.io/",
            "https://node1.1.yolab.io/",
            "http://cluster.1.yolab.io/",
            "https://cluster.1.yolab.io:8443/",
            "mailto:someone@example.com",
            "tel:+3212345678",
        ] {
            assert_eq!(routed(other), Route::SystemBrowser, "{other}");
        }
    }

    #[test]
    fn the_change_address_link_wins_over_every_other_rule() {
        assert_eq!(routed("yolab-shell://change-address"), Route::ChangeAddress);
    }

    #[test]
    fn internal_webview_urls_are_left_to_the_webview() {
        for internal in [
            "about:blank",
            "data:text/plain,hi",
            "blob:https://cluster.1.yolab.io/x",
        ] {
            assert_eq!(routed(internal), Route::Load, "{internal}");
        }
    }

    #[test]
    fn a_new_window_opens_outside_only_for_web_mail_and_phone_links() {
        let outside = |u: &str| opens_outside(&url::Url::parse(u).unwrap());
        assert!(outside("https://github.com"));
        assert!(outside("http://example.com"));
        assert!(outside("mailto:a@b.c"));
        assert!(outside("tel:123"));
        assert!(!outside("about:blank"));
        assert!(!outside("file:///etc/passwd"));
    }

    #[test]
    fn the_opener_plugin_injects_no_click_script_that_the_remote_box_page_cannot_use() {
        let source = include_str!("app.rs");
        assert!(source.contains(".open_js_links_on_click(false)"));
        assert!(!source.contains(concat!("tauri_plugin_opener::", "init()")));
    }

    #[test]
    fn the_remembered_address_auto_connects_only_at_launch_not_after_change_address() {
        let once = LaunchOnce::new();
        assert!(once.take());
        assert!(!once.take());
    }

    #[test]
    fn a_slow_earlier_connect_never_overrides_the_address_typed_after_it() {
        let attempts = Attempts::new();
        let stale = attempts.begin();
        let typed = attempts.begin();
        assert!(!attempts.is_latest(stale));
        assert!(attempts.is_latest(typed));
    }

    #[cfg(target_os = "linux")]
    mod linux_icon {
        use super::super::desktop_entry::{data_home, entry_path, icon_path, install, APP_ID};
        use super::scratch;
        use std::ffi::OsString;
        use std::path::{Path, PathBuf};

        #[test]
        fn the_wayland_app_id_matches_the_desktop_file_the_compositor_looks_up() {
            let conf: serde_json::Value =
                serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
            assert_eq!(conf["identifier"], APP_ID);
            assert_eq!(conf["app"]["enableGTKAppId"], true);
            assert!(entry_path(Path::new("/d")).ends_with(format!("applications/{APP_ID}.desktop")));
        }

        #[test]
        fn the_icon_goes_under_xdg_data_home_else_under_home() {
            let os = |s: &str| Some(OsString::from(s));
            assert_eq!(data_home(os("/x"), os("/h")), Some(PathBuf::from("/x")));
            assert_eq!(
                data_home(os(""), os("/h")),
                Some(PathBuf::from("/h/.local/share"))
            );
            assert_eq!(
                data_home(None, os("/h")),
                Some(PathBuf::from("/h/.local/share"))
            );
            assert_eq!(data_home(None, None), None);
        }

        #[test]
        fn install_writes_the_logo_and_an_entry_pointing_at_it_and_this_program() {
            let home = scratch("install");
            install(&home, Path::new("/nix/store/x/bin/yolab-desktop")).unwrap();
            let icon = std::fs::read(icon_path(&home)).unwrap();
            assert_eq!(icon, include_bytes!("../icons/icon.png"));
            let entry = std::fs::read_to_string(entry_path(&home)).unwrap();
            assert!(
                entry.contains("Exec=\"/nix/store/x/bin/yolab-desktop\""),
                "{entry}"
            );
            assert!(
                entry.contains(&format!("Icon={}", icon_path(&home).display())),
                "{entry}"
            );
            assert!(
                entry.contains("NoDisplay=true"),
                "must not add a menu entry: {entry}"
            );
            std::fs::remove_dir_all(&home).unwrap();
        }

        #[test]
        fn a_moved_program_rewrites_the_entry_on_next_launch() {
            let home = scratch("moved");
            install(&home, Path::new("/old/yolab-desktop")).unwrap();
            install(&home, Path::new("/new/yolab-desktop")).unwrap();
            let entry = std::fs::read_to_string(entry_path(&home)).unwrap();
            assert!(entry.contains("/new/yolab-desktop"), "{entry}");
            assert!(!entry.contains("/old/yolab-desktop"), "{entry}");
            std::fs::remove_dir_all(&home).unwrap();
        }

        #[test]
        fn an_icon_that_cannot_be_read_is_reported_instead_of_overwritten() {
            let home = scratch("blocked");
            std::fs::create_dir_all(icon_path(&home)).unwrap();
            assert!(install(&home, Path::new("/bin/yolab-desktop")).is_err());
            assert!(!entry_path(&home).exists());
            std::fs::remove_dir_all(&home).unwrap();
        }
    }
}
