// The runtime stub: installs the archive appended to its own exe (docs/spec.md § 3).
// No arguments: the installer window. --silent: install with no window (§ 6). --uninstall: run by the
// uninstaller copy that Installed Apps starts.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod install;
mod registration;

use serde::Serialize;
use silk_archive::Archive;
use silk_archive::config::Config;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

struct Installer {
    config: Config,
    archive: Mutex<Archive>,
    target: PathBuf,
    installed: Option<String>,
    cancel: AtomicBool,
}

#[derive(Serialize, Clone)]
struct Progress<'a> {
    done: usize,
    total: usize,
    file: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Info {
    product: String,
    version: String,
    publisher: String,
    target: String,
    files: usize,
    launch_at_finish: bool,
}

#[tauri::command]
fn info(st: State<Installer>) -> Info {
    let c = &st.config;
    Info {
        product: c.product.clone(),
        version: c.version.clone(),
        publisher: c.publisher.clone(),
        target: st.target.display().to_string(),
        files: st.archive.lock().unwrap().payload_len(),
        launch_at_finish: c.launch_at_finish,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionInfo {
    installed: Option<String>,
    already_current: bool,
}

/// Lets the page tell "install" from "already installed, reinstall?"; the compare stays in Rust.
#[tauri::command]
fn check_version(st: State<Installer>) -> VersionInfo {
    let installed = st.installed.clone();
    let already_current = install::already_current(installed.as_deref(), &st.config.version);
    VersionInfo { installed, already_current }
}

/// Installs on a background thread; the page hears `progress`, then `finished` or `failed`.
#[tauri::command]
fn start_install(app: AppHandle) {
    std::thread::spawn(move || {
        let st = app.state::<Installer>();
        st.cancel.store(false, Ordering::SeqCst);
        let result = install::install(&mut st.archive.lock().unwrap(), &st.config, &st.target, |done, total, file| {
            let _ = app.emit("progress", Progress { done, total, file });
            !st.cancel.load(Ordering::SeqCst)
        });
        let _ = match result {
            Ok(()) => app.emit("finished", ()),
            Err(e) => app.emit("failed", e),
        };
    });
}

#[tauri::command]
fn cancel_install(st: State<Installer>) {
    st.cancel.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn launch(st: State<Installer>) -> Result<(), String> {
    install::launch(&st.target, &st.config.main_exe).map_err(|e| e.to_string())
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
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
    let opened = std::env::current_exe().and_then(|exe| {
        let mut archive = Archive::open(&exe)?.ok_or_else(|| io::Error::other("This installer carries no files."))?;
        let config = archive.config()?;
        Ok((archive, config))
    });
    let (mut archive, config) = opened.unwrap_or_else(|e| fail(silent, e));
    // The uninstaller carries the config only: run without --uninstall it has nothing to install.
    if !uninstall && archive.payload_len() == 0 {
        fail(silent, format!("This is the uninstaller of {0}. Remove {0} from Settings > Apps > Installed apps.", config.product));
    }

    // The existing install is found through its Installed Apps entry, wherever it went; it counts as
    // installed only while its main exe is still there.
    let (location, version) = registration::installed(&config.product);
    if uninstall {
        let target = location.ok_or_else(|| format!("{} is not installed.", config.product)).unwrap_or_else(|e| fail(silent, e));
        install::uninstall(&config, &target).unwrap_or_else(|e| fail(silent, e));
        std::process::exit(0);
    }
    let target = location.map_or_else(|| config.target_dir(), Ok).unwrap_or_else(|e| fail(silent, e));
    let installed = version.filter(|_| target.join(&config.main_exe).is_file());

    // Checked before Tauri builds anything, so a silent run never creates a window.
    if silent {
        std::process::exit(if install::silent(&mut archive, &config, &target, installed.as_deref()) { 0 } else { 1 });
    }

    let title = format!("{} Setup", config.product);
    let (width, height) = (config.window.width as f64, config.window.height as f64);
    tauri::Builder::default()
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title(title)
                .inner_size(width, height)
                .resizable(false)
                .maximizable(false)
                .center()
                .build()?;
            Ok(())
        })
        .manage(Installer { config, archive: Mutex::new(archive), target, installed, cancel: AtomicBool::new(false) })
        .invoke_handler(tauri::generate_handler![info, check_version, start_install, cancel_install, launch, quit])
        .run(tauri::generate_context!())
        .expect("error while running the installer");
}
