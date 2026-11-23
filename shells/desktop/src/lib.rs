use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

const STORE_FILE: &str = "server.txt";
const SUPERSEDED: &str = "superseded";

const SHELL_FLAG_SCRIPT: &str = "window.__YOLAB_SHELL__ = true;";
const CHANGE_ADDRESS_URL: &str = "yolab-shell://change-address";

struct LaunchOnce(AtomicBool);

impl LaunchOnce {
    fn new() -> Self {
        Self(AtomicBool::new(true))
    }

    fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

struct Attempts(AtomicU64);

impl Attempts {
    fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    fn begin(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn is_latest(&self, attempt: u64) -> bool {
        self.0.load(Ordering::SeqCst) == attempt
    }
}

fn store_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir.join(STORE_FILE))
}

fn read_stored(app: &tauri::AppHandle) -> Option<String> {
    let path = match store_path(app) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("could not find where the address is remembered: {e}");
            return None;
        }
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "could not read the remembered address from {}: {e}",
                path.display()
            );
            return None;
        }
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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
    let is_local = host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1";

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
    let mut last = None;
    for addr in &addrs {
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

#[tauri::command]
fn stored_url(app: tauri::AppHandle) -> Option<String> {
    read_stored(&app)
}

#[tauri::command]
fn launch_url(app: tauri::AppHandle) -> Option<String> {
    if app.state::<LaunchOnce>().take() {
        read_stored(&app)
    } else {
        None
    }
}

#[tauri::command]
async fn connect(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let normalised = normalise_url(&url)?;
    let attempt = app.state::<Attempts>().begin();

    let target = normalised.clone();
    tauri::async_runtime::spawn_blocking(move || reach(&target))
        .await
        .map_err(|e| e.to_string())??;

    if !app.state::<Attempts>().is_latest(attempt) {
        return Err(SUPERSEDED.into());
    }

    let path = store_path(&app)?;
    std::fs::write(&path, &normalised)
        .map_err(|e| format!("could not remember the address in {}: {e}", path.display()))?;

    open_box_window(&app, &normalised)
}

fn open_setup_window(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(setup) = app.get_webview_window("setup") {
        return setup.set_focus().map_err(|e| e.to_string());
    }
    WebviewWindowBuilder::new(app, "setup", WebviewUrl::App("index.html".into()))
        .title("YoLab")
        .inner_size(520.0, 420.0)
        .resizable(false)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
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

pub(crate) fn route(box_url: &url::Url, target: &url::Url) -> Route {
    if is_change_address(target) {
        return Route::ChangeAddress;
    }
    match target.scheme() {
        "http" | "https" if target.origin() == box_url.origin() => Route::Load,
        "http" | "https" | "mailto" | "tel" => Route::SystemBrowser,
        _ => Route::Load,
    }
}

fn open_in_system_browser(app: &tauri::AppHandle, target: &url::Url) {
    use tauri_plugin_opener::OpenerExt;
    if let Err(e) = app.opener().open_url(target.as_str(), None::<&str>) {
        eprintln!("could not open {target} in the system browser: {e}");
    }
}

fn go_back_to_address_screen(app: &tauri::AppHandle) {
    let handle = app.clone();
    let queued = app.run_on_main_thread(move || {
        if let Err(e) = change_address(&handle) {
            eprintln!("could not go back to the address screen: {e}");
        }
    });
    if let Err(e) = queued {
        eprintln!("could not go back to the address screen: {e}");
    }
}

fn change_address(app: &tauri::AppHandle) -> Result<(), String> {
    open_setup_window(app)?;
    if let Some(w) = app.get_webview_window("main") {
        w.destroy().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn open_box_window(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    let box_url = url::Url::parse(url).map_err(|e| e.to_string())?;
    let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(box_url.clone()))
        .title("YoLab")
        .inner_size(1100.0, 780.0)
        .min_inner_size(420.0, 480.0)
        .initialization_script(SHELL_FLAG_SCRIPT);

    let handle = app.clone();
    let builder = builder.on_navigation(move |target| match route(&box_url, target) {
        Route::Load => true,
        Route::ChangeAddress => {
            go_back_to_address_screen(&handle);
            false
        }
        Route::SystemBrowser => {
            open_in_system_browser(&handle, target);
            false
        }
    });

    #[cfg(desktop)]
    let builder = {
        let handle = app.clone();
        builder.on_new_window(move |target, _features| {
            if matches!(target.scheme(), "http" | "https" | "mailto" | "tel") {
                open_in_system_browser(&handle, &target);
            }
            tauri::webview::NewWindowResponse::Deny
        })
    };

    builder.build().map_err(|e| e.to_string())?;

    if let Some(setup) = app.get_webview_window("setup") {
        setup.destroy().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod desktop_entry {
    use std::path::{Path, PathBuf};

    pub const APP_ID: &str = "ovh.demycode.yolab";
    const ICON: &[u8] = include_bytes!("../icons/icon.png");

    pub fn data_home() -> Option<PathBuf> {
        match std::env::var_os("XDG_DATA_HOME") {
            Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
            _ => std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")),
        }
    }

    pub fn icon_path(data_home: &Path) -> PathBuf {
        data_home.join("icons").join(format!("{APP_ID}.png"))
    }

    pub fn entry_path(data_home: &Path) -> PathBuf {
        data_home
            .join("applications")
            .join(format!("{APP_ID}.desktop"))
    }

    pub fn entry(exe: &Path, icon: &Path) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=YoLab\nExec=\"{}\"\nIcon={}\nStartupWMClass={APP_ID}\nNoDisplay=true\n",
            exe.display(),
            icon.display()
        )
    }

    fn write_if_changed(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        match std::fs::read(path) {
            Ok(existing) if existing == bytes => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, bytes)
    }

    pub fn install(data_home: &Path, exe: &Path) -> std::io::Result<()> {
        let icon = icon_path(data_home);
        write_if_changed(&icon, ICON)?;
        write_if_changed(&entry_path(data_home), entry(exe, &icon).as_bytes())
    }

    pub fn install_for_this_user() {
        let Some(home) = data_home() else {
            eprintln!("no XDG_DATA_HOME or HOME: the window keeps the generic icon");
            return;
        };
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                eprintln!(
                    "could not find this program's path, the window keeps the generic icon: {e}"
                );
                return;
            }
        };
        if let Err(e) = install(&home, &exe) {
            eprintln!(
                "could not install the YoLab icon under {}, the window keeps the generic icon: {e}",
                home.display()
            );
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    desktop_entry::install_for_this_user();

    let builder = tauri::Builder::default()
        .manage(LaunchOnce::new())
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .manage(Attempts::new())
        .invoke_handler(tauri::generate_handler![stored_url, launch_url, connect]);

    builder
        .setup(|app| {
            open_setup_window(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running YoLab");
}

#[cfg(test)]
mod tests {
    use super::{is_change_address, normalise_url, reach, route, Attempts, LaunchOnce, Route};
    use std::net::TcpListener;

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
        for local in ["http://localhost:3001", "http://127.0.0.1:3001"] {
            assert!(normalise_url(local).is_ok(), "{local}");
        }
    }

    #[test]
    fn nonsense_is_rejected_rather_than_guessed_at() {
        for bad in ["", "   ", "file:///etc/passwd", "javascript:alert(1)"] {
            assert!(normalise_url(bad).is_err(), "{bad:?} must not be opened");
        }
    }

    #[test]
    fn a_name_that_does_not_exist_is_reported_by_name_before_any_window_opens() {
        let err = reach("https://cluster.6.yolab.invalid").unwrap_err();
        assert!(err.contains("cluster.6.yolab.invalid"), "{err}");
        assert!(err.contains("can't be found"), "{err}");
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
        for internal in ["about:blank", "data:text/plain,hi", "blob:https://cluster.1.yolab.io/x"] {
            assert_eq!(routed(internal), Route::Load, "{internal}");
        }
    }

    #[test]
    fn the_opener_plugin_injects_no_click_script_that_the_remote_box_page_cannot_use() {
        let source = include_str!("lib.rs");
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
        use super::super::desktop_entry::{entry_path, icon_path, install, APP_ID};
        use std::path::Path;

        fn scratch(name: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir()
                .join(format!("yolab-desktop-test-{name}-{}", std::process::id()));
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("{}: {e}", dir.display()),
            }
            dir
        }

        #[test]
        fn the_wayland_app_id_matches_the_desktop_file_the_compositor_looks_up() {
            let conf: serde_json::Value =
                serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
            assert_eq!(conf["identifier"], APP_ID);
            assert_eq!(conf["app"]["enableGTKAppId"], true);
            assert!(entry_path(Path::new("/d")).ends_with(format!("applications/{APP_ID}.desktop")));
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
    }
}
