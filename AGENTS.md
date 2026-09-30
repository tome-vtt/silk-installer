# AGENTS.md

Instructions for coding agents in this repository, Spider's stage jobs included. It is the only copy:
there is no `CLAUDE.md` beside it, because Claude Code loads a root `AGENTS.md` only when no
`CLAUDE.md` exists at or above the working directory.

## What this is

Silk Setup builds themeable Windows installers from a folder, a TOML config and an HTML/CSS theme.
[docs/spec.md](docs/spec.md) is the design; [README.md](README.md) says how to use it.
[docs/theme-api.md](docs/theme-api.md) is the contract between a theme and the installer.

A Cargo workspace (Rust edition 2024), three crates:

- `archive/` (`silk-archive`): the config and the payload archive. Platform-free; the only crate that
  builds and tests off Windows.
- `compiler/` (`silk-compiler`, binary `silk.exe`): `silk build`, which packs a config, payload and
  theme onto the stub.
- `stub/` (`silk-stub`): the Tauri 2 installer runtime every built installer is. Calls Win32 through
  `windows-sys` and needs the WebView2 runtime.

`prototypes/` holds throwaway experiments, outside the workspace.

## Build and test

Windows, in PowerShell, from the repository root:

```powershell
cargo test --workspace --locked
cargo build --release --locked
```

Off Windows only `cargo test -p silk-archive --locked` works. CI runs that on Linux and the whole
workspace on Windows (`.github/workflows/ci.yml`).

## How work happens

Spider drives this repository as a Claude Code plugin: the only Spider-specific file here is
[`silk.yml`](silk.yml), which names the tracker team (**Silk Setup** on Linear), the branches, the
gate and the stages. Every stage needs a Windows node with Rust. Anyone hand-driving this repository
installs the Spider plugin once, at user scope:

```powershell
claude plugin marketplace add tome-vtt/Spider
claude plugin install spider@spider
```

Spider's plugin `README.md` covers agent identities, the shared lock and the stage skills.

- **Branches**: work branches are `spider/<ticket-id-lowercased>`, off `development`. `development` is
  the integration branch; `main` moves only by a pull request from `development` that Bart merges.
  Spider reads `silk.yml` from `main`.
- **Tickets land in Backlog.** Ready is Spider's dispatch queue; only a ticket Bart names goes there.
- **Releases** are `v*` tags Bart pushes; `.github/workflows/release.yml` builds them. Agents never tag.
- **Tests run through the `tests` lock** (the plugin's `lock.ps1 run tests -Command …`).
