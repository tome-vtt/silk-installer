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
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::thread::ScopedJoinHandle;
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
    pub stdin: &'a TalkInput,
    pub line: &'a (dyn Fn(&str) + Sync),
    pub exit: &'a dyn Fn(i32),
}

pub type TalkInput = Mutex<Option<Arc<ProgramInput>>>;

/// Shared with shutdown so it can cancel a write without first taking the answer's mutex.
pub struct ProgramInput {
    pipe: fs::File,
    closed: AtomicBool,
}

/// Write `line` to the stdin of the `talk` program running now.
pub fn answer(stdin: &TalkInput, line: &str) -> Result<(), String> {
    if line.contains(['\r', '\n']) {
        return Err("silk.write takes one line, without line breaks.".into());
    }
    let stdin = stdin.lock().map_err(|e| e.to_string())?;
    let stdin = stdin.as_ref().ok_or("No program is running that reads from Setup.")?;
    write_answer(stdin, format!("{line}\r\n").as_bytes()).map_err(|e| format!("The program stopped reading: {e}"))
}

fn write_answer(input: &ProgramInput, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        if input.closed.load(SeqCst) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        // Do not retry Interrupted: shutdown cancels the pending write, including synchronous callbacks.
        let written = (&input.pipe).write(bytes)?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[written..];
    }
    Ok(())
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
        let new = registration::new_registrations(config).inspect_err(|_| {
            let _ = fs::remove_dir_all(&staging);
        })?;
        match swap_in(target, &staging) {
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                Err(fail(e))
            }
            Ok(backup) => finish(config, target, size, talk, cancel)
                .map(|()| {
                    let _ = backup.as_deref().map(fs::remove_dir_all);
                })
                .inspect_err(|_| {
                    roll_back_install(target, backup.as_deref(), || registration::undo_new_registrations(config, &new));
                }),
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

// ponytail: rollback removes newly added registrations, but does not restore replaced shortcuts, rule definitions
// or Installed Apps values (docs/spec.md § 5). Keep their previous values until register() succeeds if that matters.
// A failure inside register() can leave other uninstall values half-updated, but DisplayVersion is written
// last so a silent retry still installs. Write a temporary key and swap it in if full metadata rollback matters.
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
        None => command.spawn().and_then(|mut child| {
            let job = job_of(&child);
            wait_or_cancel(&mut child, job.as_ref(), cancel)
        }),
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

/// Wait for `child` to exit, then, for a few seconds at most, for what it left running in `job`, which may hold a
/// file the caller is about to delete. Once `cancel` is set, end them all instead (then the error is `Interrupted`).
fn wait_or_cancel(child: &mut Child, job: Option<&OwnedHandle>, cancel: &AtomicBool) -> io::Result<ExitStatus> {
    let cancelled = || cancel.load(SeqCst);
    match exit_or_cancel(child, cancelled)? {
        Some(status) if job.is_none_or(|job| until_empty(job, cancelled)) => Ok(status),
        _ => {
            end(child, job);
            child.wait()?;
            Err(io::ErrorKind::Interrupted.into())
        }
    }
}

/// Wait for `child` to exit; None, at once, when `cancelled` says so first.
fn exit_or_cancel(child: &mut Child, cancelled: impl Fn() -> bool) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if cancelled() {
            return Ok(None);
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
// ponytail: a process started after the first listing that leaves the job before the second is missed, even
// if it still holds files. Track process handles while the program runs if that remaining window matters.
// The second listing is untested: a test needs a lister injected into end to start a process after the first
// listing, and a debugger holding its exit so it stays listed, an undocumented ordering. Add both if it regresses.
fn end(child: &mut Child, job: Option<&OwnedHandle>) {
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    // Opened first: once ended, a process leaves the job before it has closed its files.
    let mut held = job.map(processes_in).unwrap_or_default();
    let _ = child.kill();
    let Some(job) = job else { return };
    // SAFETY: `job` is an open job handle (job_of).
    unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    // Also retain processes started between the first listing and termination that are still in the job.
    held.extend(processes_in(job));
    until_exited(&held, Instant::now() + Duration::from_secs(5), &|| false);
}

/// Wait until `job` holds no process, or a few seconds have passed; false, at once, when `stop` says so first.
/// A process started with CREATE_BREAKAWAY_FROM_JOB is not in the job, so it is not waited for.
// ponytail: one that exits on its own just before a listing has left the job but may still hold its files; open
// handles while the program runs if that window ever matters.
fn until_empty(job: &OwnedHandle, stop: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let held = processes_in(job);
        if held.is_empty() || Instant::now() >= deadline {
            return true;
        }
        if !until_exited(&held, deadline, &stop) {
            return false;
        }
    }
}

/// Wait until every process in `held` has exited, or `deadline` has passed; false, at once, when `stop` says so
/// first. A process handle is signalled only once the process has closed every file and folder it held.
fn until_exited(held: &[OwnedHandle], deadline: Instant, stop: &impl Fn() -> bool) -> bool {
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    for process in held {
        // SAFETY: `process` is an open handle with the synchronize right (processes_in).
        while unsafe { WaitForSingleObject(process.as_raw_handle(), 10) } == WAIT_TIMEOUT && Instant::now() < deadline {
            if stop() {
                return false;
            }
        }
    }
    true
}

/// A handle, to wait on, for each process `job` holds; none when Windows will not say.
// ponytail: the first 64 only; a [[run]] program starting more is waited for in rounds by until_empty, not by end.
fn processes_in(job: &OwnedHandle) -> Vec<OwnedHandle> {
    use windows_sys::Win32::System::JobObjects::{JobObjectBasicProcessIdList, QueryInformationJobObject};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
    // JOBOBJECT_BASIC_PROCESS_ID_LIST, with room for 64 ids.
    #[repr(C)]
    struct List {
        assigned: u32,
        listed: u32,
        ids: [usize; 64],
    }
    let mut list = List { assigned: 0, listed: 0, ids: [0; 64] };
    let size = u32::try_from(std::mem::size_of_val(&list)).unwrap_or(0);
    // SAFETY: `job` is an open job handle, and `list` a live struct of `size` bytes laid out as Windows expects.
    // With more processes than fit, the call fails but still fills `list` with the ids that do.
    unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicProcessIdList,
            (&raw mut list).cast(),
            size,
            std::ptr::null_mut(),
        )
    };
    let listed = list.ids.len().min(usize::try_from(list.listed).unwrap_or(0));
    list.ids[..listed]
        .iter()
        .filter_map(|&id| {
            // SAFETY: OpenProcess takes any id; a null result (the process has gone) is skipped.
            let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, u32::try_from(id).ok()?) };
            // SAFETY: `process` is a handle just opened, owned from here on and closed once, on drop.
            (!process.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(process) })
        })
        .collect()
}

/// Run `command` with its stdin open to `talk.stdin` and each stdout line passed to `talk.line` as it comes,
/// then its exit code to `talk.exit`.
fn talk_through(command: Command, talk: &Talk, cancel: &AtomicBool) -> io::Result<ExitStatus> {
    talk_with_wait(command, talk, cancel, job_of, wait_or_cancel)
}

fn talk_with_wait(
    mut command: Command,
    talk: &Talk,
    cancel: &AtomicBool,
    assign_job: impl FnOnce(&Child) -> Option<OwnedHandle>,
    wait: impl FnOnce(&mut Child, Option<&OwnedHandle>, &AtomicBool) -> io::Result<ExitStatus>,
) -> io::Result<ExitStatus> {
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let job = assign_job(&child);
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
    let pipe = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
    let input = Arc::new(ProgramInput { pipe: OwnedHandle::from(pipe).into(), closed: AtomicBool::new(false) });
    let shutdown_input = Arc::downgrade(&input);
    *talk.stdin.lock().map_err(|e| io::Error::other(e.to_string()))? = Some(input);
    let (lines, received) = sync_channel(0);
    // Only owned pipe data crosses this thread: a holder outside the job may keep its read blocked after cancel.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let stop = AtomicBool::new(false);
    // Theme callbacks stay scoped, so none can use the caller's state after this returns.
    let status = std::thread::scope(|s| -> io::Result<ExitStatus> {
        let (line, stdin, stop, input) = (talk.line, talk.stdin, &stop, &shutdown_input);
        let reader = s.spawn(move || forward_lines(received, line, stdin, input, stop));
        let status = wait(&mut child, job.as_ref(), cancel).inspect_err(|_| {
            // End the job first, then interrupt answers even when an outside holder keeps the pipe open.
            end(&mut child, job.as_ref());
            stop.store(true, SeqCst);
            close_stdin(talk.stdin, input);
        })?;
        // A parent can exit while its descendants still use the pipes; cancellation remains available.
        until_read(&reader, &mut child, job.as_ref(), talk.stdin, input, cancel)
            .inspect_err(|_| stop.store(true, SeqCst))?;
        Ok(status)
    })?;
    (talk.exit)(status.code().unwrap_or(-1));
    Ok(status)
}

/// Forward stdout until EOF, or until shutdown stops waiting on an outside holder. A timeout keeps shutdown
/// independent of that holder; the zero-capacity channel prevents unbounded buffering of a program's output.
fn forward_lines(
    received: Receiver<Vec<u8>>,
    line: &(dyn Fn(&str) + Sync),
    stdin: &TalkInput,
    input: &Weak<ProgramInput>,
    stop: &AtomicBool,
) {
    while !stop.load(SeqCst) {
        match received.recv_timeout(Duration::from_millis(50)) {
            Ok(bytes) => line(&String::from_utf8_lossy(bytes.strip_suffix(b"\r").unwrap_or(&bytes))),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                close_stdin(stdin, input);
                return;
            }
        }
    }
}

/// Wait for `reader` to finish, since what the ended program started may still hold its stdout and read its stdin.
/// A cancel meanwhile ends its job and closes stdin, and is `Interrupted`.
fn until_read(
    reader: &ScopedJoinHandle<'_, ()>,
    child: &mut Child,
    job: Option<&OwnedHandle>,
    stdin: &TalkInput,
    input: &Weak<ProgramInput>,
    cancel: &AtomicBool,
) -> io::Result<()> {
    while !reader.is_finished() {
        if cancel.load(SeqCst) {
            #[cfg(test)]
            tests::signal_post_exit_cancellation();
            end(child, job);
            close_stdin(stdin, input);
            return Err(io::ErrorKind::Interrupted.into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// Close the `talk` program's stdin, so a process still reading it gets the end of its input.
fn close_stdin(stdin: &TalkInput, input: &Weak<ProgramInput>) {
    use std::sync::TryLockError;
    use windows_sys::Win32::System::IO::CancelIoEx;
    // Retain the pipe only while interrupting writers, so removing the registry entry really closes it.
    let Some(input) = input.upgrade() else { return };
    input.closed.store(true, SeqCst);
    loop {
        match stdin.try_lock() {
            Ok(mut stdin) => {
                stdin.take();
                return;
            }
            Err(TryLockError::Poisoned(_)) => return,
            Err(TryLockError::WouldBlock) => {
                // SAFETY: this function retains the pipe's Arc until it returns. Null cancels
                // every pending write on this handle; neither the handle nor any I/O buffers are freed here.
                unsafe { CancelIoEx(input.pipe.as_raw_handle(), std::ptr::null()) };
                // A writer may have passed its closed check just before cancellation; retry until it returns.
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
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

/// Undo a failed install's files and any new registrations, independently of folder existence.
fn roll_back_install(target: &Path, backup: Option<&Path>, undo: impl FnOnce()) {
    roll_back(target, backup);
    undo();
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

    thread_local! {
        static POST_EXIT_CANCELLATION: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const {
            std::cell::RefCell::new(None)
        };
    }

    pub(super) fn signal_post_exit_cancellation() {
        POST_EXIT_CANCELLATION.with(|signal| {
            if let Some(signal) = signal.borrow_mut().take() {
                let _ = signal.send(());
            }
        });
    }

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
        // One that starts the exe it is given in the background for a moment, and fails at once.
        let early = "@start \"\" /b /d \"%~dp0\" %1 -n 2 127.0.0.1 > nul\r\n@exit /b 3\r\n";
        fs::write(root.join("payload/tools/early.cmd"), early).unwrap();
        // One that leaves the asking to a program it starts in the background, and ends at once.
        fs::write(
            root.join("payload/tools/leave.cmd"),
            "@start \"\" /b %SystemRoot%\\System32\\cmd.exe /c %1\r\n@exit /b 3\r\n",
        )
        .unwrap();
        // It asks a second later, so `leave.cmd` has long been seen to end by then.
        let ask = "@ping -n 2 127.0.0.1 > nul\r\n@echo name?\r\n@set /p name=\r\n@echo hello %name%\r\n";
        fs::write(root.join("payload/tools/ask.cmd"), ask).unwrap();
        // Like `ask.cmd`, but never reads its stdin: it holds stdout for half a minute.
        let deaf = "@ping -n 2 127.0.0.1 > nul\r\n@echo name?\r\n@ping -n 30 127.0.0.1 > nul\r\n";
        fs::write(root.join("payload/tools/deaf.cmd"), deaf).unwrap();
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
    fn end_returns_once_what_the_program_started_has_let_go_of_its_folder() {
        let dir = std::env::temp_dir().join(format!("silk-stub-end-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let _cleanup = Cleanup(dir.clone());
        fs::copy(r"C:\Windows\System32\PING.EXE", dir.join("silk-end.exe")).unwrap();
        // cmd runs from outside `dir`; only the exe it starts, running from `dir`, holds it. It starts it only once
        // told to, so the exe is started inside the job. raw_arg: std's quoting (\") is not cmd's.
        let exe = dir.join("silk-end.exe");
        let start = format!(
            "/c set /p go= & start \"\" /b \"{}\" -n 30 127.0.0.1 > nul & echo started & %SystemRoot%\\System32\\find.exe \"x\"",
            exe.display()
        );
        let mut child = system("cmd.exe")
            .unwrap()
            .raw_arg(start)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let job = job_of(&child).unwrap();
        writeln!(child.stdin.as_mut().unwrap(), "go").unwrap();
        let mut said = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut said).unwrap();
        assert_eq!(said.trim(), "started");
        end(&mut child, Some(&job));
        // Before cmd itself is waited for: end alone must have waited for the exe.
        let removed = fs::remove_dir_all(&dir);
        let _ = child.wait();
        removed.unwrap();
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
    fn a_failing_program_is_waited_for_with_what_it_started_from_the_extracted_folder() {
        // Starts a bundled exe in the background, which runs for about a second, and fails at once.
        let extra = "\n[[run]]\nprogram = '{payload}\\tools\\early.cmd'\nargs = ['{payload}\\tools\\silk-second.exe']\nat = \"before-install\"\n";
        let (mut archive, config, target, root) = before_install_setup("early", 0, false, extra);
        let _cleanup = Cleanup(root.clone());
        let e = install(&mut archive, &config, &target, |_| {}, &AtomicBool::new(false), None).unwrap_err();
        assert!(e.ends_with(r"tools\early.cmd failed (exit code 3)."), "{e}");
        // The exe held a file there until it ended, so the folder is gone only if the install waited for it.
        assert_eq!(listing(&root.join("apps")), ["Silk Sample"]);
    }

    #[test]
    fn a_talk_programs_stdin_stays_open_while_what_it_started_still_writes() {
        let extra = "\n[[run]]\nprogram = '{payload}\\tools\\leave.cmd'\nargs = ['{payload}\\tools\\ask.cmd']\nat = \"before-install\"\ntalk = true\n";
        let (mut archive, config, target, root) = before_install_setup("leave", 0, false, extra);
        let _cleanup = Cleanup(root.clone());
        let (stdin, said) = (Mutex::new(None), Mutex::new(Vec::new()));
        // The theme: answers late, once `leave.cmd` has long ended; its stdin closing before then fails the answer.
        let line = |line: &str| {
            said.lock().unwrap().push(line.to_string());
            if line != "name?" {
                return;
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while stdin.lock().unwrap().is_some() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            said.lock().unwrap().push(format!("{:?}", answer(&stdin, "Ann")));
        };
        let talk = Talk { stdin: &stdin, line: &line, exit: &|_| {} };
        let e = install(&mut archive, &config, &target, |_| {}, &AtomicBool::new(false), Some(&talk)).unwrap_err();
        assert!(e.ends_with(r"tools\leave.cmd failed (exit code 3)."), "{e}");
        assert_eq!(*said.lock().unwrap(), ["name?", "Ok(())", "hello Ann"]);
    }

    #[test]
    fn cancel_ends_what_a_talk_program_left_asking_after_it_ended() {
        cancel_closes_input_held_outside_the_job("exited");
    }

    #[test]
    fn cancel_closes_a_running_talk_programs_input_held_outside_the_job() {
        cancel_closes_input_held_outside_the_job("running");
    }

    // Re-exec just this fixture: the parent starts its holder only after the theme answers, ensuring job assignment.
    fn talk_cancel_command(mode: &str) -> Command {
        let mut command = hidden(std::env::current_exe().unwrap());
        command.args(["--exact", "install::tests::talk_cancel_fixture", "--nocapture", "--quiet"]);
        command.env("SILK_TALK_CANCEL_FIXTURE", mode);
        command
    }

    #[test]
    fn post_exit_cancel_through_the_production_talk_path_ends_the_remaining_job() {
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;
        let (_, _, root) = scratch("post-exit-job");
        let _cleanup = Cleanup(root.clone());
        let release = root.join("release");
        let (stdin, cancel) = (Mutex::new(None), AtomicBool::new(false));
        let line = |line: &str| {
            if line == "spawn?" {
                answer(&stdin, "go").unwrap();
            }
        };
        let exited = AtomicBool::new(false);
        let talk = Talk { stdin: &stdin, line: &line, exit: &|_| exited.store(true, SeqCst) };
        let mut command = talk_cancel_command("job-exited");
        command.env("SILK_TALK_RELEASE", &release);
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let (armed, awaiting_cancel) = std::sync::mpsc::channel::<()>();
        let (ready, waiting) = std::sync::mpsc::channel();
        POST_EXIT_CANCELLATION.with(|signal| *signal.borrow_mut() = Some(ready));
        let mut held = Vec::new();
        let (result, ended) = std::thread::scope(|s| {
            let release = &release;
            let watchdog = s.spawn(move || {
                // Startup has its own bounds; only start the fallback clock after cancel is set.
                let _ = awaiting_cancel.recv();
                // Once production has observed post-exit cancellation, keep stdout held until the result
                // and process state are recorded. Only missing delegation needs timeout recovery.
                if waiting.recv_timeout(Duration::from_secs(20)).is_ok() {
                    let _ = finished.recv();
                }
                fs::write(release, []).unwrap();
            });
            let result = talk_with_wait(command, &talk, &cancel, job_of, |child, job, _| {
                let status = child.wait()?;
                assert!(status.success());
                let holder: u32 = fs::read_to_string(release.with_extension("ready")).unwrap().parse().unwrap();
                held.push(open_process(holder));
                let (process, mut member) = (held[0].as_raw_handle(), 0);
                // SAFETY: process and job are live handles, and member is a live BOOL out-parameter.
                assert_ne!(unsafe { IsProcessInJob(process, job.unwrap().as_raw_handle(), &mut member) }, 0);
                assert_ne!(member, 0, "holder must still belong to the exited parent's job");
                // The injected wait has completed successfully: only production's stdout wait can cancel.
                cancel.store(true, SeqCst);
                armed.send(()).unwrap();
                // Own the sender in this FnOnce closure so an early error also unblocks the watchdog.
                drop(armed);
                Ok(status)
            });
            // Observe termination before releasing the holder to recover a broken implementation. The bound only
            // absorbs a slow exit after TerminateJobObject; a holder never ended stays alive until released below.
            let ended = held_have_exited(&held, 30_000);
            let _ = done.send(());
            watchdog.join().unwrap();
            (result, ended)
        });
        until_exited(&held, Instant::now() + Duration::from_secs(5), &|| false);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(ended, "post-exit cancellation returned with a process still running in the job");
        assert!(!exited.load(SeqCst), "cancel emitted a successful exit event");
        assert!(answer(&stdin, "late").is_err());
    }

    #[test]
    fn talk_cancel_fixture() {
        use std::io::Read;
        use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
        let Ok(mode) = std::env::var("SILK_TALK_CANCEL_FIXTURE") else { return };
        if mode == "stdout-eof" {
            use windows_sys::Win32::Foundation::CloseHandle;
            println!("\nname?");
            let mut name = String::new();
            std::io::stdin().read_line(&mut name).unwrap();
            assert_eq!(name.trim(), "Ann");
            // SAFETY: this fixture owns its stdout pipe; it never writes again and exits without libtest teardown.
            assert_ne!(unsafe { CloseHandle(std::io::stdout().as_raw_handle()) }, 0);
            let mut remaining = Vec::new();
            std::io::stdin().read_to_end(&mut remaining).unwrap();
            assert!(remaining.is_empty());
            std::process::exit(0);
        }
        if mode == "job-exited" {
            start_job_holder(&PathBuf::from(std::env::var_os("SILK_TALK_RELEASE").unwrap()), Stdio::inherit);
            std::process::exit(0);
        }
        if mode == "deaf-holder" || mode == "deaf-program" || mode == "job-holder" {
            let release = PathBuf::from(std::env::var_os("SILK_TALK_RELEASE").unwrap());
            if mode == "deaf-program" {
                start_job_holder(&release, Stdio::null);
            }
            println!("\n{}", if mode == "deaf-holder" { "holding stdout" } else { "name?" });
            if mode == "job-holder" {
                let pending = release.with_extension("pending");
                fs::write(&pending, std::process::id().to_string()).unwrap();
                fs::rename(pending, release.with_extension("ready")).unwrap();
            }
            // Longer than the test's 30s termination bound, but finite if the test runner is killed.
            let deadline = Instant::now() + Duration::from_secs(if mode == "job-holder" { 60 } else { 20 });
            while !release.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            fs::write(release.with_extension("seen"), []).unwrap();
            std::process::exit(0);
        }
        if mode == "holder" {
            let eof = PathBuf::from(std::env::var_os("SILK_TALK_EOF").unwrap());
            fs::write(eof.with_extension("ready"), []).unwrap();
            // Cancellation may close stdout before this breakaway process gets scheduled.
            let _ = writeln!(std::io::stdout(), "\nname?");
            std::io::stdin().read_to_end(&mut Vec::new()).unwrap();
            fs::write(eof, []).unwrap();
            std::process::exit(0);
        }
        println!("\nspawn?");
        let mut go = String::new();
        std::io::stdin().read_line(&mut go).unwrap();
        assert_eq!(go.trim(), "go");
        // Detach on Windows: the parent must exit while the holder still waits for stdin to close.
        drop(OwnedHandle::from(
            talk_cancel_command(if mode.starts_with("deaf") { "deaf-holder" } else { "holder" })
                .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .spawn()
                .unwrap(),
        ));
        if !mode.starts_with("deaf") {
            // libtest writes to stdout before entering the holder fixture. Wait until it is past that
            // startup, or cancellation can break its stdout and end the harness before it ever reads stdin.
            let ready = PathBuf::from(std::env::var_os("SILK_TALK_EOF").unwrap()).with_extension("ready");
            let deadline = Instant::now() + Duration::from_secs(20);
            while !ready.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(ready.exists(), "the holder never entered its fixture");
        }
        if mode == "running" || mode == "deaf" {
            if mode == "deaf" {
                let release = PathBuf::from(std::env::var_os("SILK_TALK_RELEASE").unwrap());
                while !release.exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
            } else {
                std::io::stdin().read_to_end(&mut Vec::new()).unwrap();
            }
        }
        std::process::exit(0);
    }

    /// Start a "job-holder" descendant with `stdio` as its stdin and stdout, once the theme's answer shows this
    /// fixture is in its job (when it has one), and wait until it is ready.
    fn start_job_holder(release: &Path, stdio: fn() -> Stdio) {
        println!("\nspawn?");
        let mut go = String::new();
        std::io::stdin().read_line(&mut go).unwrap();
        assert_eq!(go.trim(), "go");
        let holder = talk_cancel_command("job-holder").stdin(stdio()).stdout(stdio()).spawn().unwrap();
        drop(OwnedHandle::from(holder));
        let ready = release.with_extension("ready");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "job holder never became ready");
    }

    /// A handle, to wait on, for the process `id`, which must exist.
    fn open_process(id: u32) -> OwnedHandle {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };
        // SAFETY: OpenProcess accepts any id and returns null on failure, checked before taking ownership.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, id) };
        assert!(!process.is_null(), "process {id} is gone");
        // SAFETY: this is the new, non-null handle, owned from here on and closed once, on drop.
        unsafe { OwnedHandle::from_raw_handle(process) }
    }

    #[test]
    fn stdout_eof_closes_stdin_before_waiting_for_the_talk_program_to_exit() {
        let (stdin, cancel) = (Mutex::new(None), AtomicBool::new(false));
        let (ready, waiting) = std::sync::mpsc::channel();
        let line = |line: &str| {
            if line == "name?" {
                answer(&stdin, "Ann").unwrap();
                ready.send(()).unwrap();
            }
        };
        let talk = Talk { stdin: &stdin, line: &line, exit: &|code| assert_eq!(code, 0) };
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let (result, stuck) = std::thread::scope(|s| {
            let cancel = &cancel;
            let watchdog = s.spawn(move || {
                let ready = waiting.recv_timeout(Duration::from_secs(20)).is_ok();
                let stuck = finished.recv_timeout(Duration::from_secs(3)).is_err();
                cancel.store(true, SeqCst);
                assert!(ready, "talk program never asked for its answer");
                stuck
            });
            let result = talk_through(talk_cancel_command("stdout-eof"), &talk, cancel);
            let _ = done.send(());
            (result, watchdog.join().unwrap())
        });
        assert!(!stuck, "stdout EOF did not close stdin; child needed cancellation: {result:?}");
        assert!(result.unwrap().success());
        assert!(answer(&stdin, "late").is_err());
    }

    #[test]
    fn cancel_does_not_wait_for_stdout_held_outside_the_job_without_reading_stdin() {
        cancel_leaves_deaf_stdout_holder("deaf", false);
    }

    #[test]
    fn cancel_after_parent_exit_does_not_wait_for_stdout_held_outside_the_job() {
        cancel_leaves_deaf_stdout_holder("deaf-exited", false);
    }

    #[test]
    fn a_failed_wait_does_not_wait_for_stdout_held_outside_the_job() {
        cancel_leaves_deaf_stdout_holder("deaf-failed", false);
    }

    #[test]
    fn cancel_interrupts_an_answer_to_a_holder_outside_the_job() {
        cancel_leaves_deaf_stdout_holder("deaf", true);
    }

    #[test]
    fn cancel_after_parent_exit_interrupts_an_answer_to_a_holder_outside_the_job() {
        cancel_leaves_deaf_stdout_holder("deaf-exited", true);
    }

    #[test]
    fn a_failed_wait_interrupts_an_answer_to_a_holder_outside_the_job() {
        cancel_leaves_deaf_stdout_holder("deaf-failed", true);
    }

    fn cancel_leaves_deaf_stdout_holder(mode: &str, blocked_answer: bool) {
        let (_, _, root) = scratch(&format!("{mode}-{blocked_answer}"));
        let _cleanup = Cleanup(root.clone());
        fs::create_dir_all(&root).unwrap();
        let release = root.join("release");
        let (stdin, cancel) = (Mutex::new(None), AtomicBool::new(false));
        let holding = AtomicBool::new(false);
        let (ready, waiting) = std::sync::mpsc::channel();
        let line = |line: &str| match line {
            "spawn?" => answer(&stdin, "go").unwrap(),
            "holding stdout" => {
                holding.store(true, SeqCst);
                if !blocked_answer || mode != "deaf-failed" {
                    ready.send(()).unwrap();
                }
                if blocked_answer {
                    assert!(answer(&stdin, &"x".repeat(1 << 20)).is_err());
                } else if mode != "deaf-failed" {
                    cancel.store(true, SeqCst);
                }
            }
            _ => {}
        };
        let talk = Talk { stdin: &stdin, line: &line, exit: &|_| panic!("cancel must not emit an exit event") };
        let mut command = talk_cancel_command(mode);
        command.env("SILK_TALK_RELEASE", &release);
        let seen = release.with_extension("seen");
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let (error, stuck) = std::thread::scope(|s| {
            let cancel = &cancel;
            let stdin = &stdin;
            let watchdog = s.spawn(move || {
                // Arm only once the outside holder is running, excluding process startup from the bound.
                let ready = waiting.recv_timeout(Duration::from_secs(20)).is_ok();
                if blocked_answer && mode != "deaf-failed" {
                    wait_for_blocked_answer(stdin);
                    cancel.store(true, SeqCst);
                }
                let stuck = finished.recv_timeout(Duration::from_secs(3)).is_err();
                cancel.store(true, SeqCst);
                // Release the holder only after cancellation returned, or to recover a failed test.
                fs::write(&release, []).unwrap();
                assert!(ready, "outside holder never became ready");
                stuck
            });
            let error = if mode == "deaf-failed" {
                talk_with_wait(command, &talk, cancel, job_of, |_, _, _| {
                    fail_wait_after_holder_starts(&holding, stdin, blocked_answer, &ready)
                })
                .unwrap_err()
            } else if mode == "deaf-exited" {
                // Force the stdout-wait path: cancellation must be handled after the parent has exited.
                talk_with_wait(command, &talk, cancel, job_of, |child, _, _| child.wait()).unwrap_err()
            } else {
                talk_through(command, &talk, cancel).unwrap_err()
            };
            let _ = done.send(());
            (error, watchdog.join().unwrap())
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        while !seen.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(seen.exists(), "holder did not stop");
        assert!(!stuck, "shutdown waited for stdout held outside the job");
        let expected = if mode == "deaf-failed" { io::ErrorKind::Other } else { io::ErrorKind::Interrupted };
        assert_eq!(error.kind(), expected);
        assert!(answer(&stdin, "late").is_err());
    }

    fn fail_wait_after_holder_starts(
        holding: &AtomicBool,
        stdin: &TalkInput,
        blocked_answer: bool,
        ready: &std::sync::mpsc::Sender<()>,
    ) -> io::Result<ExitStatus> {
        wait_for_stdout_holder(holding);
        if blocked_answer {
            wait_for_blocked_answer(stdin);
            ready.send(()).unwrap();
        }
        Err(io::Error::other("injected try_wait failure"))
    }

    fn wait_for_blocked_answer(stdin: &TalkInput) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while stdin.try_lock().is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(stdin.try_lock().is_err(), "answer never held stdin");
    }

    fn wait_for_stdout_holder(holding: &AtomicBool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !holding.load(SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(holding.load(SeqCst), "outside holder never started");
    }

    #[test]
    fn a_failed_talk_wait_ends_the_job_before_closing_stdin_held_by_an_answer() {
        failed_talk_wait_ends_program_before_closing_stdin(false);
    }

    #[test]
    fn a_failed_talk_wait_without_a_job_ends_the_program_before_closing_stdin_held_by_an_answer() {
        failed_talk_wait_ends_program_before_closing_stdin(true);
    }

    fn failed_talk_wait_ends_program_before_closing_stdin(without_job: bool) {
        let (_, _, root) = scratch(&format!("talk-wait-error-{without_job}"));
        let _cleanup = Cleanup(root.clone());
        fs::create_dir_all(&root).unwrap();
        let release = root.join("release");
        let (stdin, cancel, asked) = (Mutex::new(None), AtomicBool::new(false), AtomicBool::new(false));
        let big = "x".repeat(1 << 20);
        let line = |line: &str| match line {
            "spawn?" => answer(&stdin, "go").unwrap(),
            "name?" => {
                asked.store(true, SeqCst);
                assert!(answer(&stdin, &big).is_err());
            }
            _ => {}
        };
        let talk = Talk { stdin: &stdin, line: &line, exit: &|_| panic!("failed wait must not emit an exit event") };
        let mut command = talk_cancel_command("deaf-program");
        command.env("SILK_TALK_RELEASE", &release);
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let (ready, waiting) = std::sync::mpsc::channel();
        let holder_ready = release.with_extension("ready");
        // The program, then the descendant it started in its job; without a job only the program is ended.
        let mut held = Vec::new();
        let (error, stuck, ended) = std::thread::scope(|s| {
            let watchdog = s.spawn(move || {
                // Arm only after the answer holds stdin, immediately before the injected wait failure.
                let ready = waiting.recv_timeout(Duration::from_secs(20)).is_ok();
                let stuck = finished.recv_timeout(Duration::from_secs(3)).is_err();
                // A failed test releases the non-reader, breaking the blocked write without hanging the suite.
                fs::write(&release, []).unwrap();
                assert!(ready, "answer never became blocked");
                stuck
            });
            let error = talk_with_wait(
                command,
                &talk,
                &cancel,
                |child| if without_job { None } else { job_of(child) },
                |child, job, _| {
                    assert_eq!(job.is_none(), without_job);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !(asked.load(SeqCst) && stdin.try_lock().is_err()) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert!(asked.load(SeqCst) && stdin.try_lock().is_err(), "answer never held stdin");
                    let holder = fs::read_to_string(&holder_ready).unwrap().parse().unwrap();
                    held.extend([open_process(child.id()), open_process(holder)]);
                    ready.send(()).unwrap();
                    Err(io::Error::other("injected try_wait failure"))
                },
            )
            .unwrap_err();
            // Checked before `done` lets the watchdog release the fixtures, which would then exit by themselves. With a
            // job, end has already waited for every exit; without one, the program's kill may still be under way.
            let ended = if without_job { held_have_exited(&held[..1], 1000) } else { held_have_exited(&held, 0) };
            let _ = done.send(());
            (error, watchdog.join().unwrap(), ended)
        });
        until_exited(&held, Instant::now() + Duration::from_secs(5), &|| false);
        assert!(!stuck, "wait failure blocked on the answer's stdin lock");
        assert!(ended, "a failed wait returned with the program or its in-job descendant still running");
        assert_eq!(error.to_string(), "injected try_wait failure");
        assert!(answer(&stdin, "late").is_err());
    }

    /// Whether every process in `held` has exited, allowing each `grace_ms` for an exit already under way.
    fn held_have_exited(held: &[OwnedHandle], grace_ms: u32) -> bool {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: each handle was opened with the synchronize right (open_process) and is alive for this call.
        held.iter().all(|process| unsafe { WaitForSingleObject(process.as_raw_handle(), grace_ms) } == WAIT_OBJECT_0)
    }

    fn cancel_closes_input_held_outside_the_job(mode: &str) {
        let (_, _, root) = scratch(&format!("cancel-input-{mode}"));
        let _cleanup = Cleanup(root.clone());
        fs::create_dir_all(&root).unwrap();
        let eof = root.join("eof");
        let (stdin, cancel) = (Mutex::new(None), AtomicBool::new(false));
        // In exited mode the test waits for the child itself before cancelling, so it cannot accidentally
        // pass via wait_or_cancel's cancellation path. Running mode follows talk_through end to end.
        let line = |line: &str| {
            if line == "spawn?" {
                answer(&stdin, "go").unwrap();
            }
            if line == "name?" && mode == "running" {
                cancel.store(true, SeqCst);
            }
        };
        let talk = Talk { stdin: &stdin, line: &line, exit: &|_| panic!("cancel must not emit an exit event") };
        let mut command = talk_cancel_command(mode);
        command.env("SILK_TALK_EOF", &eof);
        let holder_ready = AtomicBool::new(false);
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let (e, unstuck) = std::thread::scope(|s| {
            // The holder survives job termination and keeps stdout open until EOF on stdin. Recover from a
            // missing close only to fail the test, never to make it pass or to choose which cancel path runs.
            let stdin = &stdin;
            let watchdog = s.spawn(move || {
                let fired = finished.recv_timeout(Duration::from_secs(20)).is_err();
                if fired {
                    drop(stdin.lock().unwrap().take());
                }
                fired
            });
            let e = if mode == "exited" {
                talk_with_wait(command, &talk, &cancel, job_of, |child, _, _| {
                    let status = child.wait()?;
                    holder_ready.store(eof.with_extension("ready").exists(), SeqCst);
                    cancel.store(true, SeqCst);
                    Ok(status)
                })
                .unwrap_err()
            } else {
                talk_through(command, &talk, &cancel).unwrap_err()
            };
            let _ = done.send(());
            (e, watchdog.join().unwrap())
        });
        assert!(!unstuck, "cancel did not end what the program left: {e}");
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
        assert!(
            mode != "exited" || holder_ready.load(SeqCst),
            "the parent exited before the holder entered its fixture"
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !eof.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(eof.exists(), "the holder did not see stdin close");
        assert!(answer(&stdin, "late").is_err());
    }

    #[test]
    fn cancel_ends_what_a_talk_program_left_while_an_answer_is_stuck_writing_to_it() {
        let extra = "\n[[run]]\nprogram = '{payload}\\tools\\leave.cmd'\nargs = ['{payload}\\tools\\deaf.cmd']\nat = \"before-install\"\ntalk = true\n";
        let (mut archive, config, target, root) = before_install_setup("leave-deaf", 0, false, extra);
        let _cleanup = Cleanup(root.clone());
        let (stdin, cancel, asked) = (Mutex::new(None), AtomicBool::new(false), AtomicBool::new(false));
        // The theme: answers more than the pipe holds to `deaf.cmd`, which never reads,
        // so the write blocks holding stdin.
        let big = "x".repeat(1 << 20);
        let line = |line: &str| {
            if line == "name?" {
                asked.store(true, SeqCst);
                while answer(&stdin, &big).is_ok() {}
            }
        };
        let talk = Talk { stdin: &stdin, line: &line, exit: &|_| {} };
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let (e, stuck) = std::thread::scope(|s| {
            let (stdin, cancel, asked) = (&stdin, &cancel, &asked);
            // The user: cancels once the answer is stuck in its write, holding stdin.
            s.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !(asked.load(SeqCst) && stdin.try_lock().is_err()) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                cancel.store(true, SeqCst);
            });
            // `deaf.cmd` ends by itself after half a minute, so a hang fails the test instead of hanging it.
            let watchdog = s.spawn(move || done_rx.recv_timeout(Duration::from_secs(20)).is_err());
            let e = install(&mut archive, &config, &target, |_| {}, cancel, Some(&talk)).unwrap_err();
            let _ = done_tx.send(());
            (e, watchdog.join().unwrap())
        });
        assert!(!stuck, "cancel waited for the stuck answer: {e}");
        assert_eq!(e, CANCELLED);
        assert_eq!(listing(&root.join("apps")), ["Silk Sample"]);
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
    fn a_failed_install_undoes_new_registrations_with_or_without_a_folder_backup() {
        let (target, staging, root) = scratch("rollback-registrations");
        let mut undone = 0;
        let backup = swap_in(&target, &staging).unwrap();
        roll_back_install(&target, backup.as_deref(), || undone += 1);
        assert_eq!(undone, 1);
        assert!(!target.exists());

        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.txt"), "old").unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("new.txt"), "new").unwrap();
        let backup = swap_in(&target, &staging).unwrap();
        roll_back_install(&target, backup.as_deref(), || undone += 1);
        assert_eq!(undone, 2);
        assert_eq!(fs::read_to_string(target.join("old.txt")).unwrap(), "old");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_failure_removes_registrations_created_during_the_attempt() {
        for previous_folder in [false, true] {
            let extra = "\n[[run]]\nprogram = '{target}\\tools\\talk.cmd'\nat = 'after-install'\ntalk = true\n";
            let (mut archive, mut config, target, root) =
                before_install_setup(&format!("registration-failure-{previous_folder}"), 0, false, extra);
            let _cleanup = Cleanup(root);
            config.product = format!("Silk Failed Attempt {} {previous_folder}", std::process::id());
            assert_eq!(registration::installed(&config), (None, None));
            if !previous_folder {
                fs::remove_dir_all(&target).unwrap();
            }
            let stdin = Mutex::new(None);
            let created = AtomicBool::new(false);
            let line = |line: &str| {
                if line == "name?" {
                    // The after-install program's callback creates an entry, then lets the program fail.
                    // This reaches install's own failure callback, after its registration snapshot and swap.
                    registration::register(&config, &target, 0).unwrap();
                    created.store(registration::installed(&config).0.is_some(), SeqCst);
                    answer(&stdin, "Ann").unwrap();
                }
            };
            let talk = Talk { stdin: &stdin, line: &line, exit: &|code| assert_eq!(code, 7) };
            let error =
                install(&mut archive, &config, &target, |_| {}, &AtomicBool::new(false), Some(&talk)).unwrap_err();
            let left = registration::installed(&config);
            registration::unregister(&config);
            assert!(created.load(SeqCst), "the attempt never created an entry");
            assert!(error.contains('7'), "unexpected install failure: {error}");
            assert_eq!(left, (None, None), "install left its newly created entry");
            assert_eq!(target.exists(), previous_folder);
            if previous_folder {
                assert_eq!(fs::read_to_string(target.join("sample.exe")).unwrap(), "old");
            }
        }
    }

    #[test]
    fn rollback_uses_entry_existence_independently_of_folder_existence() {
        let product = format!("Silk Rollback Test {}", std::process::id());
        let toml = format!(
            "product = \"{product}\"\nversion = \"1.0.0\"\npublisher = \"x\"\nmain_exe = \"s.exe\"\npayload = \"p\"\nscope = \"user\"\n"
        );
        let config = silk_archive::config::parse(&toml).unwrap();
        let (target, staging, root) = scratch("rollback-entry");
        // A leftover folder does not make its newly created entry an upgrade's entry.
        fs::create_dir_all(&target).unwrap();
        let new = registration::new_registrations(&config).unwrap();
        let backup = swap_in(&target, &staging).unwrap();
        registration::register(&config, &target, 0).unwrap();
        roll_back_install(&target, backup.as_deref(), || registration::undo_new_registrations(&config, &new));
        let left = registration::installed(&config);
        registration::unregister(&config);
        assert_eq!(left, (None, None));
        // Conversely, a registered install with a missing folder keeps its existing entry.
        fs::remove_dir_all(&target).unwrap();
        registration::register(&config, &target, 0).unwrap();
        let new = registration::new_registrations(&config).unwrap();
        fs::create_dir_all(&staging).unwrap();
        let backup = swap_in(&target, &staging).unwrap();
        roll_back_install(&target, backup.as_deref(), || registration::undo_new_registrations(&config, &new));
        let left = registration::installed(&config);
        registration::unregister(&config);
        assert!(left.0.is_some());
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
