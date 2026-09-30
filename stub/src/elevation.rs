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
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// Run this exe again elevated, with `args` and FLAG, and wait for it. Its exit code, or None when the
/// user said no to UAC.
pub fn relaunch(args: &[&str]) -> io::Result<Option<u32>> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (exe, verb) = (wide(std::env::current_exe()?), wide("runas"));
    let params = wide(args.iter().chain([&FLAG]).copied().collect::<Vec<_>>().join(" "));
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = exe.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    // As ShellExecuteEx asks: it may hand the call to shell extensions.
    unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(ERROR_CANCELLED as i32) { Ok(None) } else { Err(e) };
    }
    let mut code = 1;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    Ok(Some(code))
}
