// Shortcuts, the Installed Apps entry and the uninstaller (docs/spec.md § 5), generalised from Spider's
// registration.rs. Per-user only: everything under HKCU and the user's own folders.

use crate::install::hidden;
use silk_archive::config::Config;
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::System::Registry::*;

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

fn uninstall_key(product: &str) -> String {
    format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{product}")
}

pub fn uninstaller_path(target: &Path, product: &str) -> PathBuf {
    target.join(format!("Uninstall {product}.exe"))
}

#[derive(Debug, PartialEq)]
enum Value {
    Sz(String),
    Dword(u32),
}

/// What Installed Apps reads back. NoModify and NoRepair hide buttons nothing implements yet.
fn uninstall_values(config: &Config, target: &Path, size_bytes: u64, date: &str) -> Vec<(&'static str, Value)> {
    let path = |p: &Path| Value::Sz(p.display().to_string());
    vec![
        ("DisplayName", Value::Sz(config.product.clone())),
        ("DisplayVersion", Value::Sz(config.version.clone())),
        ("Publisher", Value::Sz(config.publisher.clone())),
        ("DisplayIcon", path(&target.join(&config.main_exe))),
        ("InstallLocation", path(target)),
        ("UninstallString", Value::Sz(format!("\"{}\" --uninstall", uninstaller_path(target, &config.product).display()))),
        ("NoModify", Value::Dword(1)),
        ("NoRepair", Value::Dword(1)),
        ("EstimatedSize", Value::Dword((size_bytes / 1024).min(u32::MAX as u64) as u32)),
        ("InstallDate", Value::Sz(date.into())),
    ]
}

/// Write the Installed Apps entry; the uninstaller it names is already in `target`.
pub fn register(config: &Config, target: &Path, size_bytes: u64) -> io::Result<()> {
    let key = wide(uninstall_key(&config.product));
    for (name, value) in uninstall_values(config, target, size_bytes, &today()) {
        let data: Vec<u8> = match &value {
            Value::Sz(s) => wide(s).iter().flat_map(|c| c.to_le_bytes()).collect(),
            Value::Dword(n) => n.to_le_bytes().to_vec(),
        };
        let kind = if let Value::Sz(_) = value { REG_SZ } else { REG_DWORD };
        let r = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), wide(name).as_ptr(), kind, data.as_ptr().cast(), data.len() as u32) };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
    }
    Ok(())
}

/// The existing install's folder and version, from its Installed Apps entry.
pub fn installed(product: &str) -> (Option<PathBuf>, Option<String>) {
    let key = uninstall_key(product);
    (read_sz(&key, "InstallLocation").map(PathBuf::from), read_sz(&key, "DisplayVersion"))
}

fn read_sz(key: &str, name: &str) -> Option<String> {
    let (key, name) = (wide(key), wide(name));
    let mut len = 0u32;
    let get = |buf: *mut u16, len: &mut u32| unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.cast(), len)
    };
    if get(std::ptr::null_mut(), &mut len) != 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize / 2];
    if get(buf.as_mut_ptr(), &mut len) != 0 {
        return None;
    }
    buf.truncate(len as usize / 2);
    let s = String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string();
    (!s.is_empty()).then_some(s)
}

// Absent already (an uninstall run twice) is not worth failing over, as in Spider.
pub fn unregister(product: &str) {
    unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(uninstall_key(product)).as_ptr()) };
}

/// Delete a value named by config as `HKCU\<subkey>`.
pub fn delete_value(key: &str, name: &str) {
    if let Some(sub) = hkcu_subkey(key) {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(sub).as_ptr(), wide(name).as_ptr()) };
    }
}

fn hkcu_subkey(key: &str) -> Option<&str> {
    key.get(..5).filter(|p| p.eq_ignore_ascii_case(r"HKCU\")).map(|_| &key[5..])
}

fn today() -> String {
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    format!("{:04}{:02}{:02}", t.wYear, t.wMonth, t.wDay)
}

pub fn create_shortcuts(product: &str, exe: &Path, dir: &Path, start_menu: bool, desktop: bool) -> io::Result<()> {
    let folders: Vec<&str> = [("Programs", start_menu), ("Desktop", desktop)].into_iter().filter(|f| f.1).map(|f| f.0).collect();
    if folders.is_empty() {
        return Ok(());
    }
    let ok = powershell(&shortcut_script(product, Some((exe, dir)), &folders))?.success();
    if ok { Ok(()) } else { Err(io::Error::other("PowerShell could not write the .lnk files")) }
}

/// Best effort, as in Spider: a locked .lnk must not stop the rest of the uninstall.
pub fn remove_shortcuts(product: &str) {
    let _ = powershell(&shortcut_script(product, None, &["Programs", "Desktop"]));
}

fn powershell(script: &str) -> io::Result<std::process::ExitStatus> {
    hidden("powershell").args(["-NoProfile", "-NonInteractive", "-Command", script]).status()
}

/// PowerShell that writes `<product>.lnk` (to `exe`, starting in `dir`) into each special folder, or
/// removes it when `link` is None. WScript.Shell's shortcut writer, as Spider uses: no COM binding needed.
fn shortcut_script(product: &str, link: Option<(&Path, &Path)>, folders: &[&str]) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let path = |p: &Path| q(&p.display().to_string());
    let list = folders.iter().map(|f| q(f)).collect::<Vec<_>>().join(",");
    let lnk = format!("(Join-Path ([Environment]::GetFolderPath($f)) {})", q(&format!("{product}.lnk")));
    let body = match link {
        Some((exe, dir)) => format!(
            "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({lnk}); $s.TargetPath = {}; $s.WorkingDirectory = {}; $s.Save()",
            path(exe),
            path(dir)
        ),
        None => format!("Remove-Item -LiteralPath {lnk} -ErrorAction SilentlyContinue"),
    };
    format!("$ErrorActionPreference = 'Stop'; foreach ($f in {list}) {{ {body} }}")
}

/// A running exe cannot delete itself: a detached `cmd` waits for this process to exit, then removes
/// `target`, uninstaller included (Spider's approach). Call it as the process ends.
pub fn delete_later(target: &Path) -> io::Result<()> {
    // Retries for about 10 s, in case the window takes a moment to close. raw_arg: std's quoting (\")
    // means nothing to cmd. The cwd must not be the folder being removed.
    let dir = target.display();
    hidden("cmd")
        .arg("/C")
        .raw_arg(format!("for /l %i in (1,1,10) do @(ping 127.0.0.1 -n 2 > nul & rmdir /s /q \"{dir}\" 2> nul & if not exist \"{dir}\" exit)"))
        .current_dir(std::env::temp_dir())
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = r"C:\Users\Bart\AppData\Local\Programs\Silk Sample";

    fn config() -> Config {
        silk_archive::config::parse(
            "product = \"Silk Sample\"\nversion = \"1.2.3\"\npublisher = \"Silk Co\"\nmain_exe = 'bin\\sample.exe'\npayload = \"p\"\n",
        )
        .unwrap()
    }

    #[test]
    fn uninstall_values_describe_the_actual_install() {
        let v = uninstall_values(&config(), Path::new(TARGET), 5 * 1024 * 1024 + 1, "20260929");
        let sz = |s: &str| Value::Sz(s.into());
        assert_eq!(v, [
            ("DisplayName", sz("Silk Sample")),
            ("DisplayVersion", sz("1.2.3")),
            ("Publisher", sz("Silk Co")),
            ("DisplayIcon", sz(&format!(r"{TARGET}\bin\sample.exe"))),
            ("InstallLocation", sz(TARGET)),
            ("UninstallString", sz(&format!(r#""{TARGET}\Uninstall Silk Sample.exe" --uninstall"#))),
            ("NoModify", Value::Dword(1)),
            ("NoRepair", Value::Dword(1)),
            ("EstimatedSize", Value::Dword(5 * 1024)),
            ("InstallDate", sz("20260929")),
        ]);
    }

    #[test]
    fn uninstall_key_is_per_product() {
        assert_eq!(uninstall_key("Silk Sample"), r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Silk Sample");
    }

    #[test]
    fn hkcu_prefix_is_stripped_case_insensitively() {
        assert_eq!(hkcu_subkey(r"HKCU\Software\SilkSampleTest"), Some(r"Software\SilkSampleTest"));
        assert_eq!(hkcu_subkey(r"hkcu\Software\X"), Some(r"Software\X"));
        assert_eq!(hkcu_subkey(r"HKLM\Software\X"), None);
        assert_eq!(hkcu_subkey("HKC"), None);
    }

    #[test]
    fn shortcut_script_quotes_every_string() {
        let s = shortcut_script("Bart's App", Some((Path::new(r"C:\A'pp\a.exe"), Path::new(r"C:\A'pp"))), &["Programs", "Desktop"]);
        assert_eq!(s, "$ErrorActionPreference = 'Stop'; foreach ($f in 'Programs','Desktop') { \
            $s = (New-Object -ComObject WScript.Shell).CreateShortcut((Join-Path ([Environment]::GetFolderPath($f)) 'Bart''s App.lnk')); \
            $s.TargetPath = 'C:\\A''pp\\a.exe'; $s.WorkingDirectory = 'C:\\A''pp'; $s.Save() }");
        let s = shortcut_script("X", None, &["Programs"]);
        assert_eq!(s, "$ErrorActionPreference = 'Stop'; foreach ($f in 'Programs') { \
            Remove-Item -LiteralPath (Join-Path ([Environment]::GetFolderPath($f)) 'X.lnk') -ErrorAction SilentlyContinue }");
    }
}
