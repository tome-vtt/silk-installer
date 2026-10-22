// Shortcuts, the Installed Apps entry and the uninstaller (docs/spec.md § 5), generalised from Spider's
// registration.rs. Per-user installs use HKCU and the user's own folders, per-machine ones HKLM and the
// all-users folders.

use crate::install::{system, system_dir};
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
    // SAFETY: `id` is a live GUID, the token null (the current user), and `p` a live out-pointer.
    let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) };
    // SAFETY: on success `p` is a NUL-terminated string the shell allocated, read up to its NUL before it is
    // freed below.
    let path = (hr >= 0).then(|| unsafe {
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    });
    // SAFETY: `p` is null or the shell's allocation, which it asks freed whether or not the call succeeded; freed
    // once, after its last use.
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
        (
            "UninstallString",
            Value::Sz(format!("\"{}\" --uninstall", uninstaller_path(target, &config.product).display())),
        ),
        ("NoModify", Value::Dword(1)),
        ("NoRepair", Value::Dword(1)),
        ("EstimatedSize", Value::Dword(u32::try_from(size_bytes / 1024).unwrap_or(u32::MAX))),
        ("InstallDate", Value::Sz(date.into())),
    ]
}

/// Write the Installed Apps entry; the uninstaller it names is already in `target`.
pub fn register(config: &Config, target: &Path, size_bytes: u64) -> io::Result<()> {
    let key = uninstall_key(&config.product);
    for (name, value) in uninstall_values(config, target, size_bytes, &today()) {
        set_value(root(&config.scope), &key, name, &value)?;
    }
    Ok(())
}

/// Write a string value named as `HKCU\<subkey>`, `HKLM\<subkey>` or `HKU\<sid>\<subkey>`, creating the key.
pub fn set_sz(key: &str, name: &str, value: &str) -> io::Result<()> {
    let (root, sub) =
        split_root(key).ok_or_else(|| io::Error::other(format!("{key} is not an HKCU, HKLM or HKU key")))?;
    set_value(root, sub, name, &Value::Sz(value.into()))
}

/// The string value `name` under `HKCU\<subkey>`, `HKLM\<subkey>` or `HKU\<sid>\<subkey>`, if there is one.
#[cfg(test)]
pub fn get_sz(key: &str, name: &str) -> Option<String> {
    split_root(key).and_then(|(root, sub)| read_sz(root, sub, name))
}

fn set_value(root: HKEY, key: &str, name: &str, value: &Value) -> io::Result<()> {
    let data: Vec<u8> = match value {
        Value::Sz(s) => wide(s).iter().flat_map(|c| c.to_le_bytes()).collect(),
        Value::Dword(n) => n.to_le_bytes().to_vec(),
    };
    let kind = if let Value::Sz(_) = value { REG_SZ } else { REG_DWORD };
    let len = u32::try_from(data.len()).map_err(io::Error::other)?;
    // SAFETY: `wide(key)` and `wide(name)` are NUL-terminated UTF-16 that outlive the call, and `data` is a live
    // buffer of `len` bytes.
    let r = unsafe { RegSetKeyValueW(root, wide(key).as_ptr(), wide(name).as_ptr(), kind, data.as_ptr().cast(), len) };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
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
    // SAFETY: `key` and `name` are NUL-terminated UTF-16 that outlive `get`, and each call below passes either
    // null (asking the size) or a buffer of at least `len` bytes.
    let get = |buf: *mut u16, len: &mut u32| unsafe {
        RegGetValueW(root, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.cast(), len)
    };
    if get(std::ptr::null_mut(), &mut len) != 0 {
        return None;
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    if get(buf.as_mut_ptr(), &mut len) != 0 {
        return None;
    }
    buf.truncate(len as usize / 2);
    let s = String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string();
    (!s.is_empty()).then_some(s)
}

// Absent already (an uninstall run twice) is not worth failing over, as in Spider.
pub fn unregister(config: &Config) {
    // SAFETY: the root is a predefined key, and the subkey NUL-terminated UTF-16 that lives until the call returns.
    unsafe { RegDeleteTreeW(root(&config.scope), wide(uninstall_key(&config.product)).as_ptr()) };
}

/// Delete a value named by `HKCU\<subkey>`, (per-machine) `HKLM\<subkey>` or `HKU\<sid>\<subkey>`. In an
/// elevated run HKCU is the elevating account's, which is not the signed-in user's when another account
/// elevated; HKU names the signed-in user's hive.
pub fn delete_value(key: &str, name: &str) {
    if let Some((root, sub)) = split_root(key) {
        // SAFETY: the root is a predefined key, and both names NUL-terminated UTF-16 that live until the call returns.
        unsafe { RegDeleteKeyValueW(root, wide(sub).as_ptr(), wide(name).as_ptr()) };
    }
}

/// `key` as `delete_value` and `set_sz` should name it: `HKCU\<subkey>` becomes `HKU\<user_sid>\<subkey>` when the
/// signed-in user's SID is known (an elevated copy's HKCU may be another account's); anything else stays.
pub fn in_hive(key: &str, user_sid: Option<&str>) -> String {
    match (user_sid, key.split_once('\\')) {
        (Some(sid), Some((root, sub))) if root.eq_ignore_ascii_case("HKCU") => format!(r"HKU\{sid}\{sub}"),
        _ => key.to_string(),
    }
}

fn split_root(key: &str) -> Option<(HKEY, &str)> {
    let (root, sub) = key.split_once('\\')?;
    let root = match root.to_ascii_uppercase().as_str() {
        "HKCU" => HKEY_CURRENT_USER,
        "HKLM" => HKEY_LOCAL_MACHINE,
        "HKU" => HKEY_USERS,
        _ => return None,
    };
    Some((root, sub))
}

fn today() -> String {
    // SAFETY: SYSTEMTIME is eight u16s, for which all zeros is valid.
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: `t` is a live SYSTEMTIME for GetLocalTime to fill.
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

/// Registrations absent before this attempt; rollback removes these even when a folder backup exists.
#[derive(serde::Deserialize, Debug)]
pub struct NewRegistrations {
    shortcuts: Vec<PathBuf>,
    firewall: Vec<String>,
    entry: bool,
}

pub fn new_registrations(config: &Config) -> Result<NewRegistrations, String> {
    read_registration_snapshot(&registration_snapshot_script(config))
}

fn read_registration_snapshot(script: &str) -> Result<NewRegistrations, String> {
    let output = system(r"WindowsPowerShell\v1.0\powershell.exe")
        .and_then(|mut command| command.args(["-NoProfile", "-NonInteractive", "-Command", script]).output())
        .map_err(|e| format!("could not inspect existing registrations: {e}"))?;
    if !output.status.success() {
        return Err("could not inspect existing shortcuts, firewall rules or Installed Apps entry".into());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("could not read existing registrations: {e}"))
}

fn registration_snapshot_script(config: &Config) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let [programs, desktop] = shortcut_folders(&config.scope);
    let folders = [(programs, config.shortcuts.start_menu), (desktop, config.shortcuts.desktop)]
        .into_iter()
        .filter(|f| f.1)
        .map(|f| q(f.0))
        .collect::<Vec<_>>()
        .join(",");
    let names = config.firewall.iter().map(|r| q(&r.name)).collect::<Vec<_>>().join(",");
    let hive = match config.scope {
        Scope::User => "HKEY_CURRENT_USER",
        Scope::Machine => "HKEY_LOCAL_MACHINE",
    };
    let key = q(&format!("Registry::{hive}\\{}", uninstall_key(&config.product)));
    let link = q(&format!("{}.lnk", config.product));
    format!(
        "$ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); \
         $shortcuts = @(foreach ($f in @({folders})) {{ \
         $p = Join-Path ([Environment]::GetFolderPath($f)) {link}; \
         if (!(Test-Path -LiteralPath $p)) {{ $p }} }}); \
         $names = @({names}); $existing = @(); \
         if ($names.Count) {{ $existing = @((New-Object -ComObject HNetCfg.FwPolicy2).Rules | \
         Where-Object {{ $_.Direction -eq 1 }} | ForEach-Object {{ $_.Name }}) }}; \
         @{{ shortcuts = $shortcuts; firewall = @($names | Where-Object {{ $existing -notcontains $_ }}); \
         entry = !(Test-Path -LiteralPath {key}) }} | ConvertTo-Json -Compress"
    )
}

pub fn undo_new_registrations(config: &Config, new: &NewRegistrations) {
    undo_new_registrations_with(config, new, &mut run_netsh);
}

fn undo_new_registrations_with(config: &Config, new: &NewRegistrations, netsh: Netsh) {
    new.shortcuts.iter().for_each(|path| {
        let _ = std::fs::remove_file(path);
    });
    new.firewall.iter().for_each(|name| remove_firewall_rule(name, netsh));
    if new.entry {
        unregister(config);
    }
}

pub fn create_shortcuts(
    scope: &Scope,
    product: &str,
    exe: &Path,
    dir: &Path,
    start_menu: bool,
    desktop: bool,
    app_id: Option<&str>,
) -> io::Result<()> {
    let [programs, desktop_dir] = shortcut_folders(scope);
    let folders: Vec<&str> =
        [(programs, start_menu), (desktop_dir, desktop)].into_iter().filter(|f| f.1).map(|f| f.0).collect();
    if folders.is_empty() {
        return Ok(());
    }
    let ok = powershell(&shortcut_script(product, Some((exe, dir)), &folders, app_id))?.success();
    if ok { Ok(()) } else { Err(io::Error::other("PowerShell could not write the .lnk files")) }
}

/// Best effort, as in Spider: a locked .lnk must not stop the rest of the uninstall.
pub fn remove_shortcuts(scope: &Scope, product: &str) {
    let _ = powershell(&shortcut_script(product, None, &shortcut_folders(scope), None));
}

/// Sets `System.AppUserModel.ID` on a .lnk. WScript.Shell's shortcut has no such property, so this goes
/// through the shell link's IPropertyStore; PowerShell compiles it, as windows-sys has no COM bindings.
const SET_APP_ID: &str = r#"
using System; using System.Runtime.InteropServices;
public static class SilkAppId {
  [StructLayout(LayoutKind.Sequential, Pack = 4)] struct PK { public Guid fmtid; public uint pid; }
  [StructLayout(LayoutKind.Explicit)] struct PV { [FieldOffset(0)] public ushort vt; [FieldOffset(8)] public IntPtr p; }
  [ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IPropertyStore { uint GetCount(out uint c); uint GetAt(uint i, out PK k); uint GetValue(ref PK k, out PV v); uint SetValue(ref PK k, ref PV v); uint Commit(); }
  [ComImport, Guid("0000010B-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IPersistFile { void GetClassID(out Guid g); [PreserveSig] int IsDirty(); void Load([MarshalAs(UnmanagedType.LPWStr)] string f, uint m); void Save([MarshalAs(UnmanagedType.LPWStr)] string f, bool r); void SaveCompleted([MarshalAs(UnmanagedType.LPWStr)] string f); void GetCurFile([MarshalAs(UnmanagedType.LPWStr)] out string f); }
  public static void Set(string lnk, string id) {
    object link = Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid("00021401-0000-0000-C000-000000000046")));
    ((IPersistFile)link).Load(lnk, 2);
    PK key = new PK { fmtid = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"), pid = 5 };
    PV val = new PV { vt = 31, p = Marshal.StringToCoTaskMemUni(id) };
    IPropertyStore store = (IPropertyStore)link;
    if (store.SetValue(ref key, ref val) != 0 || store.Commit() != 0) throw new Exception("could not set the app identity");
    ((IPersistFile)link).Save(lnk, true);
  }
}
"#;

/// The PowerShell before the loop that compiles `SET_APP_ID`, and the call after each save; empty with no id.
fn app_id_script(app_id: Option<&str>, lnk: &str, q: impl Fn(&str) -> String) -> (String, String) {
    let Some(id) = app_id else { return (String::new(), String::new()) };
    (format!("Add-Type -TypeDefinition @'{SET_APP_ID}'@; "), format!("; [SilkAppId]::Set({lnk}, {})", q(id)))
}

fn powershell(script: &str) -> io::Result<std::process::ExitStatus> {
    system(r"WindowsPowerShell\v1.0\powershell.exe")?
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .status()
}

/// PowerShell that writes `<product>.lnk` (to `exe`, starting in `dir`) into each special folder, or
/// removes it when `link` is None. WScript.Shell's shortcut writer, as Spider uses: no COM binding needed.
fn shortcut_script(product: &str, link: Option<(&Path, &Path)>, folders: &[&str], app_id: Option<&str>) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let path = |p: &Path| q(&p.display().to_string());
    let list = folders.iter().map(|f| q(f)).collect::<Vec<_>>().join(",");
    let lnk = format!("(Join-Path ([Environment]::GetFolderPath($f)) {})", q(&format!("{product}.lnk")));
    let (setup, set_id) = app_id_script(app_id, &lnk, q);
    let body = match link {
        Some((exe, dir)) => format!(
            "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({lnk}); $s.TargetPath = {}; $s.WorkingDirectory = {}; $s.Save(){set_id}",
            path(exe),
            path(dir)
        ),
        None => format!("Remove-Item -LiteralPath {lnk} -ErrorAction SilentlyContinue"),
    };
    format!("$ErrorActionPreference = 'Stop'; {setup}foreach ($f in {list}) {{ {body} }}")
}

/// Add each `[[firewall]]` rule, replacing any inbound rule of the same name so an upgrade leaves one copy.
/// Every new rule is added under a temporary name first and renamed only once all have been added, so a
/// failed add deletes only the temporary rules and leaves the previous version's rules as they were.
/// netsh rather than the INetFwPolicy2 COM API: windows-sys has no COM interface bindings, and the stub
/// already runs PowerShell and cmd for the same reason. Needs the elevated per-machine run.
// ponytail: a rule renamed or dropped by an upgrade stays behind; the old config is not at hand to remove it,
// so record the installed rule names if that matters.
// A rename failing partway (a local netsh call right after a successful add) still loses the old rules it reached;
// keep each old rule under a backup name until every rename succeeds if that matters.
pub fn add_firewall_rules(config: &Config, target: &Path) -> Result<(), String> {
    add_firewall_rules_with(config, target, &mut run_netsh)
}

/// netsh as the firewall functions call it: its arguments, and what to say if it fails. Tests swap in a fake.
type Netsh<'a> = &'a mut dyn FnMut(Vec<String>, &str) -> Result<(), String>;

fn add_firewall_rules_with(config: &Config, target: &Path, netsh: Netsh) -> Result<(), String> {
    let result = add_temporary_rules(config, target, netsh).and_then(|()| rename_temporary_rules(config, netsh));
    if result.is_err() {
        config.firewall.iter().for_each(|r| remove_firewall_rule(&temporary_name(&r.name), netsh));
    }
    result
}

fn rename_temporary_rules(config: &Config, netsh: Netsh) -> Result<(), String> {
    for rule in &config.firewall {
        remove_firewall_rule(&rule.name, netsh);
        netsh(firewall_rename_args(&rule.name), &format!("could not rename the firewall rule {:?}", rule.name))?;
    }
    Ok(())
}

fn add_temporary_rules(config: &Config, target: &Path, netsh: Netsh) -> Result<(), String> {
    for rule in &config.firewall {
        let temporary = temporary_name(&rule.name);
        remove_firewall_rule(&temporary, netsh);
        let args = firewall_add_args(rule, &temporary, target)?;
        netsh(args, &format!("could not add the firewall rule {:?}", rule.name))?;
    }
    Ok(())
}

fn run_netsh(args: Vec<String>, what: &str) -> Result<(), String> {
    let status = system("netsh.exe")
        .and_then(|mut netsh| netsh.args(args).status())
        .map_err(|e| format!("could not run netsh: {e}"))?;
    if status.success() {
        return Ok(());
    }
    Err(format!("{what} (netsh exit code {}).", status.code().unwrap_or(-1)))
}

fn temporary_name(name: &str) -> String {
    format!("{name} (silk upgrade)")
}

/// netsh's arguments for renaming the temporary rule of `name` to `name`.
fn firewall_rename_args(name: &str) -> Vec<String> {
    let (from, to) = (format!("name={}", temporary_name(name)), format!("name={name}"));
    ["advfirewall", "firewall", "set", "rule", &from, "dir=in", "new", &to].map(String::from).to_vec()
}

/// Best effort: a rule already gone (removed by hand, or an uninstall run twice) is not worth failing over.
pub fn remove_firewall_rules(config: &Config) {
    config.firewall.iter().for_each(|r| remove_firewall_rule(&r.name, &mut run_netsh));
}

fn remove_firewall_rule(name: &str, netsh: Netsh) {
    let args = ["advfirewall", "firewall", "delete", "rule", &format!("name={name}"), "dir=in"].map(String::from);
    let _ = netsh(args.to_vec(), "");
}

/// netsh's arguments for adding `rule` as `name`, `{target}` filled into its program. std quotes an argument
/// with spaces as `"name=A b"`, which netsh reads as the name `A b`.
fn firewall_add_args(rule: &FirewallRule, name: &str, target: &Path) -> Result<Vec<String>, String> {
    let lower = |v: &dyn std::fmt::Debug| format!("{v:?}").to_lowercase();
    let mut args = ["advfirewall", "firewall", "add", "rule"].map(String::from).to_vec();
    args.push(format!("name={name}"));
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
    // means nothing to cmd. The cwd must not be the folder being removed. ping by its full path: cmd would
    // look for it in that cwd, the user's temp folder, first.
    let (dir, ping) = (target.display(), system_dir()?.join("ping.exe"));
    let ping = ping.display();
    system("cmd.exe")?
        .arg("/C")
        .raw_arg(format!("for /l %i in (1,1,10) do @(\"{ping}\" 127.0.0.1 -n 2 > nul & rmdir /s /q \"{dir}\" 2> nul & if not exist \"{dir}\" exit)"))
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
        assert_eq!(
            v,
            [
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
            ]
        );
    }

    #[test]
    fn uninstall_key_is_per_product() {
        assert_eq!(uninstall_key("Silk Sample"), r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Silk Sample");
    }

    #[test]
    fn hkcu_keys_move_to_the_users_hive_only_when_the_sid_is_known() {
        let sid = "S-1-5-21-1-2-3-1001";
        assert_eq!(in_hive(r"HKCU\Software\X", Some(sid)), format!(r"HKU\{sid}\Software\X"));
        assert_eq!(in_hive(r"hkcu\Software\X", Some(sid)), format!(r"HKU\{sid}\Software\X"));
        assert_eq!(in_hive(r"HKCU\Software\X", None), r"HKCU\Software\X");
        assert_eq!(in_hive(r"HKLM\Software\X", Some(sid)), r"HKLM\Software\X");
        assert!(split_root(&in_hive(r"HKCU\Software\X", Some(sid))).is_some_and(|(root, _)| root == HKEY_USERS));
    }

    #[test]
    fn root_prefix_is_split_off_case_insensitively() {
        assert_eq!(split_root(r"HKCU\Software\SilkSampleTest"), Some((HKEY_CURRENT_USER, r"Software\SilkSampleTest")));
        assert_eq!(split_root(r"hkcu\Software\X"), Some((HKEY_CURRENT_USER, r"Software\X")));
        assert_eq!(split_root(r"HKLM\Software\X"), Some((HKEY_LOCAL_MACHINE, r"Software\X")));
        assert_eq!(split_root(r"hku\S-1-5-21-1\Software\X"), Some((HKEY_USERS, r"S-1-5-21-1\Software\X")));
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
    fn firewall_upgrade_renames_the_temporary_rule_over_the_old_name() {
        assert_eq!(temporary_name("Sample server"), "Sample server (silk upgrade)");
        assert_eq!(
            firewall_rename_args("Sample server"),
            [
                "advfirewall",
                "firewall",
                "set",
                "rule",
                "name=Sample server (silk upgrade)",
                "dir=in",
                "new",
                "name=Sample server",
            ]
        );
    }

    /// A netsh that keeps inbound rule names, failing the `fail_on`th add (0-based).
    fn fake_netsh(
        rules: &mut Vec<String>,
        fail_on: Option<usize>,
    ) -> impl FnMut(Vec<String>, &str) -> Result<(), String> {
        let mut adds = 0;
        move |args, _| {
            let name = args[4].strip_prefix("name=").unwrap().to_string();
            match args[2].as_str() {
                "delete" => rules.retain(|r| *r != name),
                "add" if fail_on == Some(adds) => return Err("add failed".into()),
                "add" => {
                    adds += 1;
                    rules.push(name);
                }
                _ => {
                    let new = args[7].strip_prefix("name=").unwrap().to_string();
                    rules.iter_mut().filter(|r| **r == name).for_each(|r| *r = new.clone());
                }
            }
            Ok(())
        }
    }

    fn two_rules() -> Config {
        silk_archive::config::parse(
            "product = \"S\"\nversion = \"1.0\"\npublisher = \"P\"\nscope = \"machine\"\nmain_exe = 's.exe'\npayload = \"p\"\n\
            [[firewall]]\nname = \"A\"\nports = [1]\nprotocol = \"tcp\"\nprofiles = [\"private\"]\n\
            [[firewall]]\nname = \"B\"\nports = [2]\nprotocol = \"tcp\"\nprofiles = [\"private\"]\n",
        )
        .unwrap()
    }

    #[test]
    fn failed_firewall_add_keeps_the_previous_rules_and_leaves_no_temporary_one() {
        let mut rules = vec!["A".to_string(), "B".to_string()];
        let result = add_firewall_rules_with(&two_rules(), Path::new(TARGET), &mut fake_netsh(&mut rules, Some(1)));
        assert!(result.is_err());
        assert_eq!(rules, ["A", "B"]);
    }

    #[test]
    fn rollback_removes_only_shortcuts_rules_and_entry_absent_before_registration() {
        let mut config = two_rules();
        config.scope = Scope::User;
        config.product = format!("Silk snapshot é's {}", std::process::id());
        config.shortcuts.desktop = true;
        let target = Path::new(TARGET);
        create_shortcuts(&config.scope, &config.product, &target.join("s.exe"), target, true, false, None).unwrap();
        register(&config, target, 0).unwrap();
        // Fake only the firewall COM boundary: A exists inbound, B exists only outbound.
        let script = format!(
            "function New-Object {{ param($ComObject) [pscustomobject]@{{ Rules = @(\
             [pscustomobject]@{{ Name = 'A'; Direction = 1 }}, \
             [pscustomobject]@{{ Name = 'B'; Direction = 2 }}) }} }}; {}",
            registration_snapshot_script(&config)
        );
        let new = read_registration_snapshot(&script).unwrap();
        assert_eq!(new.firewall, ["B"]);
        assert!(!new.entry);
        assert_eq!(new.shortcuts.len(), 1);
        let desktop = &new.shortcuts[0];
        create_shortcuts(&config.scope, &config.product, &target.join("s.exe"), target, true, true, None).unwrap();
        assert!(desktop.exists());
        let mut rules = vec!["A".to_string()];
        add_firewall_rules_with(&config, target, &mut fake_netsh(&mut rules, None)).unwrap();
        undo_new_registrations_with(&config, &new, &mut fake_netsh(&mut rules, None));
        let after = read_registration_snapshot(&script).unwrap();
        let entry = installed(&config);
        remove_shortcuts(&config.scope, &config.product);
        unregister(&config);
        assert!(!desktop.exists());
        assert_eq!(after.shortcuts, new.shortcuts); // Existing Start menu link survived.
        assert!(entry.0.is_some());
        assert_eq!(rules, ["A"]);
    }

    #[test]
    fn a_failed_snapshot_does_not_return_a_cleanup_plan() {
        assert!(read_registration_snapshot("throw 'inspection failed'").is_err());
    }

    #[test]
    fn successful_firewall_upgrade_ends_with_one_rule_of_each_name() {
        let mut rules = vec!["A".to_string(), "B".to_string()];
        add_firewall_rules_with(&two_rules(), Path::new(TARGET), &mut fake_netsh(&mut rules, None)).unwrap();
        rules.sort();
        assert_eq!(rules, ["A", "B"]);
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
        assert_eq!(
            firewall_add_args(&config.firewall[0], "Sample server", target).unwrap(),
            [
                "advfirewall",
                "firewall",
                "add",
                "rule",
                "name=Sample server",
                "dir=in",
                "action=allow",
                "enable=yes",
                "protocol=tcp",
                "profile=private",
                "localport=7317,7318",
            ]
        );
        assert_eq!(
            firewall_add_args(&config.firewall[1], "Sample app", target).unwrap(),
            [
                "advfirewall",
                "firewall",
                "add",
                "rule",
                "name=Sample app",
                "dir=in",
                "action=allow",
                "enable=yes",
                "protocol=udp",
                "profile=domain,public",
                r"program=C:\Program Files\S\bin\s.exe",
            ]
        );
    }

    #[test]
    fn shortcut_script_quotes_every_string() {
        let s = shortcut_script(
            "Bart's App",
            Some((Path::new(r"C:\A'pp\a.exe"), Path::new(r"C:\A'pp"))),
            &["Programs", "Desktop"],
            None,
        );
        assert_eq!(
            s,
            "$ErrorActionPreference = 'Stop'; foreach ($f in 'Programs','Desktop') { \
            $s = (New-Object -ComObject WScript.Shell).CreateShortcut((Join-Path ([Environment]::GetFolderPath($f)) 'Bart''s App.lnk')); \
            $s.TargetPath = 'C:\\A''pp\\a.exe'; $s.WorkingDirectory = 'C:\\A''pp'; $s.Save() }"
        );
        let s = shortcut_script("X", None, &["Programs"], None);
        assert_eq!(
            s,
            "$ErrorActionPreference = 'Stop'; foreach ($f in 'Programs') { \
            Remove-Item -LiteralPath (Join-Path ([Environment]::GetFolderPath($f)) 'X.lnk') -ErrorAction SilentlyContinue }"
        );
    }

    /// Writes a real per-user Start menu shortcut named for this test, reads `System.AppUserModel.ID` back
    /// through the shell (not the code that wrote it), and removes it again.
    fn written_app_id(product: &str, app_id: Option<&str>) -> String {
        let exe = std::env::current_exe().unwrap();
        create_shortcuts(&Scope::User, product, &exe, exe.parent().unwrap(), true, false, app_id).unwrap();
        let read = format!(
            "$d = [Environment]::GetFolderPath('Programs'); \
            (New-Object -ComObject Shell.Application).NameSpace($d).ParseName('{product}.lnk').ExtendedProperty('System.AppUserModel.ID')"
        );
        let out = system(r"WindowsPowerShell\v1.0\powershell.exe")
            .unwrap()
            .args(["-NoProfile", "-NonInteractive", "-Command", &read])
            .output();
        remove_shortcuts(&Scope::User, product);
        String::from_utf8(out.unwrap().stdout).unwrap().trim().to_string()
    }

    #[test]
    fn shortcut_carries_the_configured_app_id() {
        assert_eq!(written_app_id("Silk AppId Test With", Some("Silk.Test.App's.1")), "Silk.Test.App's.1");
    }

    #[test]
    fn shortcut_without_an_app_id_has_none() {
        assert_eq!(written_app_id("Silk AppId Test Without", None), "");
    }
}
