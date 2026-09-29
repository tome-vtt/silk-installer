// silk build <config.toml> [--out <file.exe>] [--stub <path>]
// Packs what the config names into a copy of the runtime stub (docs/spec.md § 3).

use silk_archive::{Archive, config};
use std::env;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: silk build <config.toml> [--out <file.exe>] [--stub <path>]";

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
    let (mut out, mut stub) = (None, None);
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--out" => &mut out,
            "--stub" => &mut stub,
            _ => return Err(USAGE.into()),
        };
        *slot = Some(PathBuf::from(args.next().ok_or(USAGE)?));
    }

    let at_config = |e: String| format!("{}: {e}", config_path.display());
    let text = fs::read_to_string(&config_path).map_err(|e| at_config(e.to_string()))?;
    let config = config::parse(&text).map_err(at_config)?;
    // Paths in the config resolve from its own folder.
    let dir = config_path.parent().unwrap_or(Path::new(""));
    let payload = dir.join(&config.payload);
    if !payload.join(&config.main_exe).is_file() {
        return Err(format!("main_exe {} is not in the payload {}", config.main_exe.display(), payload.display()));
    }
    let out = out.unwrap_or_else(|| dir.join(format!("{}-{}-setup.exe", config.product, config.version)));
    let stub = match stub {
        Some(s) => s,
        None => env::current_exe().map_err(|e| e.to_string())?.with_file_name("silk-stub.exe"),
    };
    if !stub.is_file() {
        return Err(format!("stub not found: {}", stub.display()));
    }

    let io = |e: std::io::Error| e.to_string();
    match &config.icon {
        None => silk_archive::build(&stub, &payload, &config, &out).map_err(io)?,
        Some(icon) => {
            // Resources change on a copy before the archive is appended: EndUpdateResource drops
            // anything past the image, so the stub is checked first.
            silk_archive::check_stub(&stub).map_err(io)?;
            let tmp = out.with_extension("stub.tmp");
            let result = fs::copy(&stub, &tmp)
                .map_err(io)
                .and_then(|_| set_icon(&tmp, &dir.join(icon)))
                .and_then(|_| silk_archive::build(&tmp, &payload, &config, &out).map_err(io));
            let _ = fs::remove_file(&tmp);
            result?;
        }
    }
    // Reopen what we wrote: the count comes from the finished file.
    let archive = Archive::open(&out).map_err(io)?.expect("archive just built");
    let len = fs::metadata(&out).map_err(io)?.len();
    println!("built {}: {} files, {len} bytes", out.display(), archive.payload_len());
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
    let num = |o: usize, n: usize| bytes.get(o..o + n).map(|b| b.iter().rev().fold(0usize, |a, &x| a << 8 | x as usize));
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
        images.push(bytes.get(offset..offset + size).ok_or_else(bad)?);
        group.extend(&bytes[e..e + 12]);
        group.extend((i as u16 + 1).to_le_bytes());
    }

    let (lang, old_ids) = main_icon_group(exe)?;
    // ponytail: new images take RT_ICON IDs 1..n, fine while the stub has this one icon group (tauri-build's).
    unsafe {
        let h = BeginUpdateResourceW(wide(exe).as_ptr(), 0);
        if h.is_null() {
            return Err(format!("{}: {}", exe.display(), std::io::Error::last_os_error()));
        }
        let mut ok = true;
        for (i, img) in images.iter().enumerate() {
            ok &= UpdateResourceW(h, RT_ICON, res_id(i as u16 + 1), lang, img.as_ptr().cast(), img.len() as u32) != 0;
        }
        for &id in old_ids.iter().filter(|&&id| id as usize > count) {
            ok &= UpdateResourceW(h, RT_ICON, res_id(id), lang, std::ptr::null(), 0) != 0;
        }
        ok &= UpdateResourceW(h, RT_GROUP_ICON, res_id(MAIN_ICON), lang, group.as_ptr().cast(), group.len() as u32) != 0;
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
        unsafe { *(out as *mut u16) = lang };
        0
    }
    unsafe {
        let flags = LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE;
        let m = LoadLibraryExW(wide(exe).as_ptr(), std::ptr::null_mut(), flags);
        if m.is_null() {
            return Err(format!("{}: {}", exe.display(), std::io::Error::last_os_error()));
        }
        let mut lang = u16::MAX;
        EnumResourceLanguagesW(m, RT_GROUP_ICON, res_id(MAIN_ICON), Some(first), &mut lang as *mut u16 as isize);
        let res = FindResourceExW(m, RT_GROUP_ICON, res_id(MAIN_ICON), lang);
        let ids = (!res.is_null()).then(|| {
            let data = LockResource(LoadResource(m, res)) as *const u8;
            let group = std::slice::from_raw_parts(data, SizeofResource(m, res) as usize);
            group.get(6..).unwrap_or_default().chunks_exact(14).map(|e| u16::from_le_bytes([e[12], e[13]])).collect()
        });
        FreeLibrary(m);
        ids.map(|ids| (lang, ids)).ok_or_else(|| format!("{} has no icon group {MAIN_ICON} to replace", exe.display()))
    }
}
