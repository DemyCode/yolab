use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use crate::{
    normalise_url, opens_outside, reach, read_remembered, remember, route, Attempts, LaunchOnce,
    Route, SHELL_FLAG_SCRIPT, STORE_FILE, SUPERSEDED,
};

fn store_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir.join(STORE_FILE))
}

fn read_stored(app: &tauri::AppHandle) -> Option<String> {
    match store_path(app).and_then(|path| read_remembered(&path)) {
        Ok(stored) => stored,
        Err(e) => {
            eprintln!("could not read the remembered address: {e}");
            None
        }
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

    remember(&store_path(&app)?, &normalised)?;
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
            if opens_outside(&target) {
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
fn install_icon_for_this_user() {
    let Some(home) = crate::desktop_entry::data_home(
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
    ) else {
        eprintln!("no XDG_DATA_HOME or HOME: the window keeps the generic icon");
        return;
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("could not find this program's path, the window keeps the generic icon: {e}");
            return;
        }
    };
    if let Err(e) = crate::desktop_entry::install(&home, &exe) {
        eprintln!(
            "could not install the YoLab icon under {}, the window keeps the generic icon: {e}",
            home.display()
        );
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    install_icon_for_this_user();

    tauri::Builder::default()
        .manage(LaunchOnce::new())
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .manage(Attempts::new())
        .invoke_handler(tauri::generate_handler![stored_url, launch_url, connect])
        .setup(|app| {
            open_setup_window(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running YoLab");
}
