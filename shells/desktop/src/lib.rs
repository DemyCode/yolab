
use std::sync::Mutex;

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

const STORE_FILE: &str = "server.txt";

struct Stored(Mutex<Option<String>>);

fn store_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(STORE_FILE))
}

fn read_stored(app: &tauri::AppHandle) -> Option<String> {
    let text = std::fs::read_to_string(store_path(app)?).ok()?;
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

#[tauri::command]
fn stored_url(app: tauri::AppHandle) -> Option<String> {
    read_stored(&app)
}

#[tauri::command]
async fn connect(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let normalised = normalise_url(&url)?;

    if let Some(path) = store_path(&app) {
        if let Err(e) = std::fs::write(&path, &normalised) {
            eprintln!("could not remember the address: {e}");
        }
    }
    *app.state::<Stored>().0.lock().unwrap() = Some(normalised.clone());

    open_box_window(&app, &normalised)
}

#[tauri::command]
fn change_server(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.close();
    }
    open_setup_window(&app)
}

fn open_setup_window(app: &tauri::AppHandle) -> Result<(), String> {
    WebviewWindowBuilder::new(app, "setup", WebviewUrl::App("index.html".into()))
        .title("YoLab")
        .inner_size(520.0, 420.0)
        .resizable(false)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn open_box_window(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    WebviewWindowBuilder::new(app, "main", WebviewUrl::External(parsed))
        .title("YoLab")
        .inner_size(1100.0, 780.0)
        .min_inner_size(420.0, 480.0)
        .build()
        .map_err(|e| e.to_string())?;

    if let Some(setup) = app.get_webview_window("setup") {
        let _ = setup.close();
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Stored(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![stored_url, connect, change_server])
        .setup(|app| {
            let handle = app.handle().clone();
            match read_stored(&handle) {
                Some(url) => open_box_window(&handle, &url)?,
                None => open_setup_window(&handle)?,
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running YoLab");
}

#[cfg(test)]
mod tests {
    use super::normalise_url;

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
}
