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
    /// The run a window started after `silk.reboot` owes, until `start` begins one.
    resume: reboot::Pending,
    /// The user who started an elevated copy, whose hive `silk.reboot` registers the relaunch in.
    user_sid: Option<String>,
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
        let mut archive = st.resume.start(&st.archive).ok_or("Setup is already running.")?;
        st.cancel.store(false, Ordering::SeqCst);
        let emit = |p: &install::Progress| drop(app.emit("silk:progress", p));
        let send = |event: &str, payload: serde_json::Value| drop(app.emit(event, payload));
        let line = |line: &str| emit_output(&send, line);
        let exit = |code: i32| emit_exit(&send, code);
        let talk = Some(install::Talk { stdin: &st.stdin, line: &line, exit: &exit });
        if st.uninstall {
            install::uninstall(&st.config, &st.target, st.user_sid.as_deref(), emit, talk.as_ref())?;
            st.removed.store(true, Ordering::SeqCst);
            return Ok(());
        }
        install::install(&mut archive, &st.config, &st.target, emit, &st.cancel, talk.as_ref())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A `talk` program's stdout line and exit code as the events api.js listens for.
fn emit_output(send: &impl Fn(&str, serde_json::Value), line: &str) {
    send("silk:output", line.into());
}

fn emit_exit(send: &impl Fn(&str, serde_json::Value), code: i32) {
    send("silk:exit", code.into());
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
/// path or a file: URL; and none with a `"`, which no web address holds and which would end Explorer's quoted
/// argument (explorer_open).
fn open_url_with(url: &str, open: impl FnOnce(&str) -> io::Result<()>) -> Result<(), String> {
    let web = ["http://", "https://"].iter().any(|s| url.get(..s.len()).is_some_and(|p| p.eq_ignore_ascii_case(s)))
        && !url.contains('"');
    if !web {
        return Err(format!("{url} is not a web address."));
    }
    open(url).map_err(|e| format!("Setup could not open {url}: {e}"))
}

/// Open `url` in the default browser at the user's own rights. An elevated (per-machine) installer must not
/// call the shell itself, or the browser could start as administrator: `explorer.exe "<url>"` hands the address
/// to the desktop shell that is already running, which opens it with its own, unelevated token.
fn shell_open(url: &str) -> io::Result<()> {
    let system = install::system_dir()?;
    // Explorer's exit code says nothing about the browser: only failing to start it is an error.
    explorer_open(url, system.parent().unwrap_or(Path::new(r"C:\Windows"))).spawn().map(drop)
}

/// The command that hands `url` to the desktop shell: Explorer in the Windows folder `windows`, never a bare
/// name, which `Command` would look for in the installer's own folder first. The address always goes in quotes,
/// written raw: `Command` quotes only an argument with a space, and Explorer splits an unquoted one at commas.
fn explorer_open(url: &str, windows: &Path) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = install::hidden(windows.join("explorer.exe"));
    c.raw_arg(format!("\"{url}\""));
    c
}

/// Restart Windows, coming back after sign-in (reboot.rs). The theme asking is the user's confirmation. Refused
/// while setup runs, and held so it cannot start meanwhile; and when uninstalling, as the uninstaller is gone after.
#[tauri::command]
fn reboot(st: State<Installer>) -> Result<(), String> {
    if st.uninstall {
        return Err("Setup restarts Windows only while installing.".into());
    }
    let _idle = st.archive.try_lock().map_err(|_| "Setup is still running.".to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let user_sid = st.user_sid.as_deref();
    reboot::reboot(&st.config, &reboot::command(&exe), user_sid, reboot::restart, registration::delete_value)
}

/// Whether this page starts the run after a restart: a resumed window's pages do until a run has started.
#[tauri::command]
fn resume_owed(st: State<Installer>) -> bool {
    st.resume.owed()
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
        .and_then(|()| while_waiting(WAIT_TEXT, || run_bootstrapper(&setup)));
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

const WAIT_TEXT: &str = "Setup is installing the Microsoft Edge WebView2 Runtime. Please wait...";

/// Run `work` with a small please-wait window on screen, closed before this returns. It has no close box and
/// ignores `WM_CLOSE` (Alt+F4), so the user cannot dismiss it early. Without a desktop to show it on, `work` simply runs without one.
fn while_waiting<T>(text: &str, work: impl FnOnce() -> T) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let text = text.to_string();
    let window = std::thread::spawn(move || wait_window(&text, &tx));
    // The window's thread id once the window exists, 0 when it could not be made.
    let thread = rx.recv().unwrap_or(0);
    let result = work();
    if thread != 0 {
        use windows_sys::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
        // SAFETY: plain integers. `thread` is the window thread, which runs until this very message (or fails
        // to start, and then `thread` is 0 and this is skipped), so the id cannot have been reused.
        unsafe { PostThreadMessageW(thread, WM_QUIT, 0, 0) };
    }
    let _ = window.join();
    result
}

/// The STATIC class's own window procedure, which `refuse_close` hands everything else to.
static STATIC_PROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// The please-wait window's procedure: `WM_CLOSE` (what Alt+F4 ends in) does nothing; the rest is STATIC's.
unsafe extern "system" fn refuse_close(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallWindowProcW, WM_CLOSE, WNDPROC};
    if msg == WM_CLOSE {
        return 0;
    }
    let previous = STATIC_PROC.load(Ordering::SeqCst);
    // SAFETY: `previous` is the procedure `wait_window` replaced with this one, stored before it was replaced.
    unsafe { CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(previous), hwnd, msg, wparam, lparam) }
}

/// Put `refuse_close` in front of `hwnd`'s window procedure; `hwnd` is a window of this thread.
fn refuse_close_on(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GWLP_WNDPROC, SetWindowLongPtrW};
    // SAFETY: `hwnd` is this thread's own window, and `refuse_close` has the window procedure's signature.
    let previous = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, refuse_close as *const () as isize) };
    STATIC_PROC.store(previous, Ordering::SeqCst);
}

/// Show `text` in a captioned window without a close box and pump its messages until `WM_QUIT`; sends this
/// thread's id once the window exists (0 if it could not be made).
fn wait_window(text: &str, ready: &std::sync::mpsc::Sender<u32>) {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CW_USEDEFAULT, CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, MSG, TranslateMessage,
        WS_CAPTION, WS_EX_TOPMOST, WS_VISIBLE,
    };
    const SS_CENTER: u32 = 1;
    let (class, text, null) = (registration::wide("STATIC"), registration::wide(text), std::ptr::null_mut());
    let style = WS_CAPTION | WS_VISIBLE | SS_CENTER;
    // SAFETY: the built-in STATIC class needs no instance; the strings are NUL-terminated UTF-16 that live
    // until the call returns, and a null parent, menu and parameter are allowed.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            class.as_ptr(),
            text.as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            420,
            90,
            null,
            null,
            null,
            null,
        )
    };
    if hwnd.is_null() {
        let _ = ready.send(0);
        return;
    }
    refuse_close_on(hwnd);
    // SAFETY: no arguments.
    let _ = ready.send(unsafe { GetCurrentThreadId() });
    // SAFETY: `msg` is plain data that GetMessageW fills in; `hwnd` is this thread's own window.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        DestroyWindow(hwnd);
    }
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
    // The user's SID is best effort: without it the copy asks its session who is signed in.
    let sid = elevation::user_sid().ok();
    let elevated =
        elevation::elevate(&config, &target, &args, elevation::is_admin(), sid.as_deref(), elevation::relaunch);
    if let Some(code) = elevated.unwrap_or_else(|e| fail(silent, e)) {
        std::process::exit(code);
    }
    // Whose hive holds the relaunch entry and the user's `remove_registry_value` keys: the parent's SID, else
    // the session user's when a per-machine install was started elevated directly.
    let user_sid = elevation::registered_user(&args, &config.scope, elevation::is_admin(), elevation::session_user_sid);
    let resumed = reboot::resume(&config, &args, user_sid.as_deref(), registration::delete_value);
    let launcher = match config.scope {
        Scope::User => Launch::Here,
        Scope::Machine if has(elevation::FLAG) => Launch::ByParent,
        Scope::Machine => Launch::Never,
    };

    // Checked before Tauri builds anything, so a silent run never creates a window.
    if silent && uninstall {
        install::uninstall(&config, &target, user_sid.as_deref(), |_| {}, None).unwrap_or_else(|e| fail(silent, e));
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
                // Said here, not returned: Tauri panics on a setup error, which a release build never shows.
                .build()
                .unwrap_or_else(|e| fail(false, format!("Setup could not open its window: {e}")));
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
            resume: reboot::Pending::new(resumed),
            user_sid,
        })
        .manage(theme)
        .invoke_handler(tauri::generate_handler![start, cancel, launch, write, open_url, reboot, resume_owed, quit])
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
    use serde_json::json;
    use tauri::test::{INVOKE_KEY, mock_builder, mock_context, noop_assets};
    use tauri::webview::InvokeRequest;

    #[test]
    fn a_talk_programs_lines_and_exit_code_go_out_as_the_events_api_js_listens_for() {
        let sent = Mutex::new(Vec::new());
        let send = |event: &str, payload| sent.lock().unwrap().push((event.to_string(), payload));
        emit_output(&send, "name?");
        emit_output(&send, "hello Ann");
        emit_exit(&send, 7);
        let want = [("silk:output", json!("name?")), ("silk:output", json!("hello Ann")), ("silk:exit", json!(7))];
        assert_eq!(*sent.lock().unwrap(), want.map(|(e, p)| (e.to_string(), p)));
    }

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
    fn the_browser_is_opened_through_the_desktop_shell_with_the_address_unchanged() {
        // Explorer splits an unquoted command line at commas, so every address goes in quotes, spaces or not.
        for url in ["https://example.com/a b?x=1&y=%20#top", "https://x/?scope=a,b", r"https://x,C:\prog.exe"] {
            let mut args = None;
            let open = |u: &str| {
                let c = explorer_open(u, Path::new(r"C:\Windows"));
                assert_eq!(c.get_program(), r"C:\Windows\explorer.exe");
                args = Some(c.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>());
                Ok(())
            };
            assert_eq!(open_url_with(url, open), Ok(()));
            assert_eq!(args, Some(vec![format!("\"{url}\"")]));
        }
    }

    #[test]
    fn open_url_refuses_anything_but_a_web_address() {
        // The last would end Explorer's quoted argument and pass it a program path.
        let quote = r#"https://x/" C:\prog.exe"#;
        for url in [r"C:\Windows\System32\calc.exe", "file:///C:/Windows/System32/calc.exe", "calc", "httpx://a", quote]
        {
            let e = open_url_with(url, |_| panic!("{url} reached the shell")).unwrap_err();
            assert!(e.contains("not a web address"), "{e}");
        }
    }

    #[test]
    fn the_please_wait_window_closes_and_hands_back_the_work() {
        assert_eq!(while_waiting("Please wait", || 7), 7);
    }

    #[test]
    fn the_please_wait_window_survives_alt_f4() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, IsWindow, SC_CLOSE, SendMessageW, WM_CLOSE, WM_SYSCOMMAND,
        };
        let text = "Please wait (alt-f4 test)";
        let alive = while_waiting(text, || {
            // SAFETY: a null class is allowed and the title is NUL-terminated UTF-16 that lives through the call.
            let hwnd = unsafe { FindWindowW(std::ptr::null(), registration::wide(text).as_ptr()) };
            // Without a desktop there is no window to close.
            if hwnd.is_null() {
                return true;
            }
            // SAFETY: `hwnd` is the live window of the other thread; SendMessageW waits for it to answer.
            unsafe {
                SendMessageW(hwnd, WM_SYSCOMMAND, SC_CLOSE as usize, 0);
                SendMessageW(hwnd, WM_CLOSE, 0, 0);
                IsWindow(hwnd) != 0
            }
        });
        assert!(alive, "Alt+F4 closed the please-wait window");
    }

    /// A command as a theme's `silk.write` or `silk.openUrl` reaches it: through Tauri, with the keys api.js sends.
    fn invoke(cmd: &str, body: serde_json::Value) -> Result<serde_json::Value, serde_json::Value> {
        let dir = std::env::temp_dir().join(format!("silk-stub-ipc-{cmd}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stub.exe"), "stub").unwrap();
        let toml = "product = \"P\"\nversion = \"1\"\npublisher = \"x\"\nmain_exe = \"p.exe\"\npayload = \"payload\"\n";
        let config = silk_archive::config::parse(toml).unwrap();
        let setup = dir.join("setup.exe");
        silk_archive::build(&dir.join("stub.exe"), None, None, &config, &setup).unwrap();
        let installer = Installer {
            config,
            archive: Mutex::new(Archive::open(&setup).unwrap().unwrap()),
            target: dir.clone(),
            cancel: AtomicBool::new(false),
            uninstall: false,
            removed: AtomicBool::new(false),
            launch: Launch::Never,
            launch_requested: AtomicBool::new(false),
            stdin: Mutex::new(None),
            resume: reboot::Pending::new(false),
            user_sid: None,
        };
        let app = mock_builder()
            .manage(installer)
            .invoke_handler(tauri::generate_handler![write, open_url])
            .build(mock_context(noop_assets()))
            .unwrap();
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
        let request = InvokeRequest {
            cmd: cmd.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: "http://tauri.localhost".parse().unwrap(),
            body: tauri::ipc::InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        };
        let response = tauri::test::get_ipc_response(&webview, request);
        let _ = std::fs::remove_dir_all(&dir);
        response.map(|b| b.deserialize::<serde_json::Value>().unwrap())
    }

    #[test]
    fn write_takes_the_line_key_api_js_sends() {
        // No program is reading, so a line that arrived under its name is refused with this, not a missing-key error.
        let want = Err(json!("No program is running that reads from Setup."));
        assert_eq!(invoke("write", json!({ "line": "Ann" })), want);
    }

    #[test]
    fn open_url_takes_the_url_key_and_passes_the_address_on_unchanged() {
        // A refused address comes back inside the message exactly as sent, and nothing reaches the shell.
        let url = " File:///C:/Windows/System32/calc.exe ";
        assert_eq!(invoke("open_url", json!({ "url": url })), Err(json!(format!("{url} is not a web address."))));
    }
}
