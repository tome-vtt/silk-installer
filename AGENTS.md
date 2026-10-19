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
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings -A clippy::cognitive_complexity
```

Formatting follows [`rustfmt.toml`](rustfmt.toml) (`max_width = 120`, `use_small_heuristics =
"Max"`, which keeps the code's one-line structs and calls); run `cargo fmt --all` before committing.

Clippy runs over every target with warnings denied, in CI and the gate; `cognitive_complexity` alone
is allowed there, since the complexity check below owns it and its baseline. Beyond clippy's
defaults, `[workspace.lints.clippy]` turns on three lints, cherry-picked rather than whole groups:

- `undocumented_unsafe_blocks`, denied: the stub runs elevated and calls Win32 directly, so every
  `unsafe` block carries a `// SAFETY:` comment saying what makes it sound (handle validity, buffer
  sizes, lifetimes).
- `cast_possible_truncation` and `cast_sign_loss`, for archive offsets and Win32 sizes: convert with
  `try_from`, or, only where the range is provably safe, `#[allow]` the one statement with a
  `reason`.

Everything, `silk-archive` included, needs Windows: its path checks follow Windows path rules.
`.github/workflows/ci.yml` has three jobs: `windows` tests the whole workspace, `deny` runs
cargo-deny and `workflows` runs actionlint (with shellcheck) and zizmor over the workflows and `dependabot.yml`.
Dependabot (`.github/dependabot.yml`) is set to weekly grouped updates into `development`, but with
`open-pull-requests-limit: 0` it opens only security-update pull requests; Spider's weekly catch-up
ticket moves the version pins.

Every installer redistributes the dependency tree, so [`deny.toml`](deny.toml) checks its advisories
and licences with cargo-deny, pinned to the same version in CI and the gate (`cargo install
cargo-deny --version 0.20.2 --locked`, then `cargo deny --locked check`). Its licence list is permissive only: a copyleft
licence goes to Bart, and only he allows one, crate by crate (MPL-2.0 so far).

## Function complexity and nesting

Every non-test function is capped by clippy's `cognitive_complexity` lint at 4 (`clippy.toml`),
which flags about the same share of this code as the cyclomatic cap of 12 that Spider and Tome VTT
use: clippy counts mostly `if`s and loops, not `match` arms, `&&` or `?`, so its numbers run far
lower. Tests are not linted: clippy counts every `assert!` as a branch.

Nesting is capped by clippy's `excessive_nesting` lint at 5 (`clippy.toml`), denied workspace-wide,
so `cargo clippy --workspace` itself fails on a block nested deeper. Clippy counts every braced
block, the function body, an enclosing `impl`, closures and inline `if`s included, so 5 is roughly
ESLint `max-depth` 4, the cap Spider and Tome VTT are adding. Nothing was over it when it came in, so it
has no baseline. Get under it with guard clauses and early returns (`let ... else`, `?`), `continue`
in loops, and small named functions for an inner loop's or closure's body.

[`complexity-baseline.json`](complexity-baseline.json) holds the functions over the complexity cap
when it came in, and **it never grows**. `node scripts/complexity.mjs` (the gate runs it, with its
test `node --test scripts/complexity.test.mjs`) fails on a new function over the cap or an entry
that rose, and on an entry left as it was after its function was fixed (delete it) or simplified
(lower it), on any block nested past the nesting cap, or on any `#[allow]` or `#[expect]` of either
lint, their groups or `warnings` (comments and `warn`/`deny` attributes pass). Get under a cap by
splitting the function or flattening it with guard clauses and helpers, never by silencing either
lint or raising an entry. The after-edit hook in `.claude/settings.json` runs the same check on the
edited crate and warns, never blocks: on an edit that adds or raises an entry, and on every block
nested past the cap in the edited crate and the workspace crates it depends on.

Cognitive complexity proper, Sonar's algorithm with its nesting increment that Spider and Tome VTT
cap at 15, is capped at 15 here too. Clippy's lint is not that algorithm, so Mozilla's
rust-code-analysis measures it: `rust-code-analysis-cli`, pinned to 0.0.25 (`cargo install
rust-code-analysis-cli --version 0.0.25 --locked`, which the gate runs itself). A closure counts
toward the function around it; tests, the trailing `#[cfg(test)]` module, are not measured.
[`cognitive-baseline.json`](cognitive-baseline.json) holds the 2 functions over 15 when it came in,
under the same shrink-only rules, checked by `node scripts/cognitive.mjs` (the gate runs it, with
its test `node --test scripts/cognitive.test.mjs`). The after-edit hook runs it on the edited file
and warns, never blocks, when an edit adds or raises an entry.

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
- **Control plane commands**: run `spider --help` to see them; `spider <command> --help` describes
  one, and `status`, `jobs` and the other read commands take `--json`.
- **Tickets land in Backlog.** Ready is Spider's dispatch queue; only a ticket Bart names goes there.
- **Releases** are `v*` tags Bart pushes; `.github/workflows/release.yml` builds them. Agents never tag.
- **Tests run through the `tests` lock** (the plugin's `lock.ps1 run tests -Command …`).
