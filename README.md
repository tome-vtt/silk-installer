# Silk Setup

Build good-looking Windows installers from a folder, a settings file and an HTML/CSS theme, like
NSIS but themeable.

Usable, and in use: Spider's desktop app ships with a Silk installer, and TomeVTT's desktop app is
moving to one. [docs/spec.md](docs/spec.md) is the design; `prototypes/` holds throwaway experiments
that answered its riskiest questions.

## Download a release

Each `v*` tag has a GitHub release with three assets: `silk.exe`, `silk-stub.exe` and `SHA256SUMS`.
Keep the two exes side by side under those names: `silk build` finds the stub next to `silk.exe`, so it
needs no `--stub` and no Rust toolchain. Neither exe is Authenticode-signed: `silk` refuses a signed
stub, and the config's `[sign]` signs each installer it builds.

The repository is internal, so downloading needs a GitHub token that can read it: Contents: read on
`tome-vtt/silk-setup` (a fine-grained token, or a GitHub App token), in `GH_TOKEN`. A workflow's own
`GITHUB_TOKEN` only reaches its own repository. In PowerShell, to fetch a pinned version into
`tools\silk` and check it:

```powershell
gh release download v0.2.1 -R tome-vtt/silk-setup -D tools\silk
# Compare each file with SHA256SUMS; stops with an error on a mismatch.
Get-Content tools\silk\SHA256SUMS | ForEach-Object {
    $hash, $name = $_ -split '\s+'
    if ((Get-FileHash "tools\silk\$name" -Algorithm SHA256).Hash -ne $hash) { throw "checksum mismatch: $name" }
}
tools\silk\silk.exe build C:\path\to\myapp.toml
```

To publish a version, set it in each crate's `Cargo.toml` and in `stub/tauri.conf.json`, run
`cargo build` so `Cargo.lock` follows, write its release notes in `docs/releases/v<version>.md`
([v0.2.0's](docs/releases/v0.2.0.md), say), merge, and push a matching tag (`git tag v0.3.0; git push
origin v0.3.0`). [.github/workflows/release.yml](.github/workflows/release.yml) builds the release,
uses that notes file from `docs/releases/` as the release's whole description, and refuses a tag that
differs from the crate versions.

## Build and run

Needs a Rust toolchain (edition 2024). From the repo root, in PowerShell:

```powershell
cargo test
cargo build --release
# Build an installer from a config; writes <product>-<version>-setup.exe next to it.
# --out picks another file; --stub defaults to silk-stub.exe next to silk.exe; --no-sign skips [sign];
# --version 1.4.2 replaces the config's version for this build (a release stamping its tag).
.\target\release\silk.exe build C:\path\to\myapp.toml
# Run it: a window showing the theme, or no window at all with --silent (exit code 0 or 1).
C:\path\to\Sample-1.0.0-setup.exe
C:\path\to\Sample-1.0.0-setup.exe --silent
```

`--silent` does nothing when the same or a newer version is installed, launches the app after an
install when `launch_at_finish` is set, and appends one line to `install.log` in the install folder
(a fresh install that fails writes none).

The smallest config, [examples/minimal.toml](examples/minimal.toml):

```toml
product = "Sample"
version = "1.0.0"
publisher = "Sample Co"
main_exe = "sample.exe"   # relative to the payload
payload = "payload"       # relative to this file
```

[examples/spider.toml](examples/spider.toml) shows every field but the per-machine `[[firewall]]`
below.

An app that sets its own Windows app identity (AppUserModelId) groups its pinned taskbar icon and shows
its notifications only when its shortcuts carry the same one. Set `app_user_model_id = "..."` under
`[shortcuts]` and every shortcut the installer writes gets that `System.AppUserModel.ID`; left out,
shortcuts carry none. The uninstaller removes the shortcuts as before.

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

The installer's window needs the Microsoft Edge WebView2 Runtime, which Windows 11 has and Windows
Update has given most Windows 10 PCs. Where it is missing, the installer says so, gives the
address of Microsoft's download page and exits; `--silent` needs no window, so it installs anyway. With
`webview2_bootstrapper = true` (default `false`), `silk build` packs Microsoft's WebView2 bootstrapper
into the installer (about 1.8 MB more), and the installer runs it when the runtime is missing, waits
while it downloads and installs it (per-user, with no UAC prompt, for a per-user install), then shows
its window. The bootstrapper needs the internet; if it fails, the installer shows the same error and
exits with code 1. Turning it on means your installer redistributes Microsoft's file under the WebView2
Runtime licence's terms ([compiler/webview2/README.md](compiler/webview2/README.md)).

A per-machine installer can open the Windows Firewall to the app, for other devices on the network.
Each `[[firewall]]` section is an inbound allow rule, added on install (replacing any inbound rule of
the same name, so an upgrade leaves one copy) and removed on uninstall; `scope = "user"` refuses it,
since it needs administrator rights:

```toml
[[firewall]]
name = "Spider control plane"   # also what Windows Defender Firewall lists it as
ports = [7317, 7318]            # local ports, 1-65535
protocol = "tcp"                # or "udp"
profiles = ["private"]          # any of "domain", "private", "public"
program = '{target}\Spider.exe' # optional; an absolute path
```

Give `ports`, `program` or both, and a different `name` to each rule. Prefer both when the program that listens is one the payload
installs: only that exe may then accept connections, and only on those ports. A rule with `program`
alone opens every port that exe listens on; one with `ports` alone opens them to any program, which
is what you need when the listener is not in the payload (a runtime installed elsewhere, say). Keep to
`private` unless the app must be reached on public networks too. A rule that an upgrade renames or
drops stays on the machine; the installer adds and removes rules with `netsh advfirewall`.

A `[[run]]` section runs a program with no window, failing the install (or uninstall) on a nonzero
exit unless `fail_on_nonzero = false`: `at = "after-install"` and `"before-uninstall"` run it in the
install folder, `{target}`. Once a program exits, the installer waits up to 5 seconds for the processes
it started, so an after-install program that leaves one running (a tray app, say) adds up to 5 seconds
unless it starts that process with `CREATE_BREAKAWAY_FROM_JOB`. `at = "before-install"` runs it before any file is staged, after the
running app is closed, in windowed and `--silent` installs alike; a failure leaves the installed version
as it was and shows the error, or with `--silent` writes it to `install.log` (none for a fresh install
that fails). Its files are not installed yet, so it names them as `{payload}\<path>` (to the end of
the program or argument): the installer extracts just those beside the install folder first, and
`silk build` refuses a path the payload lacks. To remove an older installer's install, say one made
by electron-builder's NSIS, whose Installed Apps entry Silk would otherwise leave beside its own:

```toml
[[run]]
program = "powershell"
args = ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", '{payload}\tools\remove-nsis.ps1', '{target}']
at = "before-install"
```

The script ships in the payload, so it is installed too. It must finish its work before it exits: run
the old uninstaller and wait for it. Silk cannot undo a before-install program that succeeded: a cancel or
a later failure rolls back only Silk's own files, so write the script so that running the installer again
recovers.

`talk = true` lets a `[[run]]` program use the theme as its face: the theme gets each line it prints
and its exit code, and writes lines to its stdin (`silk.write`, [docs/theme-api.md](docs/theme-api.md#talking-to-a-program)).
What the lines say is between the program and the theme. Without a window (`--silent`) it runs as any
other program, its stdout unread and nothing on its stdin.

Sign the finished installer, never the stub: `silk` refuses a stub that is already signed or packed.
A `[sign]` section in the config does that for you: `silk build` runs its `command` and `args` on the
uninstaller it packs and then on the finished installer (`{file}` is the path), and fails the build,
removing the unsigned file, if it exits nonzero. [examples/spider.toml](examples/spider.toml) shows
Spider's Azure Trusted Signing call.
