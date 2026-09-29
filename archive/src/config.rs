// The installer config an author writes (docs/spec.md § 3), as TOML.

use serde::{Deserialize, Serialize};
use std::path::{Component, PathBuf};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub product: String,
    pub version: String,
    pub publisher: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
    #[serde(default)]
    pub scope: Scope,
    /// The default install folder. May hold `{localappdata}` and `{product}`; expanded on the installing machine.
    #[serde(default = "default_target")]
    pub target: String,
    /// Relative to the payload; also names the process to close and the program to launch.
    pub main_exe: PathBuf,
    pub payload: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<PathBuf>,
    #[serde(default = "yes")]
    pub close_running: bool,
    #[serde(default = "yes")]
    pub launch_at_finish: bool,
    #[serde(default)]
    pub window: Window,
    #[serde(default)]
    pub shortcuts: Shortcuts,
    #[serde(default)]
    pub run: Vec<Run>,
    #[serde(default)]
    pub remove_registry_value: Vec<RegistryValue>,
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    User,
    Machine,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Window {
    pub width: u32,
    pub height: u32,
}

impl Default for Window {
    fn default() -> Self {
        Window { width: 720, height: 460 }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Shortcuts {
    pub start_menu: bool,
    pub desktop: bool,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Shortcuts { start_menu: true, desktop: false }
    }
}

/// A program run at a declared point; `program` and `args` may hold `{target}`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub at: RunAt,
    #[serde(default = "yes")]
    pub fail_on_nonzero: bool,
}

#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunAt {
    AfterInstall,
    BeforeUninstall,
}

/// A registry value the app writes itself, removed on uninstall.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryValue {
    pub key: String,
    pub name: String,
}

fn default_target() -> String {
    r"{localappdata}\Programs\{product}".into()
}

fn yes() -> bool {
    true
}

/// Parse and validate a config; errors are one line each.
pub fn parse(text: &str) -> Result<Config, String> {
    let config: Config = toml::from_str(text).map_err(|e| match e.span() {
        Some(span) => format!("line {}: {}", text[..span.start].matches('\n').count() + 1, e.message()),
        None => e.message().to_string(),
    })?;
    config.validate()?;
    Ok(config)
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        if self.product.is_empty() || self.product.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            return Err(format!("product must be a name usable as a file name, got {:?}", self.product));
        }
        if !self.version.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
            return Err(format!("version must be dotted numbers like 1.2.3, got {:?}", self.version));
        }
        if self.scope == Scope::Machine {
            return Err("per-machine installs are not supported yet".into());
        }
        let target = self.expand_target(r"C:\Users\x\AppData\Local")?;
        if !target.is_absolute() {
            return Err(format!("target must be an absolute path or start with {{localappdata}}, got {:?}", self.target));
        }
        let mut parts = self.main_exe.components().peekable();
        if parts.peek().is_none() || !parts.all(|c| matches!(c, Component::Normal(_))) {
            return Err(format!("main_exe must be a relative path inside the payload, got {:?}", self.main_exe));
        }
        if let Some(icon) = &self.icon
            && !icon.extension().is_some_and(|e| e.eq_ignore_ascii_case("ico"))
        {
            return Err(format!("icon must be a .ico file, got {icon:?}"));
        }
        if self.window.width == 0 || self.window.height == 0 {
            return Err("window width and height must be above 0".into());
        }
        for run in &self.run {
            let vars = [("target", r"C:\x")];
            for s in std::iter::once(&run.program).chain(&run.args) {
                expand(s, &vars).map_err(|e| format!("run: {e}"))?;
            }
            if run.program.is_empty() {
                return Err("run: program must not be empty".into());
            }
        }
        for v in &self.remove_registry_value {
            if !v.key.to_ascii_uppercase().starts_with(r"HKCU\") {
                return Err(format!("remove_registry_value key must be under HKCU\\ for a per-user install, got {:?}", v.key));
            }
        }
        Ok(())
    }

    fn expand_target(&self, localappdata: &str) -> Result<PathBuf, String> {
        expand(&self.target, &[("localappdata", localappdata), ("product", &self.product)])
            .map(PathBuf::from)
            .map_err(|e| format!("target: {e}"))
    }

    /// The install directory on this machine.
    pub fn target_dir(&self) -> Result<PathBuf, String> {
        let base = std::env::var("LOCALAPPDATA").map_err(|_| "LOCALAPPDATA is not set".to_string())?;
        self.expand_target(&base)
    }
}

/// Replace each `{name}` in `s` with its value from `vars`; an unknown name is an error.
pub fn expand(s: &str, vars: &[(&str, &str)]) -> Result<String, String> {
    let (mut out, mut rest) = (String::new(), s);
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let len = rest[i..].find('}').ok_or_else(|| format!("unclosed {{ in {s:?}"))?;
        let name = &rest[i + 1..i + len];
        let (_, value) = vars.iter().find(|(k, _)| *k == name).ok_or_else(|| format!("unknown placeholder {{{name}}} in {s:?}"))?;
        out.push_str(value);
        rest = &rest[i + len + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = include_str!("../../examples/minimal.toml");
    const SPIDER: &str = include_str!("../../examples/spider.toml");

    fn err(text: &str) -> String {
        parse(text).unwrap_err()
    }

    #[test]
    fn minimal_fills_defaults() {
        let c = parse(MINIMAL).unwrap();
        assert_eq!(c.scope, Scope::User);
        assert_eq!(c.target, r"{localappdata}\Programs\{product}");
        assert_eq!((c.window.width, c.window.height), (720, 460));
        assert!(c.shortcuts.start_menu && !c.shortcuts.desktop);
        assert!(c.close_running && c.launch_at_finish);
        assert!(c.icon.is_none() && c.theme.is_none() && c.run.is_empty() && c.remove_registry_value.is_empty());
        let expected = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap()).join("Programs").join(&c.product);
        assert_eq!(c.target_dir().unwrap(), expected);
    }

    const FULL: &str = r#"
product = "Full"
version = "2.10.0.7"
publisher = "Full Co"
icon = "art/full.ico"
scope = "user"
target = '{localappdata}\Apps\{product}'
main_exe = 'bin\Full.exe'
payload = "../build/out"
theme = "theme"
close_running = false
launch_at_finish = false

[window]
width = 800
height = 500

[shortcuts]
start_menu = false
desktop = true

[[run]]
program = '{target}\bin\helper.exe'
args = ["--register", '{target}']
at = "after-install"

[[run]]
program = "cmd"
args = ["/c", "exit 0"]
at = "before-uninstall"
fail_on_nonzero = false

[[remove_registry_value]]
key = 'HKCU\Software\Microsoft\Windows\CurrentVersion\Run'
name = "full"
"#;

    #[test]
    fn full_config() {
        let c = parse(FULL).unwrap();
        assert_eq!(c.version, "2.10.0.7");
        assert_eq!(c.icon, Some(PathBuf::from("art/full.ico")));
        assert_eq!(c.main_exe, PathBuf::from(r"bin\Full.exe"));
        assert_eq!(c.theme, Some(PathBuf::from("theme")));
        assert!(!c.close_running && !c.launch_at_finish);
        assert_eq!((c.window.width, c.window.height), (800, 500));
        assert!(!c.shortcuts.start_menu && c.shortcuts.desktop);
        assert_eq!(c.run[0].at, RunAt::AfterInstall);
        assert!(c.run[0].fail_on_nonzero && !c.run[1].fail_on_nonzero);
        assert_eq!(c.run[1].at, RunAt::BeforeUninstall);
        assert_eq!(c.remove_registry_value[0].name, "full");
        assert!(c.target_dir().unwrap().ends_with(r"Apps\Full"));
    }

    #[test]
    fn spider_example() {
        let c = parse(SPIDER).unwrap();
        assert_eq!((c.product.as_str(), c.version.as_str(), c.publisher.as_str()), ("Spider", "0.1.0", "Tome Gaming"));
        assert_eq!(c.main_exe, PathBuf::from("Spider.exe"));
        assert_eq!(c.remove_registry_value[0].name, "spider-desktop");
        assert!(c.target_dir().unwrap().ends_with(r"Programs\Spider"));
    }

    #[test]
    fn round_trips_through_toml() {
        let c = parse(FULL).unwrap();
        let again = parse(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(format!("{c:?}"), format!("{again:?}"));
    }

    #[test]
    fn unknown_field_names_it_and_its_line() {
        let e = err(&MINIMAL.replace("publisher", "publsher"));
        assert!(e.starts_with("line 3: unknown field `publsher`"), "{e}");
        assert!(!e.contains('\n'), "{e}");
        let e = err(&format!("{MINIMAL}\n[window]\nwidht = 5\n"));
        assert!(e.contains("unknown field `widht`"), "{e}");
    }

    #[test]
    fn bad_version() {
        for v in ["1.x", "1..2", "", "v1.0", "1.0-beta"] {
            let e = err(&MINIMAL.replace(r#"version = "1.0.0""#, &format!("version = {v:?}")));
            assert!(e.starts_with("version must be dotted numbers"), "{v}: {e}");
        }
    }

    #[test]
    fn machine_scope_is_not_supported_yet() {
        assert_eq!(err(&format!("scope = \"machine\"\n{MINIMAL}")), "per-machine installs are not supported yet");
    }

    #[test]
    fn main_exe_must_stay_inside_the_payload() {
        for exe in [r"..\evil.exe", "../evil.exe", r"C:\Windows\notepad.exe", r"\evil.exe", "a/../../evil.exe", ""] {
            let e = err(&MINIMAL.replace(r#"main_exe = "sample.exe""#, &format!("main_exe = '{exe}'")));
            assert!(e.starts_with("main_exe must be a relative path inside the payload"), "{exe}: {e}");
        }
    }

    #[test]
    fn target_placeholders() {
        let e = err(&format!("target = '{{programfiles}}\\x'\n{MINIMAL}"));
        assert!(e.contains("unknown placeholder {programfiles}"), "{e}");
        let e = err(&format!("target = 'Programs\\x'\n{MINIMAL}"));
        assert!(e.starts_with("target must be an absolute path"), "{e}");
        assert!(parse(&format!("target = 'D:\\Apps\\{{product}}'\n{MINIMAL}")).is_ok());
    }

    #[test]
    fn expand_placeholders() {
        let vars = [("a", "1"), ("b", "two")];
        assert_eq!(expand("{a}-{b}/{a}", &vars).unwrap(), "1-two/1");
        assert_eq!(expand("plain", &vars).unwrap(), "plain");
        assert!(expand("{c}", &vars).is_err());
        assert!(expand("{a", &vars).is_err());
    }
}
