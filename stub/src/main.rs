// The runtime stub: installs the archive appended to its own exe (docs/spec.md § 3).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
    cancel: AtomicBool,
}

#[derive(Serialize, Clone)]
struct Progress<'a> {
    done: usize,
    total: usize,
    file: &'a str,
}

#[derive(Serialize)]
struct Info {
    product: String,
    version: String,
    publisher: String,
    target: String,
    files: usize,
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
    }
}

/// Extracts on a background thread; the page hears `progress`, then `finished` or `failed`.
#[tauri::command]
fn start_install(app: AppHandle) {
    std::thread::spawn(move || {
        let st = app.state::<Installer>();
        st.cancel.store(false, Ordering::SeqCst);
        let result = st.archive.lock().unwrap().extract_payload(&st.target, |done, total, file| {
            let _ = app.emit("progress", Progress { done, total, file });
            !st.cancel.load(Ordering::SeqCst)
        });
        let _ = match result {
            Ok(()) => app.emit("finished", ()),
            // ponytail: a cancel leaves the files copied so far; staging + rollback come with § 5 actions.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => app.emit("failed", "cancelled"),
            Err(e) => app.emit("failed", e.to_string()),
        };
    });
}

#[tauri::command]
fn cancel_install(st: State<Installer>) {
    st.cancel.store(true, Ordering::SeqCst);
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

fn main() {
    let silent = std::env::args().skip(1).any(|a| a == "--silent");
    let opened = std::env::current_exe().and_then(|exe| {
        let mut archive = Archive::open(&exe)?.ok_or_else(|| io::Error::other("This installer carries no files."))?;
        let config = archive.config()?;
        let target = config.target_dir().map_err(io::Error::other)?;
        Ok((archive, config, target))
    });
    let (mut archive, config, target) = match opened {
        Ok(v) => v,
        Err(e) => {
            // A silent run never shows a window, not even this one.
            if !silent {
                message_box(&e.to_string());
            }
            std::process::exit(1);
        }
    };

    // Checked before Tauri builds anything, so a silent run never creates a window.
    // ponytail: no version check and no install.log yet (spec § 6); step 2 part B adds them.
    if silent {
        std::process::exit(archive.extract_payload(&target, |_, _, _| true).map_or(1, |_| 0));
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
        .manage(Installer { config, archive: Mutex::new(archive), target, cancel: AtomicBool::new(false) })
        .invoke_handler(tauri::generate_handler![info, start_install, cancel_install, quit])
        .run(tauri::generate_context!())
        .expect("error while running the installer");
}
