// Restart Windows for a run step and come back after sign-in (docs/spec.md § 4, `silk.reboot` in
// docs/theme-api.md): a RunOnce entry starts the installer again once, and that run starts over.

use crate::install::system;
use crate::registration;
use silk_archive::config::Config;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Added to the relaunch's arguments: this run follows a restart, so it starts again by itself.
pub const FLAG: &str = "--resumed";

/// Where the relaunch is registered, for either scope: Windows runs it unelevated at the user's next sign-in,
/// and a per-machine installer then asks for administrator rights as it did the first time (elevation.rs).
/// Not HKLM, whose entries run elevated without asking: anything that can replace the installer in the
/// user's Downloads could then run as administrator.
///
/// In an elevated run HKCU is the elevating account's, which is not the user's when another account answered
/// UAC: the elevated copy is told the user's SID (`elevation::SID_FLAG`) and writes under `HKU\<sid>` instead.
fn run_once(user_sid: Option<&str>) -> String {
    let key = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
    registration::in_hive(&format!(r"HKCU\{key}"), user_sid)
}

fn entry(config: &Config) -> String {
    format!("{} Setup", config.product)
}

/// The relaunch's command line: `exe` with FLAG. An install window has no other arguments but
/// `elevation::FLAG`, which the relaunch adds itself when it elevates.
pub fn command(exe: &Path) -> String {
    format!("\"{}\" {FLAG}", exe.display())
}

/// The theme asked for a restart, which is the user's confirmation: register `command` to run once at the
/// next sign-in of `user_sid` (None: this process's user), then restart Windows through `restart` (`restart`
/// below, outside tests). A failed restart removes the registration again through `delete`
/// (`registration::delete_value`, outside tests).
pub fn reboot(
    config: &Config,
    command: &str,
    user_sid: Option<&str>,
    restart: impl FnOnce() -> io::Result<()>,
    delete: impl FnOnce(&str, &str),
) -> Result<(), String> {
    registration::set_sz(&run_once(user_sid), &entry(config), command)
        .map_err(|e| format!("Setup could not arrange to start again after the restart: {e}"))?;
    restart().map_err(|e| {
        forget(config, user_sid, delete);
        format!("Setup could not restart Windows: {e}")
    })
}

/// Whether `args` hold FLAG. Windows removes a RunOnce entry before running it; this also removes one
/// started some other way, so it never starts the installer a second time. `user_sid` is whose hive holds the
/// entry, as `reboot` registered it; `delete` removes it.
pub fn resume(config: &Config, args: &[String], user_sid: Option<&str>, delete: impl FnOnce(&str, &str)) -> bool {
    let resumed = args.iter().any(|a| a == FLAG);
    if resumed {
        forget(config, user_sid, delete);
    }
    resumed
}

/// The run a resumed window owes: each page the theme opens starts it while it is owed, and it stops being
/// owed once a run has started, so a later page never starts it again.
pub struct Pending(AtomicBool);

impl Pending {
    pub fn new(resumed: bool) -> Self {
        Self(AtomicBool::new(resumed))
    }

    /// Whether a page that loads now should start the run.
    pub fn owed(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// A run starts, the theme's own or the resumed one: take `lock` (None while another run holds it), and
    /// only then stop owing the run, so a page that loads while the lock is refused still finds it owed.
    pub fn start<'a, T>(&self, lock: &'a Mutex<T>) -> Option<MutexGuard<'a, T>> {
        let guard = lock.try_lock().ok()?;
        self.0.store(false, Ordering::SeqCst);
        Some(guard)
    }
}

fn forget(config: &Config, user_sid: Option<&str>, delete: impl FnOnce(&str, &str)) {
    delete(&run_once(user_sid), &entry(config));
}

/// Restart Windows now, as a planned restart for an application install (reason p:4:2).
pub fn restart() -> io::Result<()> {
    let status = system("shutdown.exe")?.args(["/r", "/t", "0", "/d", "p:4:2"]).status()?;
    if !status.success() {
        return Err(io::Error::other(format!("shutdown exit code {}", status.code().unwrap_or(-1))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config under a test-only product name, whose RunOnce entry is removed when it drops,
    /// even when the test fails.
    struct TestInstaller(Config);

    impl TestInstaller {
        fn new(test: &str) -> Self {
            let toml = format!(
                "product = \"Silk Reboot Test {test} {}\"\nversion = \"1.0.0\"\npublisher = \"x\"\nmain_exe = \"s.exe\"\npayload = \"p\"\n",
                std::process::id()
            );
            Self(silk_archive::config::parse(&toml).unwrap())
        }

        fn registered(&self) -> Option<String> {
            registration::get_sz(&run_once(None), &entry(&self.0))
        }
    }

    impl Drop for TestInstaller {
        fn drop(&mut self) {
            forget(&self.0, None, registration::delete_value);
        }
    }

    const EXE: &str = r"C:\Users\Bart\Down loads\Sample Setup.exe";

    #[test]
    fn the_relaunch_is_the_installer_resuming() {
        assert_eq!(command(Path::new(EXE)), format!("\"{EXE}\" --resumed"));
    }

    #[test]
    fn a_reboot_registers_the_relaunch_then_restarts() {
        let t = TestInstaller::new("registers");
        let cmd = command(Path::new(EXE));
        // Until the theme asks, which is the user's confirmation, nothing is registered.
        assert_eq!(t.registered(), None);
        let mut seen = None;
        let restart = || {
            seen = Some(t.registered());
            Ok(())
        };
        assert_eq!(reboot(&t.0, &cmd, None, restart, registration::delete_value), Ok(()));
        assert_eq!(seen, Some(Some(cmd.clone())), "restarted before the relaunch was registered");
        assert_eq!(t.registered(), Some(cmd));
    }

    #[test]
    fn the_relaunch_goes_to_the_hive_of_the_user_the_elevated_copy_was_told_of() {
        assert_eq!(run_once(None), r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce");
        assert_eq!(
            run_once(Some("S-1-5-21-1-2-3-1001")),
            r"HKU\S-1-5-21-1-2-3-1001\Software\Microsoft\Windows\CurrentVersion\RunOnce"
        );
        // This test's own user stands in for the one who started the installer: HKU\<sid> is the hive HKCU
        // is, so the value shows up there, and a failed restart removes it from there again.
        let t = TestInstaller::new("sid");
        let sid = crate::elevation::user_sid().unwrap();
        let cmd = command(Path::new(EXE));
        assert_eq!(reboot(&t.0, &cmd, Some(&sid), || Ok(()), registration::delete_value), Ok(()));
        assert_eq!(t.registered(), Some(cmd.clone()));
        assert!(
            reboot(&t.0, &cmd, Some(&sid), || Err(io::Error::other("no rights")), registration::delete_value).is_err()
        );
        assert_eq!(t.registered(), None);
        // Another user's hive that is not loaded takes no new key: the value went to the SID named, not to HKCU.
        let e = reboot(&t.0, &cmd, Some("S-1-5-21-1-2-3-1001"), || panic!("restarted"), |_, _| panic!("forgot"))
            .unwrap_err();
        assert!(e.contains("could not arrange to start again"), "{e}");
        assert_eq!(t.registered(), None);
    }

    #[test]
    fn a_failed_restart_and_a_resume_forget_the_entry_in_the_users_hive_not_hkcu() {
        let sid = "S-1-5-21-1-2-3-1001";
        let t = TestInstaller::new("forgets");
        let want = (format!(r"HKU\{sid}\Software\Microsoft\Windows\CurrentVersion\RunOnce"), entry(&t.0));
        // A failed restart needs the registration to succeed first, which only the test user's own SID can.
        let own = crate::elevation::user_sid().unwrap();
        let (want_own, mut seen) = ((run_once(Some(&own)), entry(&t.0)), None);
        let fail = || Err(io::Error::other("no"));
        let _ = reboot(&t.0, "x", Some(&own), fail, |k, n| seen = Some((k.to_string(), n.to_string())));
        assert_eq!(seen, Some(want_own));
        registration::delete_value(&run_once(None), &entry(&t.0));
        let mut seen = None;
        assert!(resume(&t.0, &[FLAG.to_string()], Some(sid), |k, n| seen = Some((k.to_string(), n.to_string()))));
        assert_eq!(seen, Some(want));
    }

    #[test]
    fn a_failed_restart_leaves_nothing_registered() {
        let t = TestInstaller::new("fails");
        let e = reboot(
            &t.0,
            &command(Path::new(EXE)),
            None,
            || Err(io::Error::other("no rights")),
            registration::delete_value,
        )
        .unwrap_err();
        assert!(e.contains("could not restart Windows: no rights"), "{e}");
        assert_eq!(t.registered(), None);
    }

    #[test]
    fn starting_through_the_registration_resumes_once() {
        let t = TestInstaller::new("resumes");
        reboot(&t.0, &command(Path::new(EXE)), None, || Ok(()), registration::delete_value).unwrap();
        let registered = t.registered().unwrap();
        // The arguments Windows would start the installer with: everything after the quoted exe.
        let args: Vec<String> = registered.rsplit_once('"').unwrap().1.split_whitespace().map(String::from).collect();
        assert!(resume(&t.0, &args, None, registration::delete_value));
        assert_eq!(t.registered(), None);
        assert!(!resume(&t.0, &["--silent".into()], None, registration::delete_value));
    }

    #[test]
    fn a_resumed_window_owes_one_run_until_a_run_starts() {
        let pending = Pending::new(true);
        let archive = Mutex::new(());
        assert!(pending.owed(), "the first page did not start the run");
        // The first page's run was refused the archive lock (another run holds it) and the theme moved to
        // another page: that page still finds the run owed.
        let held = archive.lock().unwrap();
        assert!(pending.start(&archive).is_none());
        assert!(pending.owed(), "the run was cleared before it took the archive lock");
        drop(held);
        assert!(pending.start(&archive).is_some());
        assert!(!pending.owed(), "the next page the theme opened started the run again");
        assert!(!Pending::new(false).owed(), "a run that did not follow a restart started by itself");
    }
}
