# Spec: Silk Setup, a themeable Windows installer builder

Decided with Bart on 2026-09-29 in a grilling session, first written as Spider's
`docs/spec/silk-setup.md`. Paths such as `installer/`, `install.rs` and ADR-0021 below refer to the
[Spider](https://github.com/tome-vtt/Spider) repository.

Silk Setup turns the Spider desktop installer (Spider's `installer/`, ADR-0021, `cockpit.md` § 6)
into a general tool, in the spirit of NSIS but with themes built from HTML, CSS and scripts.
Spider's installer becomes its first user.

## 1. Why

NSIS, Inno Setup and WiX installers are hard to make look good. Spider's installer already draws
its screens as a web page in a small Tauri app, so the look is solved. What is missing is the generic
part: today every path, name, registry key and piece of copy is hardcoded for Spider
(`install.rs`, `registration.rs`, `index.html`, `app.js`).

## 2. Scope

- **Audience**: Bart's projects first (Spider, later TomeVTT). Kept clean enough to publish later.
  License and public docs are deferred until then.
- **Platform**: Windows only.
- **Install scope**: both per-user (no UAC, `%LOCALAPPDATA%\Programs`, HKCU, the user's shortcuts) and
  per-machine (UAC, `Program Files`, HKLM, all-users shortcuts). Each installer picks one in its config
  with `scope = "user"` (the default) or `"machine"`.
- **Out of scope**: update checking and feeds (the app keeps that, as Spider's
  `desktop/src/updater.ts` does), translations, macOS and Linux, a scripting language.
- **Not in the first version**: downloading payload files at install time, a planned optional expansion (§ 12).

Per-machine built in step 4 part B (2026-09-29); `examples/machine.toml` is the smallest config:

- **Elevation.** The stub's manifest asks for no rights, since one stub serves both scopes. A
  per-machine run without administrator rights starts itself again through UAC (`ShellExecuteEx`, verb
  `runas`) with the same `--silent` and `--uninstall` and an internal `--elevated`, waits for that copy
  and exits with its exit code. The uninstaller does the same, since Installed Apps starts it without
  rights. A declined prompt exits with 1 and no message; a silent run tries to log it, which only lands
  where the user may write (not in `Program Files`). A run that already has the rights (an IT tool,
  Run as administrator, SYSTEM) goes straight on.
- **Target and registry.** `{programfiles}` is the 64-bit Program Files from the known-folder API.
  The config refuses `{localappdata}` in a per-machine target and `{programfiles}` in a per-user one.
  The Installed Apps entry, and the lookup of the installed version, use HKLM in the 64-bit view (the
  stub is a 64-bit build). `remove_registry_value` may name HKLM keys per-machine; HKCU keys stay
  allowed, but in an elevated run HKCU is the elevating account's hive, which is not the signed-in
  user's when another account approved the prompt.
- **Shortcuts** go to the all-users Start menu and the public desktop.
- **Launch at finish.** An elevated installer must not start the app, or it would run as administrator.
  The elevated copy exits with code `0x5117` instead (a silent install with `launch_at_finish`, or the
  theme's `launch()`), and its unelevated parent starts the app with the user's own rights, then exits
  with 0. This needs no COM, unlike asking the desktop's shell to start it (`IShellDispatch2`), and the
  parent is already there. A run started with the rights has no such parent and does not start the
  app: `silk.info.launchAtFinish` is false and `launch()` rejects. An IT tool or SYSTEM should not
  start a user's app, and another administrator's prompt would start it as that account. A failed
  per-machine upgrade does not restart the app it closed, unlike a per-user one.
- Unchanged and fine elevated: staging next to the target, the free-space check, closing the running
  app (it skips this exe's own image, since the uninstaller's parent waits inside the install folder),
  rollback, `[[run]]` programs (they run elevated) and the self-deleting uninstaller. WebView2 keeps its
  data where it does for a per-user run, in the (elevating) account's
  `%LOCALAPPDATA%\dev.silk-setup.stub`, and it is left behind in both cases.
- Themes read `silk.info.scope`; the default theme says "Installs for all users of this PC".

## 3. How an author builds an installer

Like `makensis`: the author writes a config file and a theme folder, runs one command, and gets one
`.exe`. The author needs no Rust toolchain.

Distribution (2026-09-29): each `v*` tag publishes a GitHub release with `silk.exe`, the unsigned
`silk-stub.exe` and `SHA256SUMS` (`.github/workflows/release.yml`); consumers such as Spider's
workflows download a pinned version with a token that can read this repository (README).

- **Runtime stub**: a prebuilt Tauri exe, the generalised form of today's `spider-installer.exe`.
- **Compiler tool**: reads the config, packs the payload directory, the theme and the resolved config
  into an archive, and appends it to a copy of the stub. The stub reads the archive from its own file
  at start. This replaces today's compile-time `rust-embed` of `desktop/release/win-unpacked/`.
- **Config** is a TOML file (decided 2026-09-29: comments allowed, no indentation traps, native to
  Rust). It declares at least: product name, version, publisher, icon, install scope, target
  directory, main exe, payload directory, theme directory, shortcuts, and the action list (§ 5).
  `examples/spider.toml` shows every field. Relative paths resolve from the config file's folder; the
  target may use `{product}` and, by scope, `{localappdata}` (per-user) or `{programfiles}`
  (per-machine), and defaults to `{localappdata}\Programs\{product}` or `{programfiles}\{product}`.
  Close-the-running-app and launch-at-finish act on the main exe. Unknown fields are errors.
- **Command**: `silk build <config.toml> [--out <file.exe>] [--stub <path>] [--version <x.y.z>]
  [--no-sign]`. `--version` replaces the config's `version` for this build, checked the same way, before
  anything is packed, so the installer, its uninstaller and the Installed Apps entry all carry it: a
  release pipeline stamps its tag without editing the config. `--no-sign` skips `[sign]` (§ 8).

## 4. Themes

A theme is a folder of HTML, CSS and scripts. The runtime serves it in the installer window and
exposes a small JS API, the only contract between theme and runtime:

- read the config fields a theme needs (product name, version, target directory, installed version);
- `start()`, `cancel()`, progress events (current file, percent), finish and error events;
- `launch()`, `quit()`;
- the same calls for uninstall.

Themes own their screens, copy, order and any extra option pages. A default theme ships with the
tool; Spider's current look (`installer/src/`, `prototypes/spider-installer.html`) is the model.
Uninstall uses the theme too (confirm, progress, done), not a windowless run as Spider's does today.
The window size is set in config.

Built in step 3 (2026-09-29); [theme-api.md](theme-api.md) is the author's reference:

- The compiler packs the config's `theme` folder under `theme/` in the archive and refuses one without
  `index.html`; without a theme the stub uses its built-in default (`stub/ui/index.html`). The stub
  serves either from `http://silk.localhost/` through its own protocol handler, so relative paths work;
  the page cannot navigate away from it.
- The runtime injects a global `silk` before the theme's scripts: `silk.info` (product, version,
  publisher, scope, target, installed, alreadyCurrent, launchAtFinish, files, size, frame), `start()` (a
  promise), `cancel()`, `launch()`, `quit()`, `on(event, fn)` for `progress`, `finish` and `error`, and
  `window.minimize()`. Progress events carry the phase (`preparing`, `copying`, `finishing`), the file,
  files and bytes done and total, and a percent by bytes, so themes can show step lists and time left.
  Themes never touch Tauri; the page's capability grants only listening for events, minimising and
  dragging the window.
- `[window]` takes `frame = false` for a frameless window whose theme draws its own title bar, marks
  its drag area with a `data-silk-drag` attribute and calls `silk.window.minimize()` and `silk.quit()`.
  With the native frame, `minimize` and `close` hide or disable those buttons; `frame = false` with
  either set is an error. The window stays non-resizable and without maximise.
- Room left, not built: `start()` can later take options (a chosen folder, components, action
  switches) and `silk.info` more fields (icon, components, free space); new events (a close request)
  go through `on()`.

Uninstall built in step 4 (2026-09-29): the uninstaller carries the theme's `theme/` entries (no
payload; § 8 says how it is made), and `--uninstall` opens the theme with `silk.info.mode = "uninstall"`. `start()`, the
events and `quit()` then mean uninstall; progress runs `preparing`, `removing` (one event per file),
`finishing`. `cancel()` does nothing during an uninstall: stopping halfway would leave a broken app, so
the theme's confirm screen is the way out. The default and minimal themes have confirm, progress and
done screens. `--uninstall --silent` stays windowless.

## 5. Actions

A fixed set of built-in actions, plus one escape hatch:

- copy the payload into place through a staging directory, with rollback on failure (today's
  `replace_target`) and a free-space check;
- close the running app, and launch it at finish;
- create and remove shortcuts (Start menu, optionally desktop; the all-users ones per-machine);
- write and remove the Installed Apps entry (HKCU or HKLM by scope), including the uninstaller (built
  and signed by `silk build`, extracted with the payload so it swaps and rolls back with it) and the
  self-deleting uninstall (after a windowed uninstall, once the window closes; a detached
  `cmd` retries removing the folder for about 10 s);
- remove named registry values the app wrote itself (Spider's `spider-desktop` Run value): under HKCU,
  or per-machine also HKLM;
- **run a program** with arguments, at a declared point, failing the install on a nonzero exit.

## 6. Upgrade

Silk Setup keeps Spider's upgrade behaviour: running over an older version replaces it in place with
rollback, and `--silent` does that without a window, is a no-op when the installed version is the
same or newer, and writes `install.log`. Checking for and fetching new versions stays in the app.

## 7. WebView2

The runtime needs the WebView2 runtime. When it is missing, the installer shows a plain native
error telling the user to install it, then exits. No bootstrapper, no bundled engine.

## 8. Signing

The compiler tool has an optional signing step. The author gives the signing command in config,
and the tool runs it after the archive is appended, so the signature covers the whole file. Authors
without a certificate skip it. Spider's `release-desktop.yml` then calls the tool's signing step
instead of signing `spider-installer.exe` itself.

Built in step 4 (2026-09-29): `[sign]` takes a `command` and `args`, with `{file}` (the absolute path
of the file to sign) required in one of them. It runs from the config's folder, so a command
with a folder in it resolves from there; a bare name is looked up on `PATH`. `{` only starts
placeholders, so a script that needs braces belongs in a `.ps1` file. The command's output goes to the
console; a nonzero exit fails the build and deletes the file it was signing. `silk build --no-sign`
skips it for local test builds. `[sign]` is not packed into the installer. Use `pwsh`, not
`powershell`, when `silk` runs from PowerShell 7: Windows PowerShell started from pwsh inherits its
module path and loses the `Cert:` drive. `examples/spider.toml` shows Spider's Trusted Signing call.

The uninstaller is signed too. `silk build` first builds it (the same icon-patched stub, the theme
and the config, no payload), runs `[sign]` on it, then packs it into the installer's archive as
`uninstaller.exe` and signs the installer. At install time the installer extracts that entry, byte
for byte, as `Uninstall <product>.exe`. There is one path: without `[sign]`, or with `--no-sign`, the
same entry is simply unsigned. It adds the compressed stub to the download (about 3 MB), and a
second signing call per build. Free space and the Installed Apps size count the uninstaller's real
size.

History (2026-09-29): step 4 first had the installer write the uninstaller at install time from its
own image, which could not carry the signature. Checked then: SmartScreen never looked at it (its
reputation check follows the Mark of the Web, which the written file does not get, tested), and
nothing in a default Windows setup requires a signed uninstaller. But Smart App Control in
enforcement mode blocks unsigned code that Microsoft's cloud does not know, whatever launched it, and
every uninstaller is unique to its build. Publisher-based AppLocker or App Control policies block it
the same way, and a per-machine uninstaller would show "Unknown publisher" in its UAC prompt. Bart
chose signing at build time over a signed bare runtime with a sidecar file, or copying the whole
signed installer (double the disk use).

## 9. What changes for Spider, once built

- `installer/` shrinks to a config file and a theme folder. `install.rs`, `registration.rs` and
  `assets.rs` go away.
- The `.mjs` mirrors (`manifest.mjs`, `registration.mjs`), kept "byte-for-byte in step by hand" with
  the Rust, go away or move into Silk Setup's own tests.
- ADR-0021 and `cockpit.md` § 6 need amending to name Silk Setup.

## 10. Proven by prototype

`prototypes/appended-payload/` (2026-09-29) shows the stub-plus-appended-archive design works,
including after Authenticode signing:

- Signing appends the signature after the archive, so the runtime must find the archive through the
  PE header's security directory, not at the end of the file. Signing also pads to 8 bytes first.
- The signature covers the appended archive: changing one payload byte makes Windows report
  `HashMismatch`.

## 11. Open questions

Decided since: the archive is a compressed zip (deflate), read from disk rather than held in memory,
and still found through the prototype's trailer and PE security directory (Bart, 2026-09-29).
Per-machine installs needed more than HKLM, `Program Files` and a UAC manifest: all-users shortcuts,
elevation asked for at run time instead of in the manifest (one stub serves both scopes), and starting
the app at finish without administrator rights (§ 2, step 4 part B).

- How much of the theme JS API is versioned once published.

## 12. Later

Agreed with Bart on 2026-09-29 but deferred; the config should not block them. Themes that already do
these are collected in [theme-references/README.md](theme-references/README.md).

- Optional components the user ticks, each a subset of the payload with its size.
- A user-chosen install folder, picked in the theme through a native folder picker: the config's
  target is a default.
- User options that switch actions on or off at run time (a "Create desktop shortcut" checkbox): an
  action's setting in the config is a default.
- A resizable or maximisable window (frameless windows and the choice of native buttons shipped, § 4).
- Closing the window during install reaches the theme as an event, so it can confirm or cancel
  instead of the process dying mid-copy.
- The disk space the current component selection needs.
- The product icon, exposed to the theme.
- Optional online download of payload files: an installer that fetches some or all of its files
  instead of carrying them. Until then everything is packed inside the exe.

Consider before Spider switches over (§ 9):

- Registering URL protocols (e.g. `spider://` links) and file associations, removed on uninstall.
- Files in use: detect locked files beyond the main app (Windows Restart Manager), offer to close
  them or finish after a restart.
- Standard command-line options for IT and winget: silent install with a chosen folder, a log path,
  and documented exit codes, for install and uninstall.
- Fuller Installed Apps entry: estimated size, install date, help and website links.

Later still:

- Localisation of the runtime's own messages and language detection (themes already own their copy).
- Accessibility of the default theme: screen readers, keyboard-only use, high contrast.
- Following Windows light/dark mode and display scaling.
- Pre-install checks: minimum Windows version, architecture (x64/ARM64), a single running installer.
- Prerequisites such as a Microsoft runtime (the "run a program" action covers part of this).
- Repair and modify from Installed Apps.
- Uninstall options: keep or remove user data, open a feedback page afterwards.
- Deeper system changes: PATH, firewall rules, services, scheduled tasks, context-menu entries.

Not planned: MSI packages, delta patches between versions (apps update themselves), installer
telemetry.
