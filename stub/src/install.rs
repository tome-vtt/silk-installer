// The install, upgrade and uninstall actions (docs/spec.md §§ 5, 6), generalised from Spider's install.rs.
// Each action takes the target and its settings as arguments, so a theme can later override the config's defaults.

use crate::registration;
use serde::Serialize;
use silk_archive::Archive;
use silk_archive::config::{Config, Run, RunAt, Scope, expand};
use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The error a cancelled install reports; the page tells it from real failures.
pub const CANCELLED: &str = "cancelled";

/// Where an install or uninstall is, reported to the theme with every progress event.
#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Install: checking free space, then closing the app and running the before-install programs when there
    /// are any, before any file is staged. Uninstall: running the before-uninstall programs and closing the app.
    Preparing,
    /// Writing the payload into the staging folder; `cancel` stops it, as it does a running `[[run]]` program.
    Copying,
    /// Uninstall only: deleting the installed files, one event per file.
    Removing,
    /// Install: closing the running app, swapping the files in, running programs, shortcuts, registering.
    /// Uninstall: removing the shortcuts, registry values and the Installed Apps entry.
    Finishing,
}

/// One progress report: the last file written, and files and bytes done against their totals.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Progress<'a> {
    pub phase: Phase,
    pub file: &'a str,
    pub done: usize,
    pub total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// The window's side of a `talk` program (docs/theme-api.md): its stdout lines and exit code go to the
/// theme, and the theme's answers to its stdin, which `stdin` holds while it runs.
pub struct Talk<'a> {
    pub stdin: &'a Mutex<Option<ChildStdin>>,
    pub line: &'a (dyn Fn(&str) + Sync),
    pub exit: &'a dyn Fn(i32),
}

/// Write `line` to the stdin of the `talk` program running now.
pub fn answer(stdin: &Mutex<Option<ChildStdin>>, line: &str) -> Result<(), String> {
    if line.contains(['\r', '\n']) {
        return Err("silk.write takes one line, without line breaks.".into());
    }
    let mut stdin = stdin.lock().map_err(|e| e.to_string())?;
    let stdin = stdin.as_mut().ok_or("No program is running that reads from Setup.")?;
    write!(stdin, "{line}\r\n").and_then(|()| stdin.flush()).map_err(|e| format!("The program stopped reading: {e}"))
}

/// How long the running app gets to exit before the install gives up, files untouched.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// A helper process without a console window (the stub itself has none to share).
pub fn hidden(program: impl AsRef<OsStr>) -> Command {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut c = Command::new(program);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// A Windows tool, `tool` its path in the system folder, run as `hidden` runs a helper. Never a bare name:
/// `Command` looks for one in the installer's own folder (usually Downloads) first, where anything dropped
/// would run in the tool's place, elevated for a per-machine installer.
pub fn system(tool: &str) -> io::Result<Command> {
    Ok(hidden(system_dir()?.join(tool)))
}

/// The Windows system folder (System32), from GetSystemDirectoryW.
pub fn system_dir() -> io::Result<PathBuf> {
    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; MAX_PATH as usize];
    // SAFETY: `buf` is a live u16 buffer of exactly MAX_PATH u16s, the size passed.
    let len = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), MAX_PATH) };
    // 0 is a failure, and MAX_PATH or more the size the folder would have needed, with nothing written.
    if len == 0 || len >= MAX_PATH {
        return Err(io::Error::other("Setup could not find the Windows system folder."));
    }
    Ok(OsString::from_wide(&buf[..len as usize]).into())
}

/// Run the before-install programs (closing the running app first, when `close_running` is set and there are
/// any), then copy the payload into `target` through a staging folder, close the running app (when
/// `close_running` is set), swap the copy in, run the after-install programs, add the shortcuts and firewall
/// rules and register. A failing before-install program leaves Silk's staging and the installed files untouched
/// by Silk; the program itself may have changed them. Any later failure puts the previous install's files back.
/// `cancel`, once set, stops the copying or a before- or after-install program running then. Neither can undo
/// what a before-install program that succeeded did.
pub fn install(
    archive: &mut Archive,
    config: &Config,
    target: &Path,
    mut progress: impl FnMut(&Progress),
    cancel: &AtomicBool,
    talk: Option<&Talk>,
) -> Result<(), String> {
    let product = &config.product;
    let (total, bytes_total) = (archive.payload_len(), archive.payload_size());
    let mut report =
        |phase, file: &str, done, bytes_done| progress(&Progress { phase, file, done, total, bytes_done, bytes_total });
    report(Phase::Preparing, "", 0, 0);
    let fail = |e: io::Error| {
        format!(
            "{product} could not finish installing ({e}). Close any programs that might be using these files, then try again."
        )
    };
    let parent = target.parent().ok_or_else(|| format!("cannot install into {}", target.display()))?;
    fs::create_dir_all(parent).map_err(fail)?;
    let size = archive.payload_size() + archive.uninstaller_size();
    let needed = space_needed(archive.payload_size(), archive.uninstaller_size());
    if free_space(parent).map_err(fail)? < needed {
        let drive =
            target.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
        return Err(format!(
            "Drive {drive} is too full. {product} needs about {} MB free. Free up some space, then try again.",
            needed >> 20
        ));
    }

    let mut was_running = before_install(archive, config, target, parent, talk, cancel)?;
    let result = (|| {
        // Staged next to the target (same volume, so the swap is a rename); nothing partial is ever at `target`.
        let staging = parent.join(format!(".silk-installing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        // The uninstaller `silk build` made (and signed) goes in with the payload, so it swaps and rolls back with it.
        let extracted = archive
            .extract_payload(&staging, |done, bytes, file| {
                report(Phase::Copying, file, done, bytes);
                !cancel.load(SeqCst)
            })
            .and_then(|_| archive.extract_uninstaller(&registration::uninstaller_path(&staging, product)));
        if let Err(e) = extracted {
            let _ = fs::remove_dir_all(&staging);
            return Err(if e.kind() == io::ErrorKind::Interrupted { CANCELLED.into() } else { fail(e) });
        }

        report(Phase::Finishing, "", total, bytes_total);
        // Again after a before-install close: the app may have been started while the files were copied.
        if config.close_running {
            was_running |= close_running(target).inspect_err(|_| {
                let _ = fs::remove_dir_all(&staging);
            })?;
        }
        match swap_in(target, &staging) {
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                Err(fail(e))
            }
            Ok(backup) => finish(config, target, size, talk, cancel)
                .map(|()| {
                    let _ = backup.as_deref().map(fs::remove_dir_all);
                })
                .inspect_err(|_| roll_back_install(config, target, backup.as_deref(), undo_registrations)),
        }
    })();
    if result.is_err() && was_running {
        restart(config, target);
    }
    result
}

/// As Spider does: an app we stopped comes back after a failed install, whichever version ended up in place.
// ponytail: per-user only; an elevated per-machine install would start it elevated. Hand it to the
// unelevated parent (elevation::LAUNCH) if a failed per-machine upgrade should restart it too.
fn restart(config: &Config, target: &Path) {
    if config.scope == Scope::User {
        let _ = launch(target, &config.main_exe);
    }
}

/// Close the running app (when close_running) and run the before-install programs, from `parent`, each
/// with the payload files it names extracted beside the target first; nothing is staged yet. Ok(true) when
/// the app was running; a failure starts it again.
fn before_install(
    archive: &mut Archive,
    config: &Config,
    target: &Path,
    parent: &Path,
    talk: Option<&Talk>,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let runs: Vec<_> = config.run.iter().filter(|r| r.at == RunAt::BeforeInstall).collect();
    if runs.is_empty() {
        return Ok(false);
    }
    let closed = config.close_running && close_running(target)?;
    let dir = parent.join(format!(".silk-before-install-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let (target_s, dir_s) = (target.to_string_lossy(), dir.to_string_lossy());
    let vars = [("target", target_s.as_ref()), ("payload", dir_s.as_ref())];
    let result = runs.into_iter().try_for_each(|run| {
        for path in run.payload_files() {
            archive
                .extract_payload_file(path, &dir.join(path))
                .map_err(|e| format!("could not extract {path}: {e}"))?;
        }
        run_program(run, &vars, parent, talk, cancel)
    });
    let _ = fs::remove_dir_all(&dir);
    if result.is_err() && closed {
        restart(config, target);
    }
    result.map(|()| closed)
}

// ponytail: a failure inside register() can leave the entry half-updated. A failed firewall add leaves the
// previous version's rules untouched (they are replaced only once every new rule is added), but a later
// failure on an upgrade leaves the new rules, not the old ones.
fn finish(config: &Config, target: &Path, size: u64, talk: Option<&Talk>, cancel: &AtomicBool) -> Result<(), String> {
    for run in config.run.iter().filter(|r| r.at == RunAt::AfterInstall) {
        run_program(run, &[("target", target.to_string_lossy().as_ref())], target, talk, cancel)?;
    }
    let s = &config.shortcuts;
    registration::create_shortcuts(
        &config.scope,
        &config.product,
        &target.join(&config.main_exe),
        target,
        s.start_menu,
        s.desktop,
        s.app_user_model_id.as_deref(),
    )
    .map_err(|e| format!("could not create the shortcuts: {e}"))?;
    registration::add_firewall_rules(config, target)?;
    registration::register(config, target, size).map_err(|e| format!("could not register {}: {e}", config.product))
}

/// The update path (§ 6): no window, a no-op when the same or a newer version is installed, one line in
/// `install.log`, and `launch` called afterwards (when launch_at_finish) as Spider's updater expects. True
/// on success.
pub fn silent(
    archive: &mut Archive,
    config: &Config,
    target: &Path,
    installed: Option<&str>,
    launch: impl FnOnce(),
) -> bool {
    let version = &config.version;
    if let Some(v) = installed.filter(|v| already_current(Some(v), version)) {
        log_line(target, &format!("silent: {v} installed, {version} is not newer, nothing to do"));
        return true;
    }
    match install(archive, config, target, |_| {}, &AtomicBool::new(false), None) {
        Ok(()) => {
            log_line(target, &format!("silent: installed {version}"));
            if config.launch_at_finish {
                launch();
            }
            true
        }
        Err(e) => {
            log_line(target, &format!("silent: install failed: {e}"));
            false
        }
    }
}

/// Undo the install at `target`: run by the uninstaller copy with `--uninstall`. Leaves only this exe and
/// the folder, which `registration::delete_later` removes once the process ends. Not cancellable.
pub fn uninstall(
    config: &Config,
    target: &Path,
    user_sid: Option<&str>,
    mut progress: impl FnMut(&Progress),
    talk: Option<&Talk>,
) -> Result<(), String> {
    // Refuse to empty a folder that is not ours (a stale or edited InstallLocation).
    if !registration::uninstaller_path(target, &config.product).is_file() {
        return Err(format!("{} does not hold an install of {}.", target.display(), config.product));
    }
    let me = target.join(std::env::current_exe().map_err(|e| e.to_string())?.file_name().unwrap_or_default());
    let mut files = Vec::new();
    files_under(target, &me, &mut files);
    let (total, bytes_total) = (files.len(), files.iter().map(|f| f.1).sum());
    let mut report =
        |phase, file: &str, done, bytes_done| progress(&Progress { phase, file, done, total, bytes_done, bytes_total });

    report(Phase::Preparing, "", 0, 0);
    for run in config.run.iter().filter(|r| r.at == RunAt::BeforeUninstall) {
        run_program(run, &[("target", target.to_string_lossy().as_ref())], target, talk, &AtomicBool::new(false))?;
    }
    // Always, whatever close_running says: files cannot be removed from under a running app.
    close_running(target)?;

    // ponytail: empties the whole folder; with a user-chosen target (§ 12) delete only what was installed.
    // Best effort, as in Spider: a file that will not go must not stop the rest.
    let mut bytes = 0;
    for (i, (path, size)) in files.iter().enumerate() {
        let _ = fs::remove_file(path);
        bytes += size;
        let rel = path.strip_prefix(target).unwrap_or(path).to_string_lossy().replace('\\', "/");
        report(Phase::Removing, &rel, i + 1, bytes);
    }
    // The now empty folders, and anything that appeared since the list was made.
    for entry in fs::read_dir(target).into_iter().flatten().flatten() {
        let path = entry.path();
        if path != me {
            let _ = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
        }
    }

    report(Phase::Finishing, "", total, bytes_total);
    registration::remove_shortcuts(&config.scope, &config.product);
    registration::remove_firewall_rules(config);
    remove_registry_values(config, user_sid, registration::delete_value);
    registration::unregister(config);
    Ok(())
}

/// Delete the config's `remove_registry_value` entries through `delete`. An `HKCU` key goes to the signed-in
/// user's hive (`HKU\<user_sid>`) when known: an elevated run's own HKCU may be another account's.
fn remove_registry_values(config: &Config, user_sid: Option<&str>, mut delete: impl FnMut(&str, &str)) {
    for v in &config.remove_registry_value {
        delete(&registration::in_hive(&v.key, user_sid), &v.name);
    }
}

/// Every file under `dir` except `skip`, with its size. Links are listed, not followed.
fn files_under(dir: &Path, skip: &Path, out: &mut Vec<(PathBuf, u64)>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            files_under(&path, skip, out);
        } else if path != skip {
            out.push((path, entry.metadata().map_or(0, |m| m.len())));
        }
    }
}

pub fn launch(target: &Path, main_exe: &Path) -> io::Result<()> {
    Command::new(target.join(main_exe)).current_dir(target).spawn().map(drop)
}

/// Stop every process running from inside `dir` (the app and any helpers it ships), except this exe, and
/// wait for them to exit. Ok(true) when one was running; an error when one is still running at the timeout.
fn close_running(dir: &Path) -> Result<bool, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::*;
    const GONE: i32 = 87; // ERROR_INVALID_PARAMETER: the process exited since we listed it.
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    let (mut closed, mut stuck) = (false, None);
    // By image, not only pid: a per-machine uninstaller's unelevated parent (elevation.rs) runs from
    // `dir` too, waiting for this copy.
    let me = norm(&std::env::current_exe().unwrap_or_default());
    for (pid, exe) in
        processes().into_iter().filter(|(pid, exe)| *pid != std::process::id() && norm(exe) != me && inside(exe, dir))
    {
        // SAFETY: OpenProcess takes only integers; its handle is used below only once checked non-null.
        let h = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
        if h.is_null() {
            if io::Error::last_os_error().raw_os_error() != Some(GONE) {
                stuck = Some(exe);
            }
            continue;
        }
        let left = u32::try_from(deadline.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(u32::MAX);
        // Terminating an already exiting process fails harmlessly; the wait decides.
        // SAFETY: `h` is the open, non-null handle OpenProcess returned, with the terminate and synchronize
        // rights these two calls need.
        let exited = unsafe {
            TerminateProcess(h, 1);
            WaitForSingleObject(h, left) == WAIT_OBJECT_0
        };
        // SAFETY: `h` is open and closed exactly once, after its last use.
        unsafe { CloseHandle(h) };
        if exited { closed = true } else { stuck = Some(exe) }
    }
    match stuck {
        None => Ok(closed),
        Some(exe) => Err(format!(
            "{} is still running and did not close within {} s. Close it, then try again.",
            exe.display(),
            CLOSE_TIMEOUT.as_secs()
        )),
    }
}

/// Every process whose image path this user can read, as (pid, exe).
fn processes() -> Vec<(u32, PathBuf)> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::EnumProcesses;
    use windows_sys::Win32::System::Threading::*;
    let mut pids = vec![0u32; 1024];
    loop {
        // Past u32::MAX bytes, the buffer is offered as u32::MAX, which it still covers.
        let (bytes, mut used) = (u32::try_from(pids.len() * 4).unwrap_or(u32::MAX), 0u32);
        // SAFETY: `pids` is a live u32 buffer of at least `bytes` bytes, and `used` a live u32.
        if unsafe { EnumProcesses(pids.as_mut_ptr(), bytes, &mut used) } == 0 {
            return Vec::new();
        }
        if used < bytes {
            pids.truncate(used as usize / 4);
            break;
        }
        pids.resize(pids.len() * 2, 0);
    }
    const PATH_CHARS: u32 = 32768;
    let mut buf = vec![0u16; PATH_CHARS as usize];
    pids.into_iter()
        // SAFETY: `h` is used only once checked non-null, and closed once, after its last use. `len` starts at
        // `buf`'s length in u16s, so QueryFullProcessImageNameW writes within `buf` and sets `len` to what it wrote.
        .filter_map(|pid| unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut len = PATH_CHARS;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
            CloseHandle(h);
            ok.then(|| (pid, OsString::from_wide(&buf[..len as usize]).into()))
        })
        .collect()
}

/// Whether `exe` sits somewhere under `dir`. Windows paths: case and slash direction do not matter.
fn inside(exe: &Path, dir: &Path) -> bool {
    norm(exe).starts_with(&(norm(dir) + "\\"))
}

fn norm(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// Run `run` from `dir`, with no window: its placeholders filled in from `vars`. A `talk` one talks through
/// `talk` when the window gives one; with none (`--silent`) it runs as any other. `cancel`, once set, ends it.
fn run_program(
    run: &Run,
    vars: &[(&str, &str)],
    dir: &Path,
    talk: Option<&Talk>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let (program, args) = expand_run(run, vars)?;
    let mut command = hidden(resolve_program(&program)?);
    command.args(&args).current_dir(dir);
    let status = match talk.filter(|_| run.talk) {
        Some(talk) => talk_through(command, talk, cancel),
        None => command.spawn().and_then(|mut child| wait_or_cancel(&mut child, cancel)),
    }
    .map_err(|e| {
        if e.kind() == io::ErrorKind::Interrupted { CANCELLED.into() } else { format!("could not run {program}: {e}") }
    })?;
    if run.fail_on_nonzero && !status.success() {
        return Err(format!("{program} failed (exit code {}).", status.code().unwrap_or(-1)));
    }
    Ok(())
}

/// The path a `[[run]]` program starts from. A bare name (no folder) is looked up in the system folder, then
/// in Windows PowerShell's, as `powershell` needs, and never as `Command` would, in the installer's own folder
/// first (usually Downloads), where a dropped file would run elevated. A name with a folder is left as given.
fn resolve_program(program: &str) -> Result<PathBuf, String> {
    if program.contains(['\\', '/', ':']) {
        return Ok(program.into());
    }
    let file = if Path::new(program).extension().is_some() { program.to_string() } else { format!("{program}.exe") };
    let system = system_dir().map_err(|e| e.to_string())?;
    let found =
        [system.join(&file), system.join(r"WindowsPowerShell\v1.0").join(&file)].into_iter().find(|p| p.is_file());
    found.ok_or_else(|| format!("could not run {program}: it is not in the Windows system folder. Give its full path."))
}

/// Wait for `child` to exit, ending it and what it started once `cancel` is set (then the error is
/// `Interrupted`).
fn wait_or_cancel(child: &mut Child, cancel: &AtomicBool) -> io::Result<ExitStatus> {
    let job = job_of(child);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if cancel.load(SeqCst) {
            end(child, job.as_ref());
            child.wait()?;
            return Err(io::ErrorKind::Interrupted.into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A job holding `child` and every process it starts from now on, so a cancel ends them all: a `.cmd`
/// program's own exe would otherwise keep its output open. None when Windows refuses one.
// ponytail: a process started before the assignment escapes the job; start `child` suspended if that matters.
fn job_of(child: &Child) -> Option<OwnedHandle> {
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
    // SAFETY: both arguments may be null: default security, no name.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return None;
    }
    // SAFETY: `job` is the open handle just created, owned from here on and closed once, on drop.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    allow_breakaway(&job);
    // SAFETY: both handles are open: `job` above, and `child`'s for as long as `child` lives.
    (unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } != 0).then_some(job)
}

/// Let a process left running in `job` start one outside it (CREATE_BREAKAWAY_FROM_JOB), as it could before the job
/// existed. A refusal is ignored: the job still ends what it holds on cancel.
fn allow_breakaway(job: &OwnedHandle) {
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    // SAFETY: the struct is plain data, valid all-zero.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let size = u32::try_from(std::mem::size_of_val(&info)).unwrap_or(0);
    // SAFETY: `job` is an open job handle, and `info` is a live struct of `size` bytes.
    unsafe {
        SetInformationJobObject(job.as_raw_handle(), JobObjectExtendedLimitInformation, (&raw const info).cast(), size)
    };
}

/// End `child` and, through `job`, every process it started, returning once they have all exited (or after a few
/// seconds), so none still holds a file the caller is about to delete. Failing harmlessly for one that has just exited.
fn end(child: &mut Child, job: Option<&OwnedHandle>) {
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    let _ = child.kill();
    let Some(job) = job else { return };
    // SAFETY: `job` is an open job handle (job_of).
    unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while active_processes(job) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// How many processes `job` still holds; 0 when Windows will not say.
fn active_processes(job: &OwnedHandle) -> u32 {
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation, QueryInformationJobObject,
    };
    // SAFETY: the struct is plain data, valid all-zero.
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    let size = u32::try_from(std::mem::size_of_val(&info)).unwrap_or(0);
    // SAFETY: `job` is an open job handle, and `info` is a live struct of `size` bytes.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&raw mut info).cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { 0 } else { info.ActiveProcesses }
}

/// Run `command` with its stdin open to `talk.stdin` and each stdout line passed to `talk.line` as it comes,
/// then its exit code to `talk.exit`.
fn talk_through(mut command: Command, talk: &Talk, cancel: &AtomicBool) -> io::Result<ExitStatus> {
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
    *talk.stdin.lock().map_err(|e| io::Error::other(e.to_string()))? = child.stdin.take();
    // Read on a thread of its own, so this one sees a cancel while the program waits for an answer.
    let status = std::thread::scope(|s| {
        s.spawn(|| {
            for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
                (talk.line)(&String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(&line)));
            }
            // Closed, so a program still reading gets the end of its input instead of waiting for ever.
            close_stdin(talk.stdin);
        });
        let status = wait_or_cancel(&mut child, cancel);
        // Closed now too, not only once stdout ends: a process the job missed may keep stdout open while it reads.
        close_stdin(talk.stdin);
        status
    })?;
    (talk.exit)(status.code().unwrap_or(-1));
    Ok(status)
}

/// Close the `talk` program's stdin, so a process still reading it gets the end of its input.
fn close_stdin(stdin: &Mutex<Option<ChildStdin>>) {
    drop(stdin.lock().map(|mut stdin| stdin.take()));
}

/// `run`'s program and arguments with their placeholders filled in.
fn expand_run(run: &Run, vars: &[(&str, &str)]) -> Result<(String, Vec<String>), String> {
    let args = run.args.iter().map(|a| expand(a, vars)).collect::<Result<_, _>>()?;
    Ok((expand(&run.program, vars)?, args))
}

/// Spider's rule: the payload plus the uninstaller plus 64 MiB headroom.
fn space_needed(payload: u64, uninstaller: u64) -> u64 {
    payload + uninstaller + (64 << 20)
}

fn free_space(dir: &Path) -> io::Result<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut free = 0u64;
    // SAFETY: `wide(dir)` is a NUL-terminated UTF-16 path that lives until the call returns, `free` a live u64,
    // and the other two out-parameters are optional, so null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(registration::wide(dir).as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
    };
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

/// Undo a failed install: `roll_back`, and on a fresh install (no `backup`) also `undo` the shortcuts and
/// firewall rules `finish` added. An upgrade keeps them: the shortcuts name the same exe, and the rules are
/// the previous version's or their same-named replacements.
fn roll_back_install(config: &Config, target: &Path, backup: Option<&Path>, undo: impl FnOnce(&Config)) {
    roll_back(target, backup);
    if backup.is_none() {
        undo(config);
    }
}

fn undo_registrations(config: &Config) {
    registration::remove_shortcuts(&config.scope, &config.product);
    registration::remove_firewall_rules(config);
}

/// Undo a successful `swap_in`: drop the new files and put the previous install back.
fn roll_back(target: &Path, backup: Option<&Path>) {
    let _ = fs::remove_dir_all(target);
    if let Some(b) = backup {
        let _ = fs::rename(b, target);
    }
}

// ponytail: the upgrade swaps the whole folder, so files the app wrote inside it (and older log lines) do not carry over, as in Spider.
// A missing target is not created: a failed fresh install rolls it back, and a folder holding only the log is worse than no log.
pub fn log_line(target: &Path, line: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(target.join("install.log"))
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
    fn uninstall_deletes_hkcu_values_from_the_users_hive_and_hklm_ones_as_named() {
        let toml = "product = \"P\"\nversion = \"1.0.0\"\npublisher = \"x\"\nmain_exe = \"s.exe\"\npayload = \"p\"\nscope = \"machine\"\n\
            [[remove_registry_value]]\nkey = 'HKCU\\Software\\P'\nname = \"a\"\n\
            [[remove_registry_value]]\nkey = 'HKLM\\Software\\P'\nname = \"b\"\n";
        let config = silk_archive::config::parse(toml).unwrap();
        let mut seen = Vec::new();
        let mut collect = |k: &str, n: &str| seen.push((k.to_string(), n.to_string()));
        remove_registry_values(&config, Some("S-1-5-21-1"), &mut collect);
        remove_registry_values(&config, None, &mut collect);
        let want = [
            (r"HKU\S-1-5-21-1\Software\P", "a"),
            (r"HKLM\Software\P", "b"),
            (r"HKCU\Software\P", "a"),
            (r"HKLM\Software\P", "b"),
        ];
        assert_eq!(seen, want.map(|(k, n)| (k.to_string(), n.to_string())));
    }

    #[test]
    fn system_tools_run_from_the_windows_system_folder() {
        let system32 = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32");
        let shutdown = system("shutdown.exe").unwrap();
        let program = Path::new(shutdown.get_program());
        assert_eq!(norm(program), norm(&system32.join("shutdown.exe")));
        assert!(program.is_file());
    }

    #[test]
    fn a_bare_run_program_comes_from_the_system_folder_never_the_installers_own() {
        let system32 = system_dir().unwrap();
        assert_eq!(norm(&resolve_program("cmd").unwrap()), norm(&system32.join("cmd.exe")));
        assert_eq!(norm(&resolve_program("CMD.exe").unwrap()), norm(&system32.join("CMD.exe")));
        let ps = resolve_program("powershell").unwrap();
        assert!(ps.is_file() && norm(&ps).starts_with(&norm(&system32)));
        assert!(resolve_program("silk-no-such-tool").is_err());
        assert_eq!(resolve_program(r"C:\Apps\x\helper.exe").unwrap(), Path::new(r"C:\Apps\x\helper.exe"));
    }

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
            talk: false,
        };
        let (program, args) = expand_run(&run, &[("target", r"C:\Apps\Silk Sample")]).unwrap();
        assert_eq!(program, r"C:\Apps\Silk Sample\bin\helper.exe");
        assert_eq!(args, [r"--dir=C:\Apps\Silk Sample", "plain"]);
        let bad = Run { program: "{product}".into(), ..run };
        assert!(expand_run(&bad, &[("target", r"C:\x")]).is_err());
    }

    /// An installer whose before-install program, `tools\check.cmd` from its payload, lists the target's
    /// parent and any running `silk-before-waiter.exe` into `seen.txt` and exits with its second argument; plus
    /// the folder it installs into, holding a previous install.
    fn before_install_setup(name: &str, exit: u8, close: bool, extra: &str) -> (Archive, Config, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("silk-stub-before-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("payload/tools")).unwrap();
        fs::write(root.join("payload/sample.exe"), "new").unwrap();
        let check = "@dir /b /a \"%~dp0..\\..\" > \"%~1\"\r\n@tasklist /fi \"imagename eq silk-before-waiter.exe\" >> \"%~1\"\r\n@exit /b %2\r\n";
        fs::write(root.join("payload/tools/check.cmd"), check).unwrap();
        // A program that talks: asks, reads the answer from stdin, says the rights it runs with, exits 7.
        let talk = "@echo name?\r\n@set /p name=\r\n@echo hello %name%\r\n@%SystemRoot%\\System32\\whoami.exe /groups | %SystemRoot%\\System32\\find.exe \"S-1-16-\"\r\n@exit /b 7\r\n";
        fs::write(root.join("payload/tools/talk.cmd"), talk).unwrap();
        // One that asks, then leaves the answer to a program of its own, which waits on stdin until it closes.
        let wait = "@echo name?\r\n@%SystemRoot%\\System32\\find.exe \"x\"\r\n";
        fs::write(root.join("payload/tools/wait.cmd"), wait).unwrap();
        // One that starts the exe it is given in the background, says so, then waits on stdin.
        let second =
            "@start \"\" /b %1 -n 30 127.0.0.1 > nul\r\n@echo started\r\n@%SystemRoot%\\System32\\find.exe \"x\"\r\n";
        fs::write(root.join("payload/tools/second.cmd"), second).unwrap();
        fs::copy(r"C:\Windows\System32\PING.EXE", root.join("payload/tools/silk-second.exe")).unwrap();
        fs::write(root.join("stub.exe"), "stub").unwrap();
        fs::write(root.join("uninstall.exe"), "uninstaller").unwrap();
        let config = silk_archive::config::parse(&format!(
            "product = \"Silk Sample\"\nversion = \"1.0.0\"\npublisher = \"x\"\nmain_exe = \"sample.exe\"\npayload = \"payload\"\nclose_running = {close}\n\n[[run]]\nprogram = '{{payload}}\\tools\\check.cmd'\nargs = ['{}', \"{exit}\"]\nat = \"before-install\"\n{extra}",
            root.join("seen.txt").display()
        ))
        .unwrap();
        let setup = root.join("setup.exe");
        silk_archive::build(
            &root.join("stub.exe"),
            Some((&root.join("payload"), &root.join("uninstall.exe"), None)),
            None,
            &config,
            &setup,
        )
        .unwrap();
        let target = root.join("apps/Silk Sample");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("sample.exe"), "old").unwrap();
        (Archive::open(&setup).unwrap().unwrap(), config, target, root)
    }

    fn listing(dir: &Path) -> Vec<String> {
        fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn a_failing_before_install_program_changes_nothing() {
        let (mut archive, config, target, root) = before_install_setup("fail", 3, true, "");
        // The installed app, still running: closed first, so an old uninstaller finds no file locked.
        fs::copy(r"C:\Windows\System32\PING.EXE", target.join("silk-before-waiter.exe")).unwrap();
        let mut app = hidden(target.join("silk-before-waiter.exe"))
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let e = install(&mut archive, &config, &target, |_| {}, &AtomicBool::new(false), None).unwrap_err();
        assert!(e.ends_with(r"tools\check.cmd failed (exit code 3)."), "{e}");
        assert!(app.try_wait().unwrap().is_some());
        // It ran with only the previous install and its own extracted folder beside it: nothing staged.
        let seen = fs::read_to_string(root.join("seen.txt")).unwrap();
        assert!(seen.contains("Silk Sample") && seen.contains(".silk-before-install-"), "{seen}");
        assert!(!seen.contains(".silk-installing-") && !seen.contains("silk-before-waiter.exe"), "{seen}");
        assert_eq!(fs::read_to_string(target.join("sample.exe")).unwrap(), "old");
        assert_eq!(listing(&root.join("apps")), ["Silk Sample"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn logging_does_not_recreate_a_missing_target() {
        let (target, _, root) = scratch("log-missing");
        log_line(&target, "silent: install failed");
        assert!(!target.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_silent_install_logs_a_failing_before_install_program() {
        let (mut archive, config, target, root) = before_install_setup("silent", 3, false, "");
        assert!(!silent(&mut archive, &config, &target, None, || {}));
        let log = fs::read_to_string(target.join("install.log")).unwrap();
        assert!(log.contains("silent: install failed") && log.contains("(exit code 3)"), "{log}");
        assert_eq!(fs::read_to_string(target.join("sample.exe")).unwrap(), "old");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn before_install_programs_run_before_the_payload_is_staged() {
        // A failing after-install program rolls the install back before it registers anything on this machine.
        let after = "\n[[run]]\nprogram = \"cmd\"\nargs = [\"/c\", \"exit 5\"]\nat = \"after-install\"\n";
        let (mut archive, config, target, root) = before_install_setup("order", 0, false, after);
        let e = install(&mut archive, &config, &target, |_| {}, &AtomicBool::new(false), None).unwrap_err();
        assert_eq!(e, "cmd failed (exit code 5).");
        let seen = fs::read_to_string(root.join("seen.txt")).unwrap();
        assert!(seen.contains(".silk-before-install-") && !seen.contains(".silk-installing-"), "{seen}");
        assert_eq!(fs::read_to_string(target.join("sample.exe")).unwrap(), "old");
        assert_eq!(listing(&root.join("apps")), ["Silk Sample"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_talk_programs_output_and_exit_code_reach_the_theme_and_its_answers_reach_it() {
        let extra = "\n[[run]]\nprogram = '{payload}\\tools\\talk.cmd'\nat = \"before-install\"\ntalk = true\n";
        let (mut archive, config, target, root) = before_install_setup("talk", 0, false, extra);
        let (stdin, said) = (Mutex::new(None), Mutex::new(Vec::new()));
        // The theme: answers the question as it comes.
        let line = |line: &str| {
            said.lock().unwrap().push(line.to_string());
            if line == "name?" {
                answer(&stdin, "Ann").unwrap();
            }
        };
        let exit = |code| said.lock().unwrap().push(format!("exit {code}"));
        let talk = Talk { stdin: &stdin, line: &line, exit: &exit };
        let e = install(&mut archive, &config, &target, |_| {}, &AtomicBool::new(false), Some(&talk)).unwrap_err();
        assert!(e.ends_with(r"tools\talk.cmd failed (exit code 7)."), "{e}");
        // It ran with this process's own rights: an elevated installer's run step is elevated too.
        let groups =
            String::from_utf8_lossy(&Command::new("whoami").arg("/groups").output().unwrap().stdout).into_owned();
        let level = groups.lines().find(|l| l.contains("S-1-16-")).unwrap();
        assert_eq!(*said.lock().unwrap(), ["name?", "hello Ann", level, "exit 7"]);
        // Nothing reads once it has ended.
        assert!(answer(&stdin, "late").is_err());
        assert_eq!(fs::read_to_string(target.join("sample.exe")).unwrap(), "old");
        fs::remove_dir_all(root).unwrap();
    }

    /// Removes a test's folder when dropped, so a failing assert leaves nothing behind either.
    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_run_programs_job_lets_what_it_leaves_running_break_away() {
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        };
        let mut child = system("cmd.exe").unwrap().args(["/c", "pause"]).stdin(Stdio::piped()).spawn().unwrap();
        let job = job_of(&child).unwrap();
        // SAFETY: the struct is plain data, valid all-zero.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let size = u32::try_from(std::mem::size_of_val(&info)).unwrap();
        // SAFETY: `job` is an open job handle, and `info` is a live struct of `size` bytes.
        let ok = unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast(),
                size,
                std::ptr::null_mut(),
            )
        };
        end(&mut child, Some(&job));
        let _ = child.wait();
        assert_ne!(ok, 0);
        assert_ne!(info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_BREAKAWAY_OK, 0);
    }

    #[test]
    fn cancel_ends_a_talk_program_waiting_for_an_answer() {
        let extra = "\n[[run]]\nprogram = '{payload}\\tools\\wait.cmd'\nat = \"before-install\"\ntalk = true\n";
        let (mut archive, config, target, root) = before_install_setup("cancel", 0, false, extra);
        let _cleanup = Cleanup(root.clone());
        let (stdin, cancel) = (Mutex::new(None), AtomicBool::new(false));
        // The theme: cancels instead of answering.
        let line = |line: &str| {
            if line == "name?" {
                cancel.store(true, SeqCst);
            }
        };
        let talk = Talk { stdin: &stdin, line: &line, exit: &|_| {} };
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let (e, unstuck) = std::thread::scope(|s| {
            // Ends its input if cancel never reaches the program, so the test fails instead of hanging.
            let stdin = &stdin;
            let watchdog = s.spawn(move || {
                let fired = finished.recv_timeout(Duration::from_secs(20)).is_err();
                if fired {
                    drop(stdin.lock().unwrap().take());
                }
                fired
            });
            let e = install(&mut archive, &config, &target, |_| {}, &cancel, Some(&talk)).unwrap_err();
            let _ = done.send(());
            (e, watchdog.join().unwrap())
        });
        assert!(!unstuck, "cancel did not end the program: {e}");
        assert_eq!(e, CANCELLED);
        assert!(answer(&stdin, "late").is_err());
        // Its extracted folder is gone and the previous install untouched.
        assert_eq!(listing(&root.join("apps")), ["Silk Sample"]);
        assert_eq!(fs::read_to_string(target.join("sample.exe")).unwrap(), "old");
    }

    #[test]
    fn cancel_waits_for_a_program_started_from_the_extracted_folder_before_removing_it() {
        // `wait.cmd`-style: starts a bundled exe in the background, says so, then waits on stdin.
        let extra = "\n[[run]]\nprogram = '{payload}\\tools\\second.cmd'\nargs = ['{payload}\\tools\\silk-second.exe']\nat = \"before-install\"\ntalk = true\n";
        let (mut archive, config, target, root) = before_install_setup("cancel-second", 0, false, extra);
        let _cleanup = Cleanup(root.clone());
        let (stdin, cancel) = (Mutex::new(None), AtomicBool::new(false));
        let line = |line: &str| {
            if line == "started" {
                cancel.store(true, SeqCst);
            }
        };
        let talk = Talk { stdin: &stdin, line: &line, exit: &|_| {} };
        let e = install(&mut archive, &config, &target, |_| {}, &cancel, Some(&talk)).unwrap_err();
        assert_eq!(e, CANCELLED);
        // The exe it started held a file there until the job ended, so the folder is gone only if cancel waited.
        assert!(listing(&root.join("apps")).iter().eq(["Silk Sample"]), "{:?}", listing(&root.join("apps")));
        assert!(!listing(&root).iter().any(|n| n.starts_with(".silk-before-install-")));
    }

    #[test]
    fn an_answer_is_one_line() {
        assert!(answer(&Mutex::new(None), "a\nb").unwrap_err().contains("one line"));
    }

    #[test]
    fn inside_matches_whole_folder_names_only() {
        let dir = Path::new(r"C:\Users\B\AppData\Local\Programs\Silk Sample");
        assert!(inside(Path::new(r"C:\Users\B\AppData\Local\Programs\Silk Sample\sample.exe"), dir));
        assert!(inside(Path::new(r"c:\users\b\appdata\local\programs\SILK SAMPLE\bin\helper.exe"), dir));
        assert!(inside(
            Path::new(r"C:\Users\B\AppData\Local\Programs\Silk Sample\a.exe"),
            Path::new(r"C:/Users/B/AppData/Local/Programs/Silk Sample/")
        ));
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
            hidden(dir.join("waiter.exe"))
                .args(["-n", "30", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
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

    #[test]
    fn files_under_lists_nested_files_but_not_the_uninstaller() {
        let root = std::env::temp_dir().join(format!("silk-stub-files-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("top.txt"), "12345").unwrap();
        fs::write(root.join("a/b/deep.bin"), "1").unwrap();
        fs::write(root.join("Uninstall X.exe"), "me").unwrap();
        let mut files = Vec::new();
        files_under(&root, &root.join("Uninstall X.exe"), &mut files);
        files.sort();
        assert_eq!(files, [(root.join("a/b/deep.bin"), 1), (root.join("top.txt"), 5)]);
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
    fn a_failed_fresh_install_removes_its_shortcuts_and_rules_but_an_upgrade_keeps_them() {
        let toml = "product = \"P\"\nversion = \"1.0.0\"\npublisher = \"x\"\nmain_exe = \"s.exe\"\npayload = \"p\"\n";
        let config = silk_archive::config::parse(toml).unwrap();
        let (target, staging, root) = scratch("rollback-registrations");
        let mut undone = 0;
        let backup = swap_in(&target, &staging).unwrap();
        roll_back_install(&config, &target, backup.as_deref(), |_| undone += 1);
        assert_eq!(undone, 1);
        assert!(!target.exists());

        roll_back_install(&config, &target, Some(&root.join("x.old")), |_| undone += 1);
        assert_eq!(undone, 1);
        fs::remove_dir_all(root).unwrap();
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
