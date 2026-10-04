# AGENTS.md

Instructions for coding agents in this repository, Spider's stage jobs included. It is the only copy:
there is no `CLAUDE.md` beside it, because Claude Code loads a root `AGENTS.md` only when no
`CLAUDE.md` exists at or above the working directory.

## What this is

Silk Setup builds themeable Windows installers from a folder, a TOML config and an HTML/CSS theme.
[docs/spec.md](docs/spec.md) is the design; [README.md](README.md) says how to use it.
[docs/theme-api.md](docs/theme-api.md) is the contract between a theme and the installer.

A Cargo workspace (Rust edition 2024), three crates:

- `archive/` (`silk-archive`): the config and the payload archive. Compiles anywhere, but its path
  checks follow Windows path rules, so its tests pass only on Windows.
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

Everything, `silk-archive` included, needs Windows: its path checks follow Windows path rules. CI
tests the whole workspace on Windows (`.github/workflows/ci.yml`).

## Function complexity

Every non-test function is capped by clippy's `cognitive_complexity` lint at 4 (`clippy.toml`),
which flags about the same share of this code as the cyclomatic cap of 12 that Spider and Tome VTT
use: clippy counts mostly `if`s and loops, not `match` arms, `&&` or `?`, so its numbers run far
lower. Tests are not linted: clippy counts every `assert!` as a branch.

[`complexity-baseline.json`](complexity-baseline.json) holds the functions over the cap when it came
in, and **it never grows**. `node scripts/complexity.mjs` (CI and the gate run it, with its test
`node --test scripts/complexity.test.mjs`) fails on a new function over the cap or an entry that
rose, and on an entry left as it was after its function was fixed (delete it) or simplified (lower
it), or on any `#[allow]` or `#[expect]` of the lint or its groups. Get under the cap by splitting
the function, never by silencing the lint or raising an entry. The after-edit hook in
`.claude/settings.json` runs the same check on the edited crate and warns, never blocks, when an
edit adds or raises an entry.

## How work happens

Spider drives this repository as a Claude Code plugin: the only Spider-specific file here is
[`silk.yml`](silk.yml), which names the tracker (Spider's own, prefix `SILK`), the branches, the
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
- **Tickets** live on Spider's own tracker, which the control plane serves: Bart works them on the
  Loom's Tickets page, and sessions use the plugin's tracker tools (`mcp__plugin_spider_tracker__*`,
  repository `silk-setup`), never the Linear MCP. Imported tickets keep their `SILK-<n>` ids; new
  ones are `SILK-S<n>-T<m>`.
- **Tickets land in Backlog.** Ready is Spider's dispatch queue; only a ticket Bart names goes there.
- **Releases** are `v*` tags Bart pushes; `.github/workflows/release.yml` builds them. Agents never tag.
- **Tests run through the `tests` lock** (the plugin's `lock.ps1 run tests -Command …`).
