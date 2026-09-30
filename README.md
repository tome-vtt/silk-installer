# Silk Setup

Build good-looking Windows installers from a folder, a settings file and an HTML/CSS theme, like
NSIS but themeable.

Early stage: nothing usable yet. [docs/spec.md](docs/spec.md) is the design;
`prototypes/` holds throwaway experiments that answer its riskiest questions.

## Build and run

Needs a Rust toolchain (edition 2024). From the repo root, in PowerShell:

```powershell
cargo test
cargo build --release
# Build an installer from a config; writes <product>-<version>-setup.exe next to it.
# --out picks another file; --stub defaults to silk-stub.exe next to silk.exe; --no-sign skips [sign].
.\target\release\silk.exe build C:\path\to\myapp.toml
# Run it: a window showing the theme, or no window at all with --silent (exit code 0 or 1).
C:\path\to\Sample-1.0.0-setup.exe
C:\path\to\Sample-1.0.0-setup.exe --silent
```

`--silent` does nothing when the same or a newer version is installed, launches the app after an
install when `launch_at_finish` is set, and appends one line to `install.log` in the install folder.

The smallest config, [examples/minimal.toml](examples/minimal.toml):

```toml
product = "Sample"
version = "1.0.0"
publisher = "Sample Co"
main_exe = "sample.exe"   # relative to the payload
payload = "payload"       # relative to this file
```

[examples/spider.toml](examples/spider.toml) shows every field.

The installer's screens are a theme: a folder of HTML, CSS and scripts named by `theme = "..."` in the
config, packed into the exe and talking to the installer through one global, `silk`. Without one, the
built-in default theme is used. [docs/theme-api.md](docs/theme-api.md) describes the API;
[examples/minimal-theme.toml](examples/minimal-theme.toml) uses a one-file theme. `[window]` sets the
size and, with `frame = false`, a frameless window whose theme draws its own title bar.

`cargo build --release` produces both `silk.exe` and the runtime stub `silk-stub.exe` (a Tauri 2 app
in `stub/`; needs the WebView2 runtime). The installer is per-user (no UAC) by default; with
`scope = "machine"` ([examples/machine.toml](examples/machine.toml)) it installs for all users: it asks
for administrator rights (UAC) when started, and uses Program Files, HKLM and the all-users shortcuts;
the app it starts at finish still runs without administrator rights. It copies its files into
the target folder (default `%LOCALAPPDATA%\Programs\<product>` or `%ProgramFiles%\<product>`, or
wherever the installed version lives) through a staging folder, closing the running app first and putting the previous version back
if anything fails, including a `[[run]]` program's nonzero exit. It adds the shortcuts and an
Installed Apps entry whose uninstaller (`Uninstall <product>.exe`, run with `--uninstall`) removes
all of that again, plus each `[[remove_registry_value]]`, in the same theme's window (confirm, progress,
done), or with no window when also given `--silent`; it deletes itself afterwards. A config `icon` becomes the installer's
icon. Run on its own, `silk-stub.exe` carries no files and says so.

Sign the finished installer, never the stub: `silk` refuses a stub that is already signed or packed.
A `[sign]` section in the config does that for you: `silk build` runs its `command` and `args` on the
uninstaller it packs and then on the finished installer (`{file}` is the path), and fails the build,
removing the unsigned file, if it exits nonzero. [examples/spider.toml](examples/spider.toml) shows
Spider's Azure Trusted Signing call.
