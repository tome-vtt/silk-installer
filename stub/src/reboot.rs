// Restart Windows for a run step and come back after sign-in (docs/spec.md § 2, `silk.reboot` in
// docs/theme-api.md): a RunOnce entry starts the installer again once, and that run starts over.

use crate::install::hidden;
use crate::registration;
use silk_archive::config::Config;
use std::io;
use std::path::Path;

/// Added to the relaunch's arguments: this run follows a restart, so it starts again by itself.
pub const FLAG: &str = "--resumed";

/// Where the relaunch is registered, for either scope: Windows runs it unelevated at the user's next sign-in,
/// and a per-machine installer then asks for administrator rights as it did the first time (elevation.rs).
/// Not HKLM, whose entries run elevated without asking: anything that can replace the installer in the
/// user's Downloads could then run as administrator.
// ponytail: in an elevated run HKCU is the elevating account's, so an installer another account elevated comes
// back at that account's sign-in. Have the unelevated parent register it (as with elevation::LAUNCH) if that matters.
const RUN_ONCE: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce";

fn entry(config: &Config) -> String {
    format!("{} Setup", config.product)
}

/// The relaunch's command line: `exe` with FLAG. An install window has no other arguments but
/// `elevation::FLAG`, which the relaunch adds itself when it elevates.
pub fn command(exe: &Path) -> String {
    format!("\"{}\" {FLAG}", exe.display())
}

/// The theme asked for a restart, which is the user's confirmation: register `command` to run once at the
/// next sign-in, then restart Windows through `restart` (`restart` below, outside tests). A failed restart
/// removes the registration again.
pub fn reboot(config: &Config, command: &str, restart: impl FnOnce() -> io::Result<()>) -> Result<(), String> {
    registration::set_sz(RUN_ONCE, &entry(config), command)
        .map_err(|e| format!("Setup could not arrange to start again after the restart: {e}"))?;
    restart().map_err(|e| {
        forget(config);
        format!("Setup could not restart Windows: {e}")
    })
}

/// Whether `args` hold FLAG. Windows removes a RunOnce entry before running it; this also removes one
/// started some other way, so it never starts the installer a second time.
pub fn resume(config: &Config, args: &[String]) -> bool {
    let resumed = args.iter().any(|a| a == FLAG);
    if resumed {
        forget(config);
    }
    resumed
}

fn forget(config: &Config) {
    registration::delete_value(RUN_ONCE, &entry(config));
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
            registration::get_sz(RUN_ONCE, &entry(&self.0))
        }
    }

    impl Drop for TestInstaller {
        fn drop(&mut self) {
            forget(&self.0);
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
        assert_eq!(reboot(&t.0, &cmd, restart), Ok(()));
        assert_eq!(seen, Some(Some(cmd.clone())), "restarted before the relaunch was registered");
        assert_eq!(t.registered(), Some(cmd));
    }

    #[test]
    fn a_failed_restart_leaves_nothing_registered() {
        let t = TestInstaller::new("fails");
        let e = reboot(&t.0, &command(Path::new(EXE)), || Err(io::Error::other("no rights"))).unwrap_err();
        assert!(e.contains("could not restart Windows: no rights"), "{e}");
        assert_eq!(t.registered(), None);
    }

    #[test]
    fn starting_through_the_registration_resumes_once() {
        let t = TestInstaller::new("resumes");
        reboot(&t.0, &command(Path::new(EXE)), || Ok(())).unwrap();
        let registered = t.registered().unwrap();
        // The arguments Windows would start the installer with: everything after the quoted exe.
        let args: Vec<String> = registered.rsplit_once('"').unwrap().1.split_whitespace().map(String::from).collect();
        assert!(resume(&t.0, &args));
        assert_eq!(t.registered(), None);
        assert!(!resume(&t.0, &["--silent".into()]));
    }
}
