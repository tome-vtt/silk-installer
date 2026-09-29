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
# Append a payload folder to the runtime stub (defaults to silk-stub.exe next to silk.exe):
.\target\release\silk.exe build C:\path\to\payload .\MyApp-setup.exe
# Run it: a window with Install/Cancel, or no window at all with --silent (exit code 0 or 1).
.\MyApp-setup.exe
.\MyApp-setup.exe --silent
```

`cargo build --release` produces both `silk.exe` and the runtime stub `silk-stub.exe` (a Tauri 2 app
in `stub/`; needs the WebView2 runtime). For now the installer copies its files into
`%LOCALAPPDATA%\Programs\<installer file name without .exe>`; the config file will set this later.
Run on its own, `silk-stub.exe` carries no files and says so.

Sign the finished installer, never the stub: `silk` refuses a stub that is already signed or packed.
