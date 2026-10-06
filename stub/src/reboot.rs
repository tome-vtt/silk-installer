// Restart Windows for a run step and come back after sign-in (docs/spec.md § 2, `silk.reboot` in
// docs/theme-api.md): a RunOnce entry starts the installer again once, and that run starts over.

use crate::install::hidden;
use crate::registration;
use silk_archive::config::{Config, Scope};
use std::io;
use std::path::Path;

/// Added to the relaunch's arguments: this run follows a restart, so it starts again by itself.
pub const FLAG: &str = "--resumed";

/// Where the relaunch is registered. A per-machine run is elevated and writes HKLM, whose entries Windows
/// runs at an administrator's sign-in with the rights this run had; a per-user one writes HKCU, run as the user.
fn run_once(scope: &Scope) -> &'static str {
    match scope {
        Scope::User => r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce",
        Scope::Machine => r"HKLM\Software\Microsoft\Windows\CurrentVersion\RunOnce",
    }
}

fn entry(config: &Config) -> String {
    format!("{} Setup", config.product)
}

/// The relaunch's command line: `exe` with this run's arguments (the stub reads only `--uninstall` from a
/// window run), then FLAG. Not `elevation::FLAG`: no parent waits on the relaunch.
pub fn command(exe: &Path, uninstall: bool) -> String {
    let mode = if uninstall { " --uninstall" } else { "" };
    format!("\"{}\"{mode} {FLAG}", exe.display())
}

/// The theme asked for a restart, which is the user's confirmation: register `command` to run once at the
/// next sign-in, then restart Windows through `restart` (`restart` below, outside tests). A failed restart
/// removes the registration again.
pub fn reboot(config: &Config, command: &str, restart: impl FnOnce() -> io::Result<()>) -> Result<(), String> {
    registration::set_sz(run_once(&config.scope), &entry(config), command)
        .map_err(|e| format!("Setup could not arrange to start again after the restart: {e}"))?;
    restart().map_err(|e| {
        forget(config);
        format!("Setup could not restart Windows: {e}")
    })
}

/// Whether `args` hold FLAG. Windows removes a RunOnce entry before running it; this also removes one
/// started some other way, so it never starts the installer a second time.
pub fn resumed(config: &Config, args: &[String]) -> bool {
    let resumed = args.iter().any(|a| a == FLAG);
    if resumed {
        forget(config);
    }
    resumed
}

fn forget(config: &Config) {
    registration::delete_value(run_once(&config.scope), &entry(config));
}

/// Restart Windows now, as a planned restart for an application install (reason p:4:2).
pub fn restart() -> io::Result<()> {
    let status = hidden("shutdown").args(["/r", "/t", "0", "/d", "p:4:2"]).status()?;
    if !status.success() {
        return Err(io::Error::other(format!("shutdown exit code {}", status.code().unwrap_or(-1))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-user config under a test-only product name, whose RunOnce entry is removed when it drops,
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
            registration::get_sz(run_once(&self.0.scope), &entry(&self.0))
        }
    }

    impl Drop for TestInstaller {
        fn drop(&mut self) {
            forget(&self.0);
        }
    }

    const EXE: &str = r"C:\Users\Bart\Down loads\Sample Setup.exe";

    #[test]
    fn the_relaunch_has_the_same_arguments_and_resumes() {
        assert_eq!(command(Path::new(EXE), false), format!("\"{EXE}\" --resumed"));
        assert_eq!(command(Path::new(EXE), true), format!("\"{EXE}\" --uninstall --resumed"));
    }

    #[test]
    fn a_per_machine_relaunch_is_registered_where_it_runs_elevated() {
        assert_eq!(run_once(&Scope::Machine), r"HKLM\Software\Microsoft\Windows\CurrentVersion\RunOnce");
        assert_eq!(run_once(&Scope::User), r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce");
    }

    #[test]
    fn a_reboot_registers_the_relaunch_then_restarts() {
        let t = TestInstaller::new("registers");
        let cmd = command(Path::new(EXE), false);
        // Until the theme asks, which is the user's confirmation, nothing is registered.
        assert_eq!(t.registered(), None);
        let mut seen = None;
        let restart = || {
            seen = Some(t.registered());
            Ok(())
        };
        assert_eq!(reboot(&t.0, &cmd, restart), Ok(()));
        assert_eq!(seen, Some(Some(cmd.clone())), "restarted before the relaunch was registered");
        assert_eq!(t.registered(), Some(cmd));
    }

    #[test]
    fn a_failed_restart_leaves_nothing_registered() {
        let t = TestInstaller::new("fails");
        let e = reboot(&t.0, &command(Path::new(EXE), false), || Err(io::Error::other("no rights"))).unwrap_err();
        assert!(e.contains("could not restart Windows: no rights"), "{e}");
        assert_eq!(t.registered(), None);
    }

    #[test]
    fn starting_through_the_registration_resumes_once() {
        let t = TestInstaller::new("resumes");
        reboot(&t.0, &command(Path::new(EXE), true), || Ok(())).unwrap();
        let registered = t.registered().unwrap();
        // The arguments Windows would start the installer with: everything after the quoted exe.
        let args: Vec<String> = registered.rsplit_once('"').unwrap().1.split_whitespace().map(String::from).collect();
        assert!(resumed(&t.0, &args));
        assert_eq!(t.registered(), None);
        assert!(!resumed(&t.0, &["--uninstall".into()]));
    }
}
