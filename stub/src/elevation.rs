// Per-machine installs need administrator rights (docs/spec.md § 2). One stub serves both scopes, so its
// manifest asks for none: a machine-scope run without them starts itself again through UAC and waits.

use crate::install;
use crate::registration::wide;
use silk_archive::config::{Config, Scope};
use std::io;
use std::path::Path;

/// Added to the elevated copy's arguments: an unelevated parent is waiting for it.
pub const FLAG: &str = "--elevated";

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
/// rights (`admin`), and its `[[run]]` programs run with them.
pub fn elevate(
    config: &Config,
    target: &Path,
    args: &[String],
    admin: bool,
    relaunch: impl FnOnce(&[&str]) -> io::Result<Option<u32>>,
) -> Result<Option<i32>, String> {
    if config.scope != Scope::Machine || admin {
        return Ok(None);
    }
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has(FLAG) {
        return Err(format!("{} is installed for all users, which needs administrator rights.", config.product));
    }
    let passed: Vec<&str> =
        ["--silent", "--uninstall", crate::reboot::FLAG].into_iter().filter(|f| has(f)).chain([FLAG]).collect();
    Ok(Some(match relaunch(&passed).map_err(|e| e.to_string())? {
        Some(LAUNCH) => {
            let _ = install::launch(target, &config.main_exe);
            0
        }
        Some(code) => code as i32,
        None => declined(target, has("--silent")),
    }))
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
        assert_eq!(elevate(&config("machine"), Path::new(TARGET), &run, false, relaunch), Ok(Some(7)));
        assert_eq!(asked, ["--silent", "--uninstall", "--resumed", FLAG]);
    }

    #[test]
    fn a_declined_prompt_exits_with_1_and_a_launch_request_with_0() {
        let (c, target) = (config("machine"), Path::new(TARGET));
        assert_eq!(elevate(&c, target, &[], false, |_| Ok(None)), Ok(Some(1)));
        assert_eq!(elevate(&c, target, &[], false, |_| Ok(Some(LAUNCH))), Ok(Some(0)));
        let e = elevate(&c, target, &[], false, |_| Err(io::Error::other("no shell"))).unwrap_err();
        assert!(e.contains("no shell"), "{e}");
    }

    #[test]
    fn only_a_machine_scope_run_without_rights_asks() {
        let target = Path::new(TARGET);
        assert_eq!(elevate(&config("user"), target, &[], false, never), Ok(None));
        assert_eq!(elevate(&config("machine"), target, &args(&[FLAG]), true, never), Ok(None));
    }

    #[test]
    fn the_elevated_copy_without_rights_does_not_ask_again() {
        let e = elevate(&config("machine"), Path::new(TARGET), &args(&[FLAG]), false, never).unwrap_err();
        assert!(e.contains("needs administrator rights"), "{e}");
    }
}
