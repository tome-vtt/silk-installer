// Per-machine installs need administrator rights (docs/spec.md § 2). One stub serves both scopes, so its
// manifest asks for none: a machine-scope run without them starts itself again through UAC and waits.

use crate::install;
use crate::registration::wide;
use silk_archive::config::{Config, Scope};
use std::io;
use std::path::Path;

/// Added to the elevated copy's arguments: an unelevated parent is waiting for it.
pub const FLAG: &str = "--elevated";

/// Added to the elevated copy's arguments with the unelevated parent's user SID: UAC may have been answered
/// with another account's credentials, so the copy's HKCU is not the user's (reboot.rs registers in HKU).
pub const SID_FLAG: &str = "--user-sid=";

/// The elevated copy's exit code asking its parent to start the app (launch at finish): an elevated
/// installer must not start it elevated, and the parent runs with the signed-in user's own rights.
pub const LAUNCH: u32 = 0x5117;

/// Whether this process can write HKLM and Program Files: an elevated administrator, or SYSTEM.
pub fn is_admin() -> bool {
    // SAFETY: IsUserAnAdmin takes no arguments and only reads this process's token.
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// Per-machine without administrator rights: run again elevated through `relaunch` (UAC: `relaunch` below,
/// outside tests) with this run's `--silent`, `--uninstall` and `reboot::FLAG`, and FLAG, into the same
/// `target`, and return the exit code to pass on. None: this process goes on itself, per-user or with the
/// rights (`admin`), and its `[[run]]` programs run with them. The copy is told the user's SID (`user_sid`).
pub fn elevate(
    config: &Config,
    target: &Path,
    args: &[String],
    admin: bool,
    user_sid: Option<&str>,
    relaunch: impl FnOnce(&[&str]) -> io::Result<Option<u32>>,
) -> Result<Option<i32>, String> {
    if config.scope != Scope::Machine || admin {
        return Ok(None);
    }
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has(FLAG) {
        return Err(format!("{} is installed for all users, which needs administrator rights.", config.product));
    }
    let sid_arg = user_sid.map(|sid| format!("{SID_FLAG}{sid}"));
    let passed: Vec<&str> = ["--silent", "--uninstall", crate::reboot::FLAG]
        .into_iter()
        .filter(|f| has(f))
        .chain([FLAG])
        .chain(sid_arg.as_deref())
        .collect();
    Ok(Some(match relaunch(&passed).map_err(|e| e.to_string())? {
        Some(LAUNCH) => {
            let _ = install::launch(target, &config.main_exe);
            0
        }
        Some(code) => code as i32,
        None => declined(target, has("--silent")),
    }))
}

/// The SID the elevated copy was told to register for, if `args` carry a well-formed one: it becomes part of a
/// registry path, so anything but `S-1-` and digits and hyphens is refused.
pub fn user_sid_arg(args: &[String]) -> Option<&str> {
    let sid = args.iter().find_map(|a| a.strip_prefix(SID_FLAG))?;
    let digits = sid.strip_prefix("S-1-")?;
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '-')).then_some(sid)
}

/// This process's user as a string SID, which an unelevated parent passes to its elevated copy.
pub fn user_sid() -> io::Result<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = std::ptr::null_mut();
    // SAFETY: the pseudo-handle of this process is always valid, and `token` is a live out-pointer.
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) })?;
    let sid = token_sid(token);
    // SAFETY: `token` is the handle OpenProcessToken just returned, closed once, after its last use.
    unsafe { CloseHandle(token) };
    sid
}

fn check(result: i32) -> io::Result<()> {
    if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn token_sid(token: windows_sys::Win32::Foundation::HANDLE) -> io::Result<String> {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_USER, TokenUser};
    let mut len = 0;
    // SAFETY: a null buffer of length 0 only asks for the size, which lands in the live `len`.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    // u64s, so the buffer is aligned for TOKEN_USER.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` is a live buffer of at least `len` bytes, and `token` a handle with TOKEN_QUERY.
    check(unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) })?;
    // SAFETY: the call filled `buf` with a TOKEN_USER, whose SID points into `buf`, which outlives the call.
    sid_text(unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid })
}

/// `sid` as `S-1-...`; it must point to a valid SID.
fn sid_text(sid: windows_sys::Win32::Security::PSID) -> io::Result<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let mut text = std::ptr::null_mut();
    // SAFETY: `sid` is a valid SID (the caller's promise); `text` is a live out-pointer.
    check(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
    // SAFETY: on success `text` is a NUL-terminated string allocated for us, read up to its NUL before it is
    // freed once.
    unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Ok(sid)
    }
}

/// The user to register the restart relaunch for (reboot.rs): the SID the unelevated parent passed, else, in a
/// per-machine process that already has administrator rights (Run as administrator with another account's
/// credentials, so no parent passed one and HKCU is that account's), the SID of whoever is signed in to this
/// process's session (`session`). A per-user install goes into its own account's profile, so it keeps HKCU.
pub fn registered_user(
    args: &[String],
    scope: &Scope,
    admin: bool,
    session: impl FnOnce() -> io::Result<String>,
) -> Option<String> {
    let fallback = admin && *scope == Scope::Machine;
    user_sid_arg(args).map(String::from).or_else(|| fallback.then(|| session().ok()).flatten())
}

/// A string `class` of this process's session's WTS information, such as its user name, up to its NUL.
fn session_string(class: i32) -> io::Result<Vec<u16>> {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSFreeMemory, WTSQuerySessionInformationW,
    };
    let (mut text, mut bytes) = (std::ptr::null_mut(), 0);
    // SAFETY: the current server and session are always valid, and `text` and `bytes` are live out-pointers.
    check(unsafe {
        WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, class, &mut text, &mut bytes)
    })?;
    // SAFETY: on success `text` points to `bytes` bytes of UTF-16 allocated for us, copied before it is freed once.
    unsafe {
        let wide = std::slice::from_raw_parts(text.cast::<u16>(), bytes as usize / 2);
        let wide = wide.iter().copied().take_while(|&c| c != 0).collect();
        WTSFreeMemory(text.cast());
        Ok(wide)
    }
}

/// The SID of the user signed in to this process's session, by their name: what a process started elevated as
/// another account has no parent to be told. The session's user is whoever signed in to it, on the console or
/// over remote desktop, whatever account's credentials started this process.
pub fn session_user_sid() -> io::Result<String> {
    use windows_sys::Win32::Security::{LookupAccountNameW, SECURITY_MAX_SID_SIZE};
    use windows_sys::Win32::System::RemoteDesktop::{WTSDomainName, WTSUserName};
    let (user, domain) = (session_string(WTSUserName)?, session_string(WTSDomainName)?);
    if user.is_empty() {
        return Err(io::Error::other("nobody is signed in to this session"));
    }
    let name: Vec<u16> = domain.into_iter().chain([u16::from(b'\\')]).chain(user).chain([0]).collect();
    // u64s, so the buffer is aligned for a SID.
    let mut sid = [0u64; (SECURITY_MAX_SID_SIZE as usize).div_ceil(8)];
    let mut domain_buf = [0u16; 256];
    let (mut sid_len, mut domain_len, mut kind) = (SECURITY_MAX_SID_SIZE, 256, 0);
    // SAFETY: both buffers are live and at least as long as the sizes passed, and `name` is NUL-terminated.
    check(unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain_buf.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    })?;
    sid_text(sid.as_mut_ptr().cast())
}

/// The user said no to UAC: nothing changed. Logged only where this user may write.
fn declined(target: &Path, silent: bool) -> i32 {
    if silent {
        install::log_line(target, "silent: administrator rights declined, nothing changed");
    }
    1
}

/// Run this exe again elevated, with `args`, and wait for it. Its exit code, or None when the user said no
/// to UAC.
pub fn relaunch(args: &[&str]) -> io::Result<Option<u32>> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (exe, verb) = (wide(std::env::current_exe()?), wide("runas"));
    let params = wide(args.join(" "));
    // SAFETY: SHELLEXECUTEINFOW is a C struct of integers, pointers and handles, for which all zeros (null
    // and 0, the API's unset value) is valid.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap();
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = exe.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    // As ShellExecuteEx asks: it may hand the call to shell extensions.
    // SAFETY: the reserved pointer must be null, and is.
    unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
    // SAFETY: `info` is a SHELLEXECUTEINFOW with its own size in cbSize, and its string pointers point into
    // `verb`, `exe` and `params`, NUL-terminated buffers that live to the end of this function.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(ERROR_CANCELLED as i32) { Ok(None) } else { Err(e) };
    }
    let mut code = 1;
    // SAFETY: SEE_MASK_NOCLOSEPROCESS made `info.hProcess` a process handle this function owns (a null one these
    // calls refuse with an error); it is closed once, after its last use. `code` is a live u32.
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    Ok(Some(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = r"C:\nowhere\Silk Sample";

    fn config(scope: &str) -> Config {
        let toml = format!(
            "product = \"Silk Sample\"\nversion = \"1.0.0\"\npublisher = \"x\"\nmain_exe = \"sample.exe\"\npayload = \"p\"\nscope = \"{scope}\"\n"
        );
        silk_archive::config::parse(&toml).unwrap()
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn never(_: &[&str]) -> io::Result<Option<u32>> {
        panic!("asked for administrator rights")
    }

    #[test]
    fn a_machine_scope_run_without_rights_asks_with_its_own_flags() {
        let mut asked = Vec::new();
        let relaunch = |a: &[&str]| {
            asked = args(a);
            Ok(Some(7))
        };
        let run = args(&["--resumed", "--uninstall", "--silent"]);
        assert_eq!(elevate(&config("machine"), Path::new(TARGET), &run, false, None, relaunch), Ok(Some(7)));
        assert_eq!(asked, ["--silent", "--uninstall", "--resumed", FLAG]);
    }

    #[test]
    fn the_elevated_copy_is_told_the_user_who_started_the_installer() {
        let mut asked = Vec::new();
        let relaunch = |a: &[&str]| {
            asked = args(a);
            Ok(Some(0))
        };
        let sid = Some("S-1-5-21-1-2-3-1001");
        assert_eq!(elevate(&config("machine"), Path::new(TARGET), &[], false, sid, relaunch), Ok(Some(0)));
        assert_eq!(asked, [FLAG, "--user-sid=S-1-5-21-1-2-3-1001"]);
        assert_eq!(user_sid_arg(&asked), sid);
    }

    #[test]
    fn only_a_well_formed_sid_is_taken_from_the_arguments() {
        for bad in ["--user-sid=", "--user-sid=S-1-", r"--user-sid=S-1-5\..\x", "--user-sid=S-2-5", "--user-sid=1-5"] {
            assert_eq!(user_sid_arg(&args(&[bad])), None, "{bad}");
        }
        assert_eq!(user_sid_arg(&args(&["--silent"])), None);
    }

    #[test]
    fn this_process_has_a_user_sid() {
        let sid = user_sid().unwrap();
        assert_eq!(user_sid_arg(&args(&[&format!("{SID_FLAG}{sid}")])), Some(sid.as_str()));
    }

    #[test]
    fn an_installer_started_elevated_registers_for_the_session_user() {
        let (machine, user) = (Scope::Machine, Scope::User);
        let session = || Ok("S-1-5-21-1-2-3-1001".to_string());
        let passed = args(&["--user-sid=S-1-5-21-9-9-9-500"]);
        // The parent's SID wins; an elevated per-machine process without a parent asks its session; a plain
        // one has none, and neither has a per-user one, which installs into its own account's profile.
        assert_eq!(registered_user(&passed, &machine, true, session).as_deref(), Some("S-1-5-21-9-9-9-500"));
        assert_eq!(registered_user(&[], &machine, true, session).as_deref(), Some("S-1-5-21-1-2-3-1001"));
        assert_eq!(registered_user(&[], &machine, false, session), None);
        assert_eq!(registered_user(&[], &user, true, session), None);
        assert_eq!(registered_user(&[], &machine, true, || Err(io::Error::other("nobody"))), None);
    }

    #[test]
    fn the_session_user_has_a_well_formed_sid_when_somebody_is_signed_in() {
        // Tests may run in session 0, where nobody is signed in: only a SID that comes back is checked.
        if let Ok(sid) = session_user_sid() {
            assert_eq!(user_sid_arg(&args(&[&format!("{SID_FLAG}{sid}")])), Some(sid.as_str()));
        }
    }

    #[test]
    fn a_declined_prompt_exits_with_1_and_a_launch_request_with_0() {
        let (c, target) = (config("machine"), Path::new(TARGET));
        assert_eq!(elevate(&c, target, &[], false, None, |_| Ok(None)), Ok(Some(1)));
        assert_eq!(elevate(&c, target, &[], false, None, |_| Ok(Some(LAUNCH))), Ok(Some(0)));
        let e = elevate(&c, target, &[], false, None, |_| Err(io::Error::other("no shell"))).unwrap_err();
        assert!(e.contains("no shell"), "{e}");
    }

    #[test]
    fn only_a_machine_scope_run_without_rights_asks() {
        let target = Path::new(TARGET);
        assert_eq!(elevate(&config("user"), target, &[], false, None, never), Ok(None));
        assert_eq!(elevate(&config("machine"), target, &args(&[FLAG]), true, None, never), Ok(None));
    }

    #[test]
    fn the_elevated_copy_without_rights_does_not_ask_again() {
        let e = elevate(&config("machine"), Path::new(TARGET), &args(&[FLAG]), false, None, never).unwrap_err();
        assert!(e.contains("needs administrator rights"), "{e}");
    }
}
