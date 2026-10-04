// Shortcuts, the Installed Apps entry and the uninstaller (docs/spec.md § 5), generalised from Spider's
// registration.rs. Per-user installs use HKCU and the user's own folders, per-machine ones HKLM and the
// all-users folders.

use crate::install::hidden;
use silk_archive::config::{Config, FirewallRule, Scope, expand};
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::System::Registry::*;

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

// The 64-bit registry view and Program Files come for free in a 64-bit process; a 32-bit stub would
// land in WOW6432Node and Program Files (x86).
const _: () = assert!(cfg!(target_pointer_width = "64"), "the stub must be a 64-bit build");

/// The config's target on this machine, `{localappdata}` and `{programfiles}` from the known-folder API.
pub fn target_dir(config: &Config) -> Result<PathBuf, String> {
    use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_ProgramFiles};
    config.target_dir(&known_folder(&FOLDERID_LocalAppData)?, &known_folder(&FOLDERID_ProgramFiles)?)
}

fn known_folder(id: &windows_sys::core::GUID) -> Result<String, String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut p = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) };
    let path = (hr >= 0).then(|| unsafe {
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    });
    unsafe { CoTaskMemFree(p.cast()) };
    path.ok_or_else(|| format!("Setup could not find a system folder (error {hr:#010x})."))
}

/// Where Installed Apps looks for this scope's entries.
fn root(scope: &Scope) -> HKEY {
    match scope {
        Scope::User => HKEY_CURRENT_USER,
        Scope::Machine => HKEY_LOCAL_MACHINE,
    }
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
        let r = unsafe { RegSetKeyValueW(root(&config.scope), key.as_ptr(), wide(name).as_ptr(), kind, data.as_ptr().cast(), data.len() as u32) };
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
    }
    Ok(())
}

/// The existing install's folder and version, from its Installed Apps entry.
pub fn installed(config: &Config) -> (Option<PathBuf>, Option<String>) {
    let (root, key) = (root(&config.scope), uninstall_key(&config.product));
    (read_sz(root, &key, "InstallLocation").map(PathBuf::from), read_sz(root, &key, "DisplayVersion"))
}

fn read_sz(root: HKEY, key: &str, name: &str) -> Option<String> {
    let (key, name) = (wide(key), wide(name));
    let mut len = 0u32;
    let get = |buf: *mut u16, len: &mut u32| unsafe {
        RegGetValueW(root, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.cast(), len)
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
pub fn unregister(config: &Config) {
    unsafe { RegDeleteTreeW(root(&config.scope), wide(uninstall_key(&config.product)).as_ptr()) };
}

/// Delete a value named by config as `HKCU\<subkey>` or (per-machine) `HKLM\<subkey>`. In an elevated
/// run HKCU is the elevating account's, which is not the signed-in user's when another account elevated.
pub fn delete_value(key: &str, name: &str) {
    if let Some((root, sub)) = split_root(key) {
        unsafe { RegDeleteKeyValueW(root, wide(sub).as_ptr(), wide(name).as_ptr()) };
    }
}

fn split_root(key: &str) -> Option<(HKEY, &str)> {
    let root = match key.get(..5)?.to_ascii_uppercase().as_str() {
        r"HKCU\" => HKEY_CURRENT_USER,
        r"HKLM\" => HKEY_LOCAL_MACHINE,
        _ => return None,
    };
    Some((root, &key[5..]))
}

fn today() -> String {
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    format!("{:04}{:02}{:02}", t.wYear, t.wMonth, t.wDay)
}

/// The special folders (for `[Environment]::GetFolderPath`) of the Start menu and the desktop.
fn shortcut_folders(scope: &Scope) -> [&'static str; 2] {
    match scope {
        Scope::User => ["Programs", "Desktop"],
        Scope::Machine => ["CommonPrograms", "CommonDesktopDirectory"],
    }
}

pub fn create_shortcuts(scope: &Scope, product: &str, exe: &Path, dir: &Path, start_menu: bool, desktop: bool) -> io::Result<()> {
    let [programs, desktop_dir] = shortcut_folders(scope);
    let folders: Vec<&str> = [(programs, start_menu), (desktop_dir, desktop)].into_iter().filter(|f| f.1).map(|f| f.0).collect();
    if folders.is_empty() {
        return Ok(());
    }
    let ok = powershell(&shortcut_script(product, Some((exe, dir)), &folders))?.success();
    if ok { Ok(()) } else { Err(io::Error::other("PowerShell could not write the .lnk files")) }
}

/// Best effort, as in Spider: a locked .lnk must not stop the rest of the uninstall.
pub fn remove_shortcuts(scope: &Scope, product: &str) {
    let _ = powershell(&shortcut_script(product, None, &shortcut_folders(scope)));
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

/// Add each `[[firewall]]` rule, first deleting any rule of the same name so an upgrade leaves one copy.
/// netsh rather than the INetFwPolicy2 COM API: windows-sys has no COM interface bindings, and the stub
/// already runs PowerShell and cmd for the same reason. Needs the elevated per-machine run.
// ponytail: a rule renamed or dropped by an upgrade stays behind; the old config is not at hand to remove it.
pub fn add_firewall_rules(config: &Config, target: &Path) -> Result<(), String> {
    for rule in &config.firewall {
        remove_firewall_rule(&rule.name);
        let status = hidden("netsh").args(firewall_add_args(rule, target)?).status().map_err(|e| format!("could not run netsh: {e}"))?;
        if !status.success() {
            return Err(format!("could not add the firewall rule {:?} (netsh exit code {}).", rule.name, status.code().unwrap_or(-1)));
        }
    }
    Ok(())
}

/// Best effort: a rule already gone (removed by hand, or an uninstall run twice) is not worth failing over.
pub fn remove_firewall_rules(config: &Config) {
    config.firewall.iter().for_each(|r| remove_firewall_rule(&r.name));
}

fn remove_firewall_rule(name: &str) {
    let _ = hidden("netsh").args(["advfirewall", "firewall", "delete", "rule", &format!("name={name}"), "dir=in"]).status();
}

/// netsh's arguments for adding `rule`, `{target}` filled into its program. std quotes an argument with
/// spaces as `"name=A b"`, which netsh reads as the name `A b`.
fn firewall_add_args(rule: &FirewallRule, target: &Path) -> Result<Vec<String>, String> {
    let lower = |v: &dyn std::fmt::Debug| format!("{v:?}").to_lowercase();
    let mut args = ["advfirewall", "firewall", "add", "rule"].map(String::from).to_vec();
    args.push(format!("name={}", rule.name));
    args.extend(["dir=in", "action=allow", "enable=yes"].map(String::from));
    args.push(format!("protocol={}", lower(&rule.protocol)));
    args.push(format!("profile={}", rule.profiles.iter().map(|p| lower(p)).collect::<Vec<_>>().join(",")));
    if !rule.ports.is_empty() {
        args.push(format!("localport={}", rule.ports.iter().map(u32::to_string).collect::<Vec<_>>().join(",")));
    }
    if let Some(program) = &rule.program {
        let target = target.to_string_lossy();
        args.push(format!("program={}", expand(program, &[("target", &target)])?));
    }
    Ok(args)
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
    fn root_prefix_is_split_off_case_insensitively() {
        assert_eq!(split_root(r"HKCU\Software\SilkSampleTest"), Some((HKEY_CURRENT_USER, r"Software\SilkSampleTest")));
        assert_eq!(split_root(r"hkcu\Software\X"), Some((HKEY_CURRENT_USER, r"Software\X")));
        assert_eq!(split_root(r"HKLM\Software\X"), Some((HKEY_LOCAL_MACHINE, r"Software\X")));
        assert_eq!(split_root(r"HKCR\Software\X"), None);
        assert_eq!(split_root("HKC"), None);
    }

    #[test]
    fn scope_picks_the_registry_root_and_shortcut_folders() {
        assert_eq!(root(&Scope::User), HKEY_CURRENT_USER);
        assert_eq!(root(&Scope::Machine), HKEY_LOCAL_MACHINE);
        assert_eq!(shortcut_folders(&Scope::User), ["Programs", "Desktop"]);
        assert_eq!(shortcut_folders(&Scope::Machine), ["CommonPrograms", "CommonDesktopDirectory"]);
    }

    #[test]
    fn firewall_rule_becomes_netsh_arguments() {
        let config = silk_archive::config::parse(
            "product = \"S\"\nversion = \"1.0\"\npublisher = \"P\"\nscope = \"machine\"\nmain_exe = 's.exe'\npayload = \"p\"\n\
            [[firewall]]\nname = \"Sample server\"\nports = [7317, 7318]\nprotocol = \"tcp\"\nprofiles = [\"private\"]\n\
            [[firewall]]\nname = \"Sample app\"\nprogram = '{target}\\bin\\s.exe'\nprotocol = \"udp\"\nprofiles = [\"domain\", \"public\"]\n",
        )
        .unwrap();
        let target = Path::new(r"C:\Program Files\S");
        assert_eq!(firewall_add_args(&config.firewall[0], target).unwrap(), [
            "advfirewall", "firewall", "add", "rule", "name=Sample server", "dir=in", "action=allow", "enable=yes",
            "protocol=tcp", "profile=private", "localport=7317,7318",
        ]);
        assert_eq!(firewall_add_args(&config.firewall[1], target).unwrap(), [
            "advfirewall", "firewall", "add", "rule", "name=Sample app", "dir=in", "action=allow", "enable=yes",
            "protocol=udp", "profile=domain,public", r"program=C:\Program Files\S\bin\s.exe",
        ]);
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
