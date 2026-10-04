// The runtime stub: installs the archive appended to its own exe (docs/spec.md § 3).
// No arguments: the installer window. --silent: install with no window (§ 6). --uninstall: run by the
// uninstaller copy that Installed Apps starts: the same theme in uninstall mode, or no window with --silent.
// A per-machine installer or uninstaller first gets administrator rights (elevation.rs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod elevation;
mod install;
mod registration;
mod theme;

use serde::Serialize;
use silk_archive::Archive;
use silk_archive::config::{Config, Scope};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};

struct Installer {
    config: Config,
    archive: Mutex<Archive>,
    target: PathBuf,
    cancel: AtomicBool,
    uninstall: bool,
    /// Set once an uninstall has succeeded: the folder, uninstaller included, goes when the window closes.
    removed: AtomicBool,
    launch: Launch,
    /// Set when the theme asked to launch and `launch` is ByParent: the exit code passes it on.
    launch_requested: AtomicBool,
}

/// Who starts the app at finish. An elevated run must not (the app would run as administrator), so the
/// copy that `elevation::relaunch` started leaves it to its unelevated parent. A per-machine run elevated
/// some other way (an IT tool, Run as administrator) has no such parent and does not start it.
#[derive(Clone, Copy, PartialEq)]
enum Launch {
    Here,
    ByParent,
    Never,
}

/// What `silk.info` holds: fixed for the life of the window, so it goes into the page with the API.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Info<'a> {
    /// "install" or "uninstall": one theme serves both.
    mode: &'a str,
    product: &'a str,
    version: &'a str,
    publisher: &'a str,
    /// "user" or "machine", so a theme can say who it installs for.
    scope: &'a Scope,
    target: String,
    installed: Option<&'a str>,
    already_current: bool,
    launch_at_finish: bool,
    frame: bool,
    files: usize,
    size: u64,
}

/// The theme JS API (`window.silk`, docs/theme-api.md) with `info` filled in, run before any theme script.
fn api_script(info: &Info) -> String {
    include_str!("api.js").replace("/*INFO*/", &serde_json::to_string(info).unwrap())
}

/// Installs (or uninstalls, in uninstall mode) on a blocking thread, emitting `silk:progress`; resolves
/// when done, rejects with the message (or `cancelled`). A second call while one runs is refused.
#[tauri::command]
async fn start(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<Installer>();
        let mut archive = st.archive.try_lock().map_err(|_| "Setup is already running.".to_string())?;
        st.cancel.store(false, Ordering::SeqCst);
        let emit = |p: &install::Progress| {
            let _ = app.emit("silk:progress", p);
            !st.cancel.load(Ordering::SeqCst)
        };
        if st.uninstall {
            install::uninstall(&st.config, &st.target, |p| {
                emit(p);
            })?;
            st.removed.store(true, Ordering::SeqCst);
            return Ok(());
        }
        install::install(&mut archive, &st.config, &st.target, emit)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn cancel(st: State<Installer>) {
    st.cancel.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn launch(st: State<Installer>) -> Result<(), String> {
    match st.launch {
        Launch::Here => install::launch(&st.target, &st.config.main_exe).map_err(|e| e.to_string()),
        // The parent starts it once this process exits (the run loop below).
        Launch::ByParent => {
            st.launch_requested.store(true, Ordering::SeqCst);
            Ok(())
        }
        Launch::Never => {
            Err(format!("Setup ran as administrator, so it leaves starting {} to you.", st.config.product))
        }
    }
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    // SAFETY: a null owner is allowed, and both strings are NUL-terminated UTF-16 that live until the call returns.
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("Setup").as_ptr(), MB_OK | MB_ICONERROR) };
}

fn fail(silent: bool, e: impl std::fmt::Display) -> ! {
    // A silent run never shows a window, not even this one.
    if !silent {
        message_box(&e.to_string());
    }
    std::process::exit(1);
}

fn main() {
    let has = |flag: &str| std::env::args().skip(1).any(|a| a == flag);
    let (silent, uninstall) = (has("--silent"), has("--uninstall"));
    let exe = std::env::current_exe().unwrap_or_else(|e| fail(silent, e));
    let opened = Archive::open(&exe).and_then(|archive| {
        let mut archive = archive.ok_or_else(|| io::Error::other("This installer carries no files."))?;
        let config = archive.config()?;
        Ok((archive, config))
    });
    let (mut archive, config) = opened.unwrap_or_else(|e| fail(silent, e));
    // The uninstaller carries the config only: run without --uninstall it has nothing to install.
    if !uninstall && archive.payload_len() == 0 {
        fail(
            silent,
            format!(
                "This is the uninstaller of {0}. Remove {0} from Settings > Apps > Installed apps.",
                config.product
            ),
        );
    }

    // The existing install is found through its Installed Apps entry, wherever it went; it counts as
    // installed only while its main exe is still there.
    let (location, version) = registration::installed(&config);
    let target = match location {
        Some(location) => location,
        None if uninstall => fail(silent, format!("{} is not installed.", config.product)),
        None => registration::target_dir(&config).unwrap_or_else(|e| fail(silent, e)),
    };
    let installed = version.filter(|_| target.join(&config.main_exe).is_file());

    // Per-machine without administrator rights: run again elevated and pass on its exit code. The copy
    // installs into the same target: the existing install's, or the config's default.
    if config.scope == Scope::Machine && !elevation::is_admin() {
        if has(elevation::FLAG) {
            fail(silent, format!("{} is installed for all users, which needs administrator rights.", config.product));
        }
        let args: Vec<&str> =
            [("--silent", silent), ("--uninstall", uninstall)].into_iter().filter(|a| a.1).map(|a| a.0).collect();
        let code = match elevation::relaunch(&args).unwrap_or_else(|e| fail(silent, e)) {
            Some(elevation::LAUNCH) => {
                let _ = install::launch(&target, &config.main_exe);
                0
            }
            Some(code) => code as i32,
            // The user said no to UAC: nothing changed. Logged only where this user may write.
            None => {
                if silent {
                    install::log_line(&target, "silent: administrator rights declined, nothing changed");
                }
                1
            }
        };
        std::process::exit(code);
    }
    let launcher = match config.scope {
        Scope::User => Launch::Here,
        Scope::Machine if has(elevation::FLAG) => Launch::ByParent,
        Scope::Machine => Launch::Never,
    };

    // Checked before Tauri builds anything, so a silent run never creates a window.
    if silent && uninstall {
        install::uninstall(&config, &target, |_| {}).unwrap_or_else(|e| fail(silent, e));
        std::process::exit(if registration::delete_later(&target).is_ok() { 0 } else { 1 });
    }
    if silent {
        let mut code = 0;
        let ok = install::silent(&mut archive, &config, &target, installed.as_deref(), || match launcher {
            Launch::Here => drop(install::launch(&target, &config.main_exe)),
            Launch::ByParent => code = elevation::LAUNCH as i32,
            Launch::Never => {}
        });
        std::process::exit(if ok { code } else { 1 });
    }

    let info = Info {
        mode: if uninstall { "uninstall" } else { "install" },
        product: &config.product,
        version: &config.version,
        publisher: &config.publisher,
        scope: &config.scope,
        target: target.display().to_string(),
        installed: installed.as_deref(),
        already_current: install::already_current(installed.as_deref(), &config.version),
        launch_at_finish: config.launch_at_finish && launcher != Launch::Never,
        frame: config.window.frame,
        files: archive.payload_len(),
        size: archive.payload_size(),
    };
    let api = api_script(&info);
    // A second handle for serving the theme, so pages load while an install holds the first.
    let theme = theme::Theme(archive.has_theme().then(|| {
        let second =
            Archive::open(&exe).ok().flatten().unwrap_or_else(|| fail(false, "Setup could not reread itself."));
        Mutex::new(second)
    }));
    let title = if uninstall { format!("Uninstall {}", config.product) } else { format!("{} Setup", config.product) };
    let w = &config.window;
    let (width, height, frame, minimize, close) = (w.width as f64, w.height as f64, w.frame, w.minimize, w.close);
    tauri::Builder::default()
        .register_uri_scheme_protocol("silk", |ctx, request| {
            theme::respond(&ctx.app_handle().state(), request.uri().path())
        })
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::CustomProtocol(theme::URL.parse().unwrap()))
                .title(title)
                .inner_size(width, height)
                .resizable(false)
                .maximizable(false)
                .decorations(frame)
                .minimizable(minimize)
                .closable(close)
                .center()
                .initialization_script(api)
                // The theme's own pages only: a link to the web must not load it inside the installer.
                .on_navigation(|url| url.as_str().starts_with(theme::URL))
                .build()?;
            Ok(())
        })
        .manage(Installer {
            config,
            archive: Mutex::new(archive),
            target,
            cancel: AtomicBool::new(false),
            uninstall,
            removed: AtomicBool::new(false),
            launch: launcher,
            launch_requested: AtomicBool::new(false),
        })
        .manage(theme)
        .invoke_handler(tauri::generate_handler![start, cancel, launch, quit])
        .build(tauri::generate_context!())
        .expect("error while running the installer")
        .run(|app, event| {
            if !matches!(event, RunEvent::Exit) {
                return;
            }
            // Only now, as the process ends: the uninstaller cannot remove itself while it runs.
            let st = app.state::<Installer>();
            if st.removed.load(Ordering::SeqCst) {
                let _ = registration::delete_later(&st.target);
            }
            if st.launch_requested.load(Ordering::SeqCst) {
                std::process::exit(elevation::LAUNCH as i32);
            }
        });
}
