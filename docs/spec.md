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
- **Install scope**: both per-user (no UAC, `%LOCALAPPDATA%\Programs`, HKCU) and per-machine (UAC,
  `Program Files`, HKLM). Each installer picks one in its config.
- **Out of scope**: update checking, downloading or feeds (the app keeps that, as Spider's
  `desktop/src/updater.ts` does), translations, macOS and Linux, a scripting language.

## 3. How an author builds an installer

Like `makensis`: the author writes a config file and a theme folder, runs one command, and gets one
`.exe`. The author needs no Rust toolchain.

- **Runtime stub**: a prebuilt Tauri exe, the generalised form of today's `spider-installer.exe`.
- **Compiler tool**: reads the config, packs the payload directory, the theme and the resolved config
  into an archive, and appends it to a copy of the stub. The stub reads the archive from its own file
  at start. This replaces today's compile-time `rust-embed` of `desktop/release/win-unpacked/`.
- **Config** declares at least: product name, version, publisher, icon, install scope, target
  directory, main exe, payload directory, theme directory, shortcuts, and the action list (§ 5).

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

## 5. Actions

A fixed set of built-in actions, plus one escape hatch:

- copy the payload into place through a staging directory, with rollback on failure (today's
  `replace_target`) and a free-space check;
- close the running app, and launch it at finish;
- create and remove shortcuts (Start menu, optionally desktop);
- write and remove the Installed Apps entry (HKCU or HKLM by scope), including the uninstaller copy
  and the self-deleting uninstall;
- remove named registry values the app wrote itself (Spider's `spider-desktop` Run value);
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

- Archive format and whether it is compressed. The prototype uses a bare uncompressed format held
  in memory.
- Whether per-machine installs need anything beyond HKLM, `Program Files` and a UAC manifest (for
  example, all-users shortcuts).
- How much of the theme JS API is versioned once published.
