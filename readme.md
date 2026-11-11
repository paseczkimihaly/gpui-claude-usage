A GPUI widget that shows your current Claude usage and statistics.

## Requirements

- Windows 10/11
- Rust (stable): `rustup update stable`
- Windows SDK (for `fxc.exe`, used to compile shaders in release builds). Set its path in `.cargo/config.toml` if your SDK version differs.
- Claude Code, logged in (the widget reads its local logs and login token from `~/.claude`)

## Run

```powershell
cargo run            # debug build, opens a console for logs
cargo run --release  # release build, no console
```

## Build

```powershell
powershell -ExecutionPolicy Bypass -File build.ps1
```

This produces a single standalone exe at `dist\claude-usage-<version>.exe`, with the version taken from `Cargo.toml` (also shown in the widget's header). Bump `version` in `Cargo.toml` before building a new release. If the widget is already running, the script closes it first.

## Start with Windows

Click `⋯` in the widget's header and turn on **Start with Windows**. This adds a `ClaudeUsage` entry under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` that points at the running exe (per-user, no admin needed). When you launch a newer release build, it updates the entry to point at itself.

## Test

```powershell
cargo test
```
