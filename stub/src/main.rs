// The runtime stub: installs the archive appended to its own exe (docs/spec.md § 3).
// No arguments: the installer window. --silent: install with no window (§ 6). --uninstall: run by the
// uninstaller copy that Installed Apps starts: the same theme in uninstall mode, or no window with --silent.
// A per-machine installer or uninstaller first gets administrator rights (elevation.rs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod elevation;
mod install;
mod reboot;
mod registration;
mod theme;

use serde::Serialize;
use silk_archive::Archive;
use silk_archive::config::{Config, Scope};
use std::io;
use std::path::{Path, PathBuf};
use std::process::ChildStdin;
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
    /// The stdin of the `talk` program running now, which `silk.write` writes to.
    stdin: Mutex<Option<ChildStdin>>,
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
    /// Started again after `silk.reboot` restarted Windows: the page starts the install once it has loaded.
    resumed: bool,
    frame: bool,
    files: usize,
    size: u64,
}

/// The theme JS API (`window.silk`, docs/theme-api.md) with `info` filled in, run before any theme script.
fn api_script(info: &Info) -> String {
    include_str!("api.js").replace("/*INFO*/", &serde_json::to_string(info).unwrap())
}

/// Installs (or uninstalls, in uninstall mode) on a blocking thread, emitting `silk:progress`, and
/// `silk:output` and `silk:exit` for a `talk` program; resolves when done, rejects with the message (or
/// `cancelled`). A second call while one runs is refused.
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
        let line = |line: &str| drop(app.emit("silk:output", line));
        let exit = |code: i32| drop(app.emit("silk:exit", code));
        let talk = Some(install::Talk { stdin: &st.stdin, line: &line, exit: &exit });
        if st.uninstall {
            install::uninstall(
                &st.config,
                &st.target,
                |p| {
                    emit(p);
                },
                talk.as_ref(),
            )?;
            st.removed.store(true, Ordering::SeqCst);
            return Ok(());
        }
        install::install(&mut archive, &st.config, &st.target, emit, talk.as_ref())
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

/// One line to the stdin of the `talk` program running now; async, so a program not reading never blocks the window.
#[tauri::command]
async fn write(st: State<'_, Installer>, line: String) -> Result<(), String> {
    install::answer(&st.stdin, &line)
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    open_url_with(&url, shell_open)
}

/// Hand `url` to `open` (the shell, outside tests): web addresses only, since the shell would run a program
/// path or a file: URL.
fn open_url_with(url: &str, open: impl FnOnce(&str) -> io::Result<()>) -> Result<(), String> {
    let web = ["http://", "https://"].iter().any(|s| url.get(..s.len()).is_some_and(|p| p.eq_ignore_ascii_case(s)));
    if !web {
        return Err(format!("{url} is not a web address."));
    }
    open(url).map_err(|e| format!("Setup could not open {url}: {e}"))
}

/// Open `url` in the default browser.
// ponytail: an elevated (per-machine) installer opens it with its own rights, as administrator; Edge drops
// them itself, other browsers may not. Hand it to the unelevated parent, as LAUNCH does the app, if that matters.
fn shell_open(url: &str) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, file) = (registration::wide("open"), registration::wide(url));
    let null = std::ptr::null();
    // SAFETY: a null owner, parameters and folder are allowed; `verb` and `file` are NUL-terminated UTF-16
    // that live until the call returns.
    let done = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), null, null, SW_SHOWNORMAL) };
    // Its result is an HINSTANCE only by name: above 32 is success.
    let code = done as isize;
    if code > 32 { Ok(()) } else { Err(io::Error::other(format!("the shell refused it (code {code})"))) }
}

/// Restart Windows, coming back after sign-in (reboot.rs). The theme asking is the user's confirmation; refused
/// while setup runs, and held so it cannot start meanwhile.
#[tauri::command]
fn reboot(st: State<Installer>) -> Result<(), String> {
    let _idle = st.archive.try_lock().map_err(|_| "Setup is still running.".to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    reboot::reboot(&st.config, &reboot::command(&exe, st.uninstall), reboot::restart)
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

/// The window needs WebView2 (docs/spec.md § 7): without it, install it with the bootstrapper an installer
/// built with `webview2_bootstrapper` carries, or say so; either before asking for administrator rights,
/// where Tauri would open an empty window and panic. A silent run has no window, so it needs none.
fn require_webview2(silent: bool, archive: &mut Archive) {
    if silent || tauri::webview_version().is_ok() {
        return;
    }
    if let Err(e) = install_webview2(archive) {
        fail(false, e);
    }
}

const WEBVIEW2_URL: &str = "https://developer.microsoft.com/microsoft-edge/webview2";

/// Install WebView2 with the bootstrapper the installer carries, from a folder of its own in the temp folder;
/// Err is the message to show.
fn install_webview2(archive: &mut Archive) -> Result<(), String> {
    if !archive.has_webview2() {
        return Err(format!(
            "Setup needs the Microsoft Edge WebView2 Runtime, which is not installed on this PC. \
            Install it from {WEBVIEW2_URL}, then run Setup again."
        ));
    }
    let dir = std::env::temp_dir().join(format!("silk-webview2-{}", std::process::id()));
    let setup = dir.join("MicrosoftEdgeWebview2Setup.exe");
    let still_missing = |_| io::Error::other("it is still missing afterwards");
    let bootstrapped = std::fs::create_dir(&dir)
        .and_then(|()| archive.extract_webview2(&setup))
        .and_then(|()| run_bootstrapper(&setup));
    let _ = std::fs::remove_dir_all(&dir);
    bootstrapped.and_then(|()| tauri::webview_version().map(drop).map_err(still_missing)).map_err(|e| {
        format!(
            "Setup could not install the Microsoft Edge WebView2 Runtime it needs: {e}. Check that this PC is \
            online, or install it from {WEBVIEW2_URL}, then run Setup again."
        )
    })
}

/// Run Microsoft's WebView2 bootstrapper and wait while it downloads and installs the runtime, as Microsoft
/// documents it: per-user when Setup runs without administrator rights, so a per-user install asks for none,
/// and per-machine when elevated.
fn run_bootstrapper(setup: &Path) -> io::Result<()> {
    let status = std::process::Command::new(setup).args(["/silent", "/install"]).status()?;
    if !status.success() {
        // Its exit codes are HRESULTs, which Microsoft lists in hex.
        return Err(io::Error::other(format!(
            "Microsoft's installer failed (exit code {:#x})",
            status.code().unwrap_or(-1)
        )));
    }
    Ok(())
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
    require_webview2(silent, &mut archive);

    // Per-machine without administrator rights: run again elevated and pass on its exit code. The copy
    // installs into the same target: the existing install's, or the config's default.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let elevated = elevation::elevate(&config, &target, &args, elevation::is_admin(), elevation::relaunch);
    if let Some(code) = elevated.unwrap_or_else(|e| fail(silent, e)) {
        std::process::exit(code);
    }
    let resumed = reboot::resumed(&config, &args);
    let launcher = match config.scope {
        Scope::User => Launch::Here,
        Scope::Machine if has(elevation::FLAG) => Launch::ByParent,
        Scope::Machine => Launch::Never,
    };

    // Checked before Tauri builds anything, so a silent run never creates a window.
    if silent && uninstall {
        install::uninstall(&config, &target, |_| {}, None).unwrap_or_else(|e| fail(silent, e));
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
        resumed,
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
            stdin: Mutex::new(None),
        })
        .manage(theme)
        .invoke_handler(tauri::generate_handler![start, cancel, launch, write, open_url, reboot, quit])
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| fail(false, format!("Setup could not open its window: {e}")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_url_hands_a_web_address_to_the_shell_unchanged() {
        let url = "HTTPS://example.com/sign in?code=a,b&x=%20#top";
        let mut opened = None;
        let open = |u: &str| {
            opened = Some(u.to_string());
            Ok(())
        };
        assert_eq!(open_url_with(url, open), Ok(()));
        assert_eq!(opened.as_deref(), Some(url));
    }

    #[test]
    fn open_url_refuses_anything_but_a_web_address() {
        for url in [r"C:\Windows\System32\calc.exe", "file:///C:/Windows/System32/calc.exe", "calc", "httpx://a"] {
            let e = open_url_with(url, |_| panic!("{url} reached the shell")).unwrap_err();
            assert!(e.contains("not a web address"), "{e}");
        }
    }
}
