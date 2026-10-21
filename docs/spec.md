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
  `runas`) with the same `--silent`, `--uninstall` and `--resumed` (§ 4) and the internal `--elevated` and `--user-sid=<sid>` (the
  user who started it, whose registry hive `silk.reboot` writes to), waits for that copy
  and exits with its exit code. The uninstaller does the same, since Installed Apps starts it without
  rights. A declined prompt exits with 1 and no message; a silent run over an existing install tries to
  log it, which only lands where the user may write (not in `Program Files`), and a declined
  fresh silent install logs nothing, since there is no install folder to hold the log. A run that already has the rights (an IT tool,
  Run as administrator, SYSTEM) goes straight on.
- **Target and registry.** `{programfiles}` is the 64-bit Program Files from the known-folder API.
  The config refuses `{localappdata}` in a per-machine target and `{programfiles}` in a per-user one.
  The Installed Apps entry, and the lookup of the installed version, use HKLM in the 64-bit view (the
  stub is a 64-bit build). `remove_registry_value` may name HKLM keys per-machine; HKCU keys stay
  allowed: an elevated uninstall deletes them under `HKU\<sid>`, the signed-in user's hive, using the
  SID the unelevated parent passed (`--user-sid`), or the session user's when started elevated directly.
  With no session user (session 0, such as a deployment tool running as SYSTEM) or on any failed lookup,
  the keys are deleted from the process's own HKCU, and the signed-in user's values stay behind. (Before
  SILK-S25-T1, an elevated uninstall in session 0 used the console user's SID.) A per-user uninstall always
  deletes from its own HKCU, never another account's hive; the session's user is the process's own session
  (`WTS_CURRENT_SESSION`), so over remote desktop it is the remote session's user, not the console's.
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
  rollback, `[[run]]` programs (they run elevated, a `talk` one too) and the self-deleting uninstaller.
  The theme's `silk.openUrl` hands the address to the running desktop shell (`explorer.exe "<url>"`, always
  quoted, since Explorer splits an unquoted argument at commas; an address holding `"` or ending in `\` is refused), so the
  browser opens at the user's own rights, not elevated (SILK-S29-T1; Bart's hand check on a spare PC is still
  to confirm it, including approving the UAC prompt with a different administrator account). With no desktop
  shell window (`GetShellWindow`: a replacement shell, a kiosk, a crashed Explorer) `openUrl` fails instead,
  since `explorer.exe` would become the shell itself and start the browser elevated (SILK-S51-T1).
  `elevation::elevate` decides whether to ask and what to ask with, taking the UAC call as an argument, so tests check it without a prompt. WebView2 keeps its
  data where it does for a per-user run, in the (elevating) account's
  `%LOCALAPPDATA%\dev.silk-setup.stub`, and it is left behind in both cases.
- Themes read `silk.info.scope`; the default theme says "Installs for all users of this PC".

## 3. How an author builds an installer

Like `makensis`: the author writes a config file and a theme folder, runs one command, and gets one
`.exe`. The author needs no Rust toolchain.

Distribution (2026-09-29): each `v*` tag publishes a GitHub release with `silk.exe`, the unsigned
`silk-stub.exe` and `SHA256SUMS` (`.github/workflows/release.yml`), its notes from
`docs/releases/<tag>.md`; consumers such as Spider's workflows download a pinned version with a
token that can read this repository (README).

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
  Close-the-running-app and launch-at-finish act on the main exe. `webview2_bootstrapper` (default
  `false`) packs Microsoft's WebView2 bootstrapper into the installer (§ 7). Unknown fields are errors.
- **`[[run]]`** runs a program at `at = "before-install"`, `"after-install"` or `"before-uninstall"`,
  hidden (`CREATE_NO_WINDOW`) in windowed and `--silent` runs alike; `program` and `args` may hold
  `{target}`, the install folder. Once it exits, the installer waits up to 5 seconds more for the
  processes it started to exit, so none holds a file the installer then deletes or rolls back; one started
  with `CREATE_BREAKAWAY_FROM_JOB` is not waited for. A nonzero exit fails the install or uninstall unless
  `fail_on_nonzero = false`. A before-install program (2026-10-05, for TomeVTT to remove an
  electron-builder NSIS install, which Silk would otherwise not see: it registers under a GUID key, not
  `...\Uninstall\<product>`) runs before any payload file is staged, once free space is checked and,
  when `close_running` is set, the running app closed (without a before-install program the app is
  closed after copying, just before the swap). Its failure leaves Silk's staging and the installed
  files untouched by Silk; the program itself may already have changed them, and a cancel ends it
  where it stands. Once it has succeeded, nothing undoes what it did: a later failure or cancel (for
  example copying, the swap, an after-install program, shortcuts, firewall rules or registration)
  rolls back only Silk's own files, so a program that removes the old install leaves the machine with
  none. It should leave a state a later install attempt can recover from. The payload is not on disk
  yet, so it names payload files as `{payload}\<path>`, each running to the end of its program or
  argument: the installer extracts just those to a `.silk-before-install-<pid>` folder beside the
  target (as safe from other users as the staging folder, which an elevated install relies on), runs
  the program from the target's parent folder, and deletes the folder afterwards. `silk build` refuses
  one the installer would not find, matching names as the installer does: either slash, ASCII case
  only (not other Unicode case or an 8.3 short name, which only the file system resolves). The
  program must finish its work before it exits. `talk = true` (2026-10-06, SILK-5, for Spider's node
  installer, whose bundled setup program asks questions) makes the theme its face in a windowed run:
  its stdout lines and exit code go to the theme as events and the theme writes lines to its stdin
  (§ 4); Silk only moves lines.
  Without a window it runs as any other. A `program` that is a bare name (`powershell`, `cmd`) is looked
  up in the Windows system folder, then in `WindowsPowerShell\v1.0` beside it, and never in the
  installer's own folder (2026-10-11, SILK-S38-T1: a file dropped beside a downloaded installer would
  run elevated); one not found there fails the step. A `program` with a folder is run as given.
  Example, as TomeVTT uses it:

  ```toml
  [[run]]
  program = "powershell"
  args = ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", '{payload}\tools\remove-nsis.ps1', '{target}']
  at = "before-install"
  ```
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
- Added 2026-10-06 (SILK-5): `output` and `exit` events from a `talk` `[[run]]` program, `write(line)`
  to its stdin, and `openUrl(url)`, `http`/`https` only, in the default browser.
- Added 2026-10-06 (SILK-6): `reboot()` and `silk.info.resumed`, for a run step that needs Windows
  restarted. The theme's call is the user's confirmation (its own page asks; Silk shows no dialog of
  its own). The stub writes a RunOnce value, `<product> Setup` (under HKCU, or under `HKU\<sid>` of
  the user named by `--user-sid=` when an elevated run writes it), of its own path with
  `--resumed`, then runs `shutdown /r /t 0 /d p:4:2`; a failed restart deletes the value again. A call
  while `start()` runs, or in uninstall mode (the uninstaller is gone afterwards), is refused. Windows
  runs the value unelevated at the user's next sign-in, so a per-machine installer asks for elevation
  again (§ 2, passing `--resumed` on), which also keeps launch at finish with its unelevated parent.
  Not HKLM RunOnce, which would run it elevated without a prompt: the installer usually sits in the
  user's Downloads, so anything able to replace it there would run as administrator; and Windows runs
  HKLM entries only at an administrator's sign-in, holding back the desktop until they end. When another
  account answered the UAC prompt, HKCU is that account's: the unelevated parent passes its user SID as
  `--user-sid=`, and the elevated copy writes (and on a failed restart deletes) the value under
  `HKEY_USERS\<sid>` instead. A process started already elevated as another account (Run as
  administrator with that account's credentials, § 2) has no parent to say: a per-machine one looks
  up the user signed in to its own session, on the console or over remote desktop
  (`elevation::registered_user`), and falls back to its own HKCU if it finds nobody or the lookup
  fails in any way (WTS or `LookupAccountNameW` errors, say a `domain\user` name it cannot resolve).
  A per-user one keeps its own HKCU, since it installs into that account's profile. Not covered: an
  installer started as another account from outside the signed-in user's session (a scheduled task,
  a service or `psexec`), which registers for that session's user, or for its own HKCU in session 0. Windows
  deletes a RunOnce value before running it, and a run with `--resumed` deletes it too, so it starts
  once. The resumed window sets `silk.info.resumed` and
  `api.js` calls `start()` on the page's `load` event, once the theme's scripts listen. `api.js` runs
  on every page the theme opens, so it asks `resume_owed` first: the run is owed until a `start`
  takes the archive lock, so a page left before then hands it to the next, and once a run has
  started no later page starts it again. The whole install runs again, files and `[[run]]`
  programs, and the setup program skips what it already did.
  `reboot::reboot` takes the restart as an argument, so tests check the registration without
  restarting.
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
- create and remove shortcuts (Start menu, optionally desktop; the all-users ones per-machine), each carrying the config's `shortcuts.app_user_model_id` as its
  `System.AppUserModel.ID` when set;
- write and remove the Installed Apps entry (HKCU or HKLM by scope), including the uninstaller (built
  and signed by `silk build`, extracted with the payload so it swaps and rolls back with it) and the
  self-deleting uninstall (after a windowed uninstall, once the window closes; a detached
  `cmd` retries removing the folder for about 10 s);
- remove named registry values the app wrote itself (Spider's `spider-desktop` Run value): under HKCU,
  or per-machine also HKLM;
- add inbound Windows Firewall rules (`[[firewall]]`, per-machine only) on install, each replacing any
  inbound rule of its name so an upgrade leaves one copy, and delete them on uninstall; a rule an
  upgrade renames or drops stays behind. New rules are added under a temporary name and renamed once all
  are added, so a failed add leaves the previous version's rules in place (a rename failing partway can still lose an
  old rule: each is deleted just before its replacement is renamed). A failure later in the install
  (registration) rolls back the files, and on a fresh install also removes the shortcuts, rules and
  Installed Apps entry it added (an upgrade keeps them), and a failed fresh install writes no `install.log`,
  since its folder is gone. Registration writes the entry's values one at a time, so a failed upgrade can leave
  the entry part new version and part previous (its version already the new one, say) over the previous
  version's files, until an install succeeds.
  Through `netsh advfirewall`, not the `INetFwPolicy2` COM API:
  `windows-sys` has no COM interface bindings, and the stub already shells out (PowerShell, `cmd`);
- **run a program** with arguments, at a declared point (before install, after install, before
  uninstall; § 3), failing the install on a nonzero exit.

## 6. Upgrade

Silk Setup keeps Spider's upgrade behaviour: running over an older version replaces it in place with
rollback, and `--silent` does that without a window, is a no-op when the installed version is the
same or newer, and writes `install.log` (not for a fresh install that fails or is declined, at any step including
before the swap: there is no install folder yet, or it is rolled back, so there is no log).
Checking for and fetching new versions stays in the app.

## 7. WebView2

The runtime needs the WebView2 runtime. Windows 11 ships it and Windows Update has pushed it to
Windows 10, but it can be missing on locked-down, LTSC, Server or offline machines. The installer
checks for it before asking for administrator rights, and only when it will open a window: `--silent`
never needs it, so a silent install completes without it, whatever the config says.

By default, when it is missing, the installer shows a plain native error telling the user to install
it, with Microsoft's download page, and exits with code 1.

With `webview2_bootstrapper = true` (2026-10-05; default `false`), `silk build` packs
Microsoft's Evergreen bootstrapper, `MicrosoftEdgeWebview2Setup.exe` (about 1.8 MB), into the
installer's archive as `webview2-setup.exe`; the uninstaller does not carry it. `silk.exe` embeds the
file, vendored with its source, version, hash and Microsoft's redistribution terms in
`compiler/webview2/`, so a build fetches nothing; Microsoft's docs allow packaging the bootstrapper
with an app, on the WebView2 Runtime licence's terms, which the author's installer takes on. When the
runtime is missing, the installer writes the bootstrapper to a fresh folder in the temp folder, runs
it with Microsoft's `/silent /install` and waits, then checks again and carries on to the themed
window. The bootstrapper shows no window of its own, so while it downloads and installs the runtime
(a minute or more on a slow connection) Setup shows a small native please-wait window, "Setup is installing
the Microsoft Edge WebView2 Runtime", with no close box; it closes when the bootstrapper finishes, before the
themed window opens or an error shows. A `--silent` run needs no window and creates none. It installs it
per-user when Setup runs without administrator rights, so a per-user install asks for none, and
per-machine when Setup runs elevated. A per-machine installer checks before it asks for elevation, so
it too installs a per-user runtime for whoever started it, which its elevated copy then finds (a
different administrator account, elevating over the shoulder, runs it again, elevated). The
bootstrapper downloads the runtime, so this does not help an offline machine: when it fails, or the
runtime is still missing after it, the installer shows a native error naming WebView2 and linking to
Microsoft's download page, and exits with code 1. No bundled engine: the ~130 MB standalone runtime
installer is out of scope.

When the runtime is installed but the window still cannot be made (a broken or blocked runtime), the
installer shows a native error saying it could not open its window, with WebView2's reason, and exits
with code 1.

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
- Deeper system changes: PATH, services, scheduled tasks, context-menu entries (firewall rules shipped, § 5).

Not planned: MSI packages, delta patches between versions (apps update themselves), installer
telemetry.
