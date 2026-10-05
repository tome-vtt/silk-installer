// silk build <config.toml> [--out <file.exe>] [--stub <path>] [--version <x.y.z>] [--no-sign]
// Packs what the config names into a copy of the runtime stub (docs/spec.md § 3).

use silk_archive::{Archive, config};
use std::env;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const USAGE: &str =
    "usage: silk build <config.toml> [--out <file.exe>] [--stub <path>] [--version <x.y.z>] [--no-sign]";

fn main() {
    if let Err(e) = run(env::args().skip(1).collect()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some("build") {
        return Err(USAGE.into());
    }
    let config_path = PathBuf::from(args.next().ok_or(USAGE)?);
    let (mut out, mut stub, mut version, mut no_sign) = (None, None, None, false);
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--no-sign" => {
                no_sign = true;
                continue;
            }
            "--version" => {
                version = Some(args.next().ok_or(USAGE)?);
                continue;
            }
            "--out" => &mut out,
            "--stub" => &mut stub,
            _ => return Err(USAGE.into()),
        };
        *slot = Some(PathBuf::from(args.next().ok_or(USAGE)?));
    }

    let at_config = |e: String| format!("{}: {e}", config_path.display());
    let text = fs::read_to_string(&config_path).map_err(|e| at_config(e.to_string()))?;
    let mut config = config::parse(&text).map_err(at_config)?;
    // Before anything reads it: the file name, the packed config and so the uninstaller and registry.
    if let Some(v) = version {
        config.set_version(v).map_err(|e| format!("--version: {e}"))?;
    }
    // Paths in the config resolve from its own folder.
    let dir = config_path.parent().unwrap_or(Path::new(""));
    let payload = dir.join(&config.payload);
    check_payload(&config, &payload)?;
    let theme = config.theme.as_ref().map(|t| dir.join(t));
    let out = out.unwrap_or_else(|| dir.join(format!("{}-{}-setup.exe", config.product, config.version)));
    let stub = match stub {
        Some(s) => s,
        None => env::current_exe().map_err(|e| e.to_string())?.with_file_name("silk-stub.exe"),
    };
    if !stub.is_file() {
        return Err(format!("stub not found: {}", stub.display()));
    }

    let io = |e: std::io::Error| e.to_string();
    let sign = config.sign.as_ref().filter(|_| !no_sign);
    // Checked first: resources change on a copy (EndUpdateResource drops anything past the image).
    silk_archive::check_stub(&stub).map_err(io)?;
    let patched = out.with_extension("stub.tmp");
    // Named .exe so signing tools treat it as one.
    let uninstaller = out.with_extension("uninstaller.exe");
    let result = (|| {
        let stub = match &config.icon {
            None => stub.clone(),
            Some(icon) => {
                fs::copy(&stub, &patched).map_err(io)?;
                set_icon(&patched, &dir.join(icon))?;
                patched.clone()
            }
        };
        // The uninstaller first (same stub, theme and config, no payload), signed before it is packed:
        // the installer extracts it as is, so it stays signed (docs/spec.md § 8).
        silk_archive::build(&stub, None, theme.as_deref(), &config, &uninstaller).map_err(io)?;
        if let Some(sign) = sign {
            sign_file(sign, dir, &uninstaller)?;
        }
        silk_archive::build(&stub, Some((&payload, &uninstaller)), theme.as_deref(), &config, &out).map_err(io)?;
        if let Some(sign) = sign {
            sign_file(sign, dir, &out)?;
        }
        Ok::<_, String>(())
    })();
    let _ = fs::remove_file(&patched);
    let _ = fs::remove_file(&uninstaller);
    result?;
    let signed = if sign.is_some() { ", installer and uninstaller signed" } else { "" };
    // Reopen what we wrote: the count comes from the finished file, and a signed one must still open.
    let archive = Archive::open(&out).map_err(io)?.expect("archive just built");
    let len = fs::metadata(&out).map_err(io)?.len();
    let theme = theme.map_or("the default theme".into(), |t| format!("theme {}", t.display()));
    println!("built {}: {} files, {theme}, {len} bytes{signed}", out.display(), archive.payload_len());
    Ok(())
}

/// Refuse a build whose main exe, or a file a before-install program names by `{payload}`, the payload lacks.
fn check_payload(config: &config::Config, payload: &Path) -> Result<(), String> {
    if !payload.join(&config.main_exe).is_file() {
        return Err(format!("main_exe {} is not in the payload {}", config.main_exe.display(), payload.display()));
    }
    match config.run.iter().flat_map(|r| r.payload_files()).find(|f| !payload.join(f).is_file()) {
        Some(f) => Err(format!("run: {{payload}}\\{f} is not in the payload {}", payload.display())),
        None => Ok(()),
    }
}

/// Run the config's `[sign]` command on `exe`, from the config's folder `dir`; its output goes straight
/// to ours, so a failure shows why. A failure removes `exe`, so no unsigned file is left where a signed
/// one is expected.
fn sign_file(sign: &config::Sign, dir: &Path, exe: &Path) -> Result<(), String> {
    let exe = std::path::absolute(exe).map_err(|e| e.to_string())?;
    let (command, args) = sign.expand(&exe.to_string_lossy())?;
    // A relative command with a folder in it means one next to the config, like every other config path
    // (joining an absolute one keeps it as it is); a bare name is looked up on PATH.
    let program = if command.contains(['\\', '/']) { dir.join(&command) } else { PathBuf::from(&command) };
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let status = std::process::Command::new(&program)
        .args(&args)
        .current_dir(dir)
        .status()
        .map_err(|e| format!("sign: could not run {command}: {e}"))?;
    if !status.success() {
        let _ = fs::remove_file(&exe);
        return Err(format!(
            "sign: {command} failed (exit code {}); {} was removed",
            status.code().unwrap_or(-1),
            exe.display()
        ));
    }
    Ok(())
}

/// The stub's main icon group: the ID tauri-build embeds and Tauri loads the window icon from.
const MAIN_ICON: u16 = 32512;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain([0]).collect()
}

fn res_id(n: u16) -> windows_sys::core::PCWSTR {
    n as usize as _
}

/// Replace the main icon group in `exe` with the images in the `.ico` file `ico`.
fn set_icon(exe: &Path, ico: &Path) -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{RT_GROUP_ICON, RT_ICON};

    let bytes = fs::read(ico).map_err(|e| format!("icon {}: {e}", ico.display()))?;
    let bad = || format!("icon {}: not a valid .ico file", ico.display());
    let num =
        |o: usize, n: usize| bytes.get(o..o + n).map(|b| b.iter().rev().fold(0usize, |a, &x| a << 8 | x as usize));
    if num(0, 2) != Some(0) || num(2, 2) != Some(1) || num(4, 2).is_none_or(|n| n == 0) {
        return Err(bad());
    }
    // RT_GROUP_ICON is the .ico directory with each entry's file offset swapped for an RT_ICON ID.
    let count = num(4, 2).unwrap();
    let mut group = bytes[..6].to_vec();
    let mut images = Vec::new();
    for i in 0..count {
        let e = 6 + 16 * i;
        let (size, offset) = (num(e + 8, 4).ok_or_else(bad)?, num(e + 12, 4).ok_or_else(bad)?);
        let (id, len) = (u16::try_from(i + 1).map_err(|_| bad())?, u32::try_from(size).map_err(|_| bad())?);
        images.push((id, len, bytes.get(offset..offset + size).ok_or_else(bad)?));
        group.extend(&bytes[e..e + 12]);
        group.extend(id.to_le_bytes());
    }
    let group_len = u32::try_from(group.len()).map_err(|_| bad())?;

    let (lang, old_ids) = main_icon_group(exe)?;
    // ponytail: new images take RT_ICON IDs 1..n, fine while the stub has this one icon group (tauri-build's).
    // SAFETY: `wide(exe)` is a NUL-terminated UTF-16 path that lives until BeginUpdateResourceW returns. `h` is
    // used only once checked non-null, and EndUpdateResourceW releases it on every path past that check. Each
    // UpdateResourceW gets a pointer and length from one live slice (`img`, `group`), which outlives the call,
    // or null and 0, which deletes the resource.
    unsafe {
        let h = BeginUpdateResourceW(wide(exe).as_ptr(), 0);
        if h.is_null() {
            return Err(format!("{}: {}", exe.display(), std::io::Error::last_os_error()));
        }
        let mut ok = true;
        for &(id, len, img) in &images {
            ok &= UpdateResourceW(h, RT_ICON, res_id(id), lang, img.as_ptr().cast(), len) != 0;
        }
        for &id in old_ids.iter().filter(|&&id| id as usize > count) {
            ok &= UpdateResourceW(h, RT_ICON, res_id(id), lang, std::ptr::null(), 0) != 0;
        }
        ok &= UpdateResourceW(h, RT_GROUP_ICON, res_id(MAIN_ICON), lang, group.as_ptr().cast(), group_len) != 0;
        let err = std::io::Error::last_os_error();
        if EndUpdateResourceW(h, !ok as i32) == 0 || !ok {
            return Err(format!("could not set the icon of {}: {err}", exe.display()));
        }
    }
    Ok(())
}

/// The language and RT_ICON IDs of the main icon group in `exe`.
fn main_icon_group(exe: &Path) -> Result<(u16, Vec<u16>), String> {
    use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::RT_GROUP_ICON;
    use windows_sys::core::{BOOL, PCWSTR};

    unsafe extern "system" fn first(_: HMODULE, _: PCWSTR, _: PCWSTR, lang: u16, out: isize) -> BOOL {
        // SAFETY: `out` is the `&mut lang` main_icon_group passes EnumResourceLanguagesW, a live, aligned u16
        // for the whole enumeration, which is the only time this runs.
        unsafe { *(out as *mut u16) = lang };
        0
    }
    // SAFETY: `wide(exe)` is a NUL-terminated UTF-16 path that lives until LoadLibraryExW returns. `m` is used
    // only once checked non-null, and freed last. The resource is read only when FindResourceExW found it and
    // LockResource returned non-null: then it is SizeofResource bytes inside `m`'s mapping, copied out into
    // `ids` before FreeLibrary unmaps it.
    unsafe {
        let flags = LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE;
        let m = LoadLibraryExW(wide(exe).as_ptr(), std::ptr::null_mut(), flags);
        if m.is_null() {
            return Err(format!("{}: {}", exe.display(), std::io::Error::last_os_error()));
        }
        let mut lang = u16::MAX;
        EnumResourceLanguagesW(m, RT_GROUP_ICON, res_id(MAIN_ICON), Some(first), &mut lang as *mut u16 as isize);
        let res = FindResourceExW(m, RT_GROUP_ICON, res_id(MAIN_ICON), lang);
        let data = if res.is_null() { std::ptr::null() } else { LockResource(LoadResource(m, res)).cast::<u8>() };
        let ids = (!data.is_null()).then(|| {
            let group = std::slice::from_raw_parts(data, SizeofResource(m, res) as usize);
            group
                .get(6..)
                .unwrap_or_default()
                .as_chunks::<14>()
                .0
                .iter()
                .map(|e| u16::from_le_bytes([e[12], e[13]]))
                .collect()
        });
        FreeLibrary(m);
        ids.map(|ids| (lang, ids)).ok_or_else(|| format!("{} has no icon group {MAIN_ICON} to replace", exe.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn before_install_files_must_be_in_the_payload() {
        let payload = env::temp_dir().join(format!("silk-compiler-payload-{}", std::process::id()));
        let _ = fs::remove_dir_all(&payload);
        fs::create_dir_all(payload.join("tools")).unwrap();
        fs::write(payload.join("sample.exe"), "").unwrap();
        let config = config::parse(&format!(
            "{}\n[[run]]\nprogram = '{{payload}}\\tools\\clean.cmd'\nat = \"before-install\"\n",
            include_str!("../../examples/minimal.toml")
        ))
        .unwrap();
        let e = check_payload(&config, &payload).unwrap_err();
        assert!(e.starts_with(r"run: {payload}\tools\clean.cmd is not in the payload"), "{e}");
        fs::write(payload.join(r"tools\clean.cmd"), "").unwrap();
        check_payload(&config, &payload).unwrap();
        fs::remove_dir_all(payload).unwrap();
    }
}
