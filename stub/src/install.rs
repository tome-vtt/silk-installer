// The install, upgrade and uninstall actions (docs/spec.md §§ 5, 6), generalised from Spider's install.rs.
// Each action takes the target and its settings as arguments, so a theme can later override the config's defaults.

use crate::registration;
use silk_archive::Archive;
use silk_archive::config::{Config, Run, RunAt, expand};
use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The error a cancelled install reports; the page tells it from real failures.
pub const CANCELLED: &str = "cancelled";

/// How long the running app gets to exit before the install gives up, files untouched.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// A helper process without a console window (the stub itself has none to share).
pub fn hidden(program: impl AsRef<OsStr>) -> Command {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut c = Command::new(program);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// Copy the payload into `target` through a staging folder, then run the after-install programs, add the
/// shortcuts and register. Any failure puts the previous install back; `progress` returning false cancels.
pub fn install(archive: &mut Archive, config: &Config, target: &Path, progress: impl FnMut(usize, usize, &str) -> bool) -> Result<(), String> {
    let product = &config.product;
    let fail = |e: io::Error| format!("{product} could not finish installing ({e}). Close any programs that might be using these files, then try again.");
    let parent = target.parent().ok_or_else(|| format!("cannot install into {}", target.display()))?;
    fs::create_dir_all(parent).map_err(fail)?;
    let size = archive.payload_size() + archive.stub_len();
    let needed = space_needed(archive.payload_size(), archive.stub_len());
    if free_space(parent).map_err(fail)? < needed {
        let drive = target.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
        return Err(format!("Drive {drive} is too full. {product} needs about {} MB free. Free up some space, then try again.", needed >> 20));
    }

    // Staged next to the target (same volume, so the swap is a rename); nothing partial is ever at `target`.
    let staging = parent.join(format!(".silk-installing-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    if let Err(e) = archive.extract_payload(&staging, progress) {
        let _ = fs::remove_dir_all(&staging);
        return Err(if e.kind() == io::ErrorKind::Interrupted { CANCELLED.into() } else { fail(e) });
    }

    let was_running = if config.close_running {
        close_running(target).inspect_err(|_| {
            let _ = fs::remove_dir_all(&staging);
        })?
    } else {
        false
    };
    let result = match swap_in(target, &staging) {
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            Err(fail(e))
        }
        Ok(backup) => match finish(config, target, size) {
            Ok(()) => {
                if let Some(b) = backup {
                    let _ = fs::remove_dir_all(b);
                }
                Ok(())
            }
            Err(e) => {
                roll_back(target, backup.as_deref());
                Err(e)
            }
        },
    };
    // As Spider does: an app we stopped comes back, whichever version ended up in place.
    if result.is_err() && was_running {
        let _ = launch(target, &config.main_exe);
    }
    result
}

// ponytail: rollback restores the files only; a failure inside register() can leave the entry half-updated.
fn finish(config: &Config, target: &Path, size: u64) -> Result<(), String> {
    for run in config.run.iter().filter(|r| r.at == RunAt::AfterInstall) {
        run_program(run, target)?;
    }
    let s = &config.shortcuts;
    registration::create_shortcuts(&config.product, &target.join(&config.main_exe), target, s.start_menu, s.desktop)
        .map_err(|e| format!("could not create the shortcuts: {e}"))?;
    registration::register(config, target, size).map_err(|e| format!("could not register {}: {e}", config.product))
}

/// The update path (§ 6): no window, a no-op when the same or a newer version is installed, one line in
/// `install.log`, and the app launched afterwards as Spider's updater expects. True on success.
pub fn silent(archive: &mut Archive, config: &Config, target: &Path, installed: Option<&str>) -> bool {
    let version = &config.version;
    if let Some(v) = installed.filter(|v| already_current(Some(v), version)) {
        log_line(target, &format!("silent: {v} installed, {version} is not newer, nothing to do"));
        return true;
    }
    match install(archive, config, target, |_, _, _| true) {
        Ok(()) => {
            log_line(target, &format!("silent: installed {version}"));
            if config.launch_at_finish {
                let _ = launch(target, &config.main_exe);
            }
            true
        }
        Err(e) => {
            log_line(target, &format!("silent: install failed: {e}"));
            false
        }
    }
}

/// Undo the install at `target`: run by the uninstaller copy with `--uninstall`.
// ponytail: windowless like Spider's; the themed uninstall (confirm, progress, done) is step 4.
pub fn uninstall(config: &Config, target: &Path) -> Result<(), String> {
    // Refuse to empty a folder that is not ours (a stale or edited InstallLocation).
    if !registration::uninstaller_path(target, &config.product).is_file() {
        return Err(format!("{} does not hold an install of {}.", target.display(), config.product));
    }
    for run in config.run.iter().filter(|r| r.at == RunAt::BeforeUninstall) {
        run_program(run, target)?;
    }
    // Always, whatever close_running says: files cannot be removed from under a running app.
    close_running(target)?;
    registration::remove_shortcuts(&config.product);
    for v in &config.remove_registry_value {
        registration::delete_value(&v.key, &v.name);
    }
    registration::unregister(&config.product);
    registration::self_delete(target).map_err(|e| e.to_string())
}

pub fn launch(target: &Path, main_exe: &Path) -> io::Result<()> {
    Command::new(target.join(main_exe)).current_dir(target).spawn().map(drop)
}

/// Stop every process running from inside `dir` (the app and any helpers it ships), except this one, and
/// wait for them to exit. Ok(true) when one was running; an error when one is still running at the timeout.
fn close_running(dir: &Path) -> Result<bool, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::*;
    const GONE: i32 = 87; // ERROR_INVALID_PARAMETER: the process exited since we listed it.
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    let (mut closed, mut stuck) = (false, None);
    for (pid, exe) in processes().into_iter().filter(|(pid, exe)| *pid != std::process::id() && inside(exe, dir)) {
        let h = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
        if h.is_null() {
            if io::Error::last_os_error().raw_os_error() != Some(GONE) {
                stuck = Some(exe);
            }
            continue;
        }
        let left = deadline.saturating_duration_since(Instant::now()).as_millis() as u32;
        // Terminating an already exiting process fails harmlessly; the wait decides.
        let exited = unsafe {
            TerminateProcess(h, 1);
            WaitForSingleObject(h, left) == WAIT_OBJECT_0
        };
        unsafe { CloseHandle(h) };
        if exited { closed = true } else { stuck = Some(exe) }
    }
    match stuck {
        None => Ok(closed),
        Some(exe) => Err(format!("{} is still running and did not close within {} s. Close it, then try again.", exe.display(), CLOSE_TIMEOUT.as_secs())),
    }
}

/// Every process whose image path this user can read, as (pid, exe).
fn processes() -> Vec<(u32, PathBuf)> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
    use windows_sys::Win32::System::Threading::*;
    let mut pids = vec![0u32; 1024];
    loop {
        let (bytes, mut used) = ((pids.len() * 4) as u32, 0u32);
        if unsafe { EnumProcesses(pids.as_mut_ptr(), bytes, &mut used) } == 0 {
            return Vec::new();
        }
        if used < bytes {
            pids.truncate(used as usize / 4);
            break;
        }
        pids.resize(pids.len() * 2, 0);
    }
    let mut buf = vec![0u16; 32768];
    pids.into_iter()
        .filter_map(|pid| unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
            CloseHandle(h);
            ok.then(|| (pid, OsString::from_wide(&buf[..len as usize]).into()))
        })
        .collect()
}

/// Whether `exe` sits somewhere under `dir`. Windows paths: case and slash direction do not matter.
fn inside(exe: &Path, dir: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(exe).starts_with(&(norm(dir) + "\\"))
}

fn run_program(run: &Run, target: &Path) -> Result<(), String> {
    let (program, args) = expand_run(run, target)?;
    let status = hidden(&program).args(&args).current_dir(target).status().map_err(|e| format!("could not run {program}: {e}"))?;
    if run.fail_on_nonzero && !status.success() {
        return Err(format!("{program} failed (exit code {}).", status.code().unwrap_or(-1)));
    }
    Ok(())
}

/// `run`'s program and arguments with `{target}` filled in.
fn expand_run(run: &Run, target: &Path) -> Result<(String, Vec<String>), String> {
    let target = target.to_string_lossy();
    let vars = [("target", target.as_ref())];
    let args = run.args.iter().map(|a| expand(a, &vars)).collect::<Result<_, _>>()?;
    Ok((expand(&run.program, &vars)?, args))
}

/// Spider's rule: the payload plus the uninstaller (the stub part of the setup exe) plus 64 MiB headroom.
fn space_needed(payload: u64, uninstaller: u64) -> u64 {
    payload + uninstaller + (64 << 20)
}

fn free_space(dir: &Path) -> io::Result<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut free = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(registration::wide(dir).as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(free) }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

/// Move `staging` to `target`, setting any previous install aside at `<target>.old`; returns that backup.
/// A failed swap puts the previous install straight back.
fn swap_in(target: &Path, staging: &Path) -> io::Result<Option<PathBuf>> {
    if !target.exists() {
        return fs::rename(staging, target).map(|_| None);
    }
    let backup = with_suffix(target, ".old");
    if backup.exists() {
        fs::remove_dir_all(&backup)?;
    }
    fs::rename(target, &backup)?;
    match fs::rename(staging, target) {
        Ok(()) => Ok(Some(backup)),
        Err(e) => {
            let _ = fs::rename(&backup, target);
            Err(e)
        }
    }
}

/// Undo a successful `swap_in`: drop the new files and put the previous install back.
fn roll_back(target: &Path, backup: Option<&Path>) {
    let _ = fs::remove_dir_all(target);
    if let Some(b) = backup {
        let _ = fs::rename(b, target);
    }
}

// ponytail: the upgrade swaps the whole folder, so files the app wrote inside it (and older log lines) do not carry over, as in Spider.
fn log_line(target: &Path, line: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = fs::create_dir_all(target)
        .and_then(|_| fs::OpenOptions::new().create(true).append(true).open(target.join("install.log")))
        .and_then(|mut f| writeln!(f, "[{secs}] {line}"));
}

/// Numeric, dot by dot ("1.4.10" > "1.4.9"); missing parts count as 0 ("1.4" == "1.4.0").
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parse = |v: &str| v.split('.').map(|p| p.parse().unwrap_or(0)).collect::<Vec<u64>>();
    let (a, b) = (parse(a), parse(b));
    (0..a.len().max(b.len()))
        .map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)))
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// True when `installed` is the incoming version or newer; nothing installed is never current.
pub fn already_current(installed: Option<&str>, incoming: &str) -> bool {
    installed.is_some_and(|v| compare_versions(incoming, v) != Ordering::Greater)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_and_pad_with_zero() {
        assert_eq!(compare_versions("1.4.10", "1.4.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.4.9", "1.4.10"), Ordering::Less);
        assert_eq!(compare_versions("1.4", "1.4.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.4.1", "1.4"), Ordering::Greater);
    }

    #[test]
    fn already_current_only_at_the_same_or_a_newer_installed_version() {
        assert!(!already_current(None, "1.0.0"));
        assert!(!already_current(Some("1.0.0"), "1.1.0"));
        assert!(already_current(Some("1.1.0"), "1.1.0"));
        assert!(already_current(Some("1.1.0"), "1.0.0"));
    }

    #[test]
    fn space_needed_adds_the_uninstaller_and_headroom() {
        assert_eq!(space_needed(1000, 200), 1200 + 64 * 1024 * 1024);
    }

    #[test]
    fn run_expands_target_in_program_and_args() {
        let run = Run {
            program: r"{target}\bin\helper.exe".into(),
            args: vec!["--dir={target}".into(), "plain".into()],
            at: RunAt::AfterInstall,
            fail_on_nonzero: true,
        };
        let (program, args) = expand_run(&run, Path::new(r"C:\Apps\Silk Sample")).unwrap();
        assert_eq!(program, r"C:\Apps\Silk Sample\bin\helper.exe");
        assert_eq!(args, [r"--dir=C:\Apps\Silk Sample", "plain"]);
        let bad = Run { program: "{product}".into(), ..run };
        assert!(expand_run(&bad, Path::new(r"C:\x")).is_err());
    }

    #[test]
    fn inside_matches_whole_folder_names_only() {
        let dir = Path::new(r"C:\Users\B\AppData\Local\Programs\Silk Sample");
        assert!(inside(Path::new(r"C:\Users\B\AppData\Local\Programs\Silk Sample\sample.exe"), dir));
        assert!(inside(Path::new(r"c:\users\b\appdata\local\programs\SILK SAMPLE\bin\helper.exe"), dir));
        assert!(inside(Path::new(r"C:\Users\B\AppData\Local\Programs\Silk Sample\a.exe"), Path::new(r"C:/Users/B/AppData/Local/Programs/Silk Sample/")));
        assert!(!inside(Path::new(r"C:\Users\B\AppData\Local\Programs\Silk Sample2\sample.exe"), dir));
        assert!(!inside(Path::new(r"C:\Other\sample.exe"), dir));
        assert!(!inside(dir, dir));
    }

    #[test]
    fn close_running_stops_only_processes_from_the_folder() {
        let root = std::env::temp_dir().join(format!("silk-stub-close-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // A same-named copy in a sibling whose name starts the same must survive.
        let (app, other) = (root.join("App"), root.join("App2"));
        let start = |dir: &Path| {
            fs::create_dir_all(dir).unwrap();
            fs::copy(r"C:\Windows\System32\PING.EXE", dir.join("waiter.exe")).unwrap();
            hidden(dir.join("waiter.exe")).args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap()
        };
        let (mut a, mut b) = (start(&app), start(&other));
        assert_eq!(close_running(&app), Ok(true));
        assert!(a.try_wait().unwrap().is_some());
        assert!(b.try_wait().unwrap().is_none());
        assert_eq!(close_running(&app), Ok(false));
        b.kill().unwrap();
        b.wait().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn scratch(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("silk-stub-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("staging")).unwrap();
        fs::write(root.join("staging/new.txt"), "new").unwrap();
        (root.join("Silk Sample"), root.join("staging"), root)
    }

    #[test]
    fn swap_into_an_empty_slot() {
        let (target, staging, root) = scratch("fresh");
        assert_eq!(swap_in(&target, &staging).unwrap(), None);
        assert!(target.join("new.txt").exists() && !staging.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn swap_over_an_install_keeps_a_backup_until_rolled_back() {
        let (target, staging, root) = scratch("upgrade");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.txt"), "old").unwrap();
        let backup = swap_in(&target, &staging).unwrap().unwrap();
        assert_eq!(backup, root.join("Silk Sample.old"));
        assert!(target.join("new.txt").exists() && !target.join("old.txt").exists());

        roll_back(&target, Some(&backup));
        assert_eq!(fs::read_to_string(target.join("old.txt")).unwrap(), "old");
        assert!(!target.join("new.txt").exists() && !backup.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn roll_back_of_a_fresh_install_leaves_nothing() {
        let (target, staging, root) = scratch("fresh-rollback");
        swap_in(&target, &staging).unwrap();
        roll_back(&target, None);
        assert!(!target.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_swap_restores_the_previous_install() {
        let (target, _, root) = scratch("failed-swap");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.txt"), "old").unwrap();
        assert!(swap_in(&target, &root.join("never-finished")).is_err());
        assert_eq!(fs::read_to_string(target.join("old.txt")).unwrap(), "old");
        fs::remove_dir_all(root).unwrap();
    }
}
