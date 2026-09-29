// The runtime stub: installs the archive appended to its own exe (docs/spec.md § 3).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;
use silk_archive::Archive;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager, State};

struct Installer {
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
    target: String,
    files: usize,
}

#[tauri::command]
fn info(st: State<Installer>) -> Info {
    Info { target: st.target.display().to_string(), files: st.archive.lock().unwrap().payload_len() }
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

// ponytail: step 1 installs per-user into %LOCALAPPDATA%\Programs\<exe stem>; the TOML config (step 2) replaces it.
fn target_dir(exe: &Path) -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?;
    let stem = exe.file_stem().ok_or_else(|| io::Error::other("installer has no file name"))?;
    Ok(PathBuf::from(base).join("Programs").join(stem))
}

fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("Setup").as_ptr(), MB_OK | MB_ICONERROR) };
}

fn main() {
    let silent = std::env::args().skip(1).any(|a| a == "--silent");
    let opened = std::env::current_exe().and_then(|exe| {
        let archive = Archive::open(&exe)?.ok_or_else(|| io::Error::other("This installer carries no files."))?;
        Ok((archive, target_dir(&exe)?))
    });
    let (mut archive, target) = match opened {
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
    // ponytail: no version check and no install.log yet (spec § 6); both come with the config.
    if silent {
        std::process::exit(archive.extract_payload(&target, |_, _, _| true).map_or(1, |_| 0));
    }

    tauri::Builder::default()
        .manage(Installer { archive: Mutex::new(archive), target, cancel: AtomicBool::new(false) })
        .invoke_handler(tauri::generate_handler![info, start_install, cancel_install, quit])
        .run(tauri::generate_context!())
        .expect("error while running the installer");
}
