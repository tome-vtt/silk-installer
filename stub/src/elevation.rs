// Per-machine installs need administrator rights (docs/spec.md § 2). One stub serves both scopes, so its
// manifest asks for none: a machine-scope run without them starts itself again through UAC and waits.

use crate::registration::wide;
use std::io;

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

/// Run this exe again elevated, with `args` and FLAG, and wait for it. Its exit code, or None when the
/// user said no to UAC.
pub fn relaunch(args: &[&str]) -> io::Result<Option<u32>> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (exe, verb) = (wide(std::env::current_exe()?), wide("runas"));
    let params = wide(args.iter().chain([&FLAG]).copied().collect::<Vec<_>>().join(" "));
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
