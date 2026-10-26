# Pinkbin

A portable Windows disk analyzer with an AI conversation interface and rule-based cleanup. Based on [cccyd2003-qwq/pinkbin](https://github.com/cccyd2003-qwq/pinkbin), with upstream history and the original [MIT license](LICENSE) preserved.

[中文说明](README.md) · [Download](https://github.com/OmoriRay/Pinkbin/releases/latest)

Current version: **0.4.2**, Windows 10 / 11 x64. Download the ZIP from Releases, extract it into a writable folder, and run the executable inside it. Do not run it directly from the archive. WebView2 Runtime is required.

Pinkbin removes Studio, adds fixed-drive and known-cache checks, cleanup history, file information copying, scan reconnection after UI reload, pre-scan AI chat, response timing, a connection test and HTTP connection reuse.

Settings and application data live in **PinkbinData beside the executable**. Move that folder together with the executable. It contains local AI configuration and must not be committed or shared.

Large or old files are not automatically considered junk. Full-drive candidates require manual selection. Clicking Delete permanently immediately deletes the selected files without an extra confirmation screen. Deleted files do not enter the Recycle Bin and cannot be restored from it. AI file analysis uses metadata; images explicitly attached to a conversation are sent to the configured AI provider.

## Development

Install Rust MSVC, Node.js 20+, pnpm 9 and the Tauri Windows prerequisites.

```powershell
pnpm install --frozen-lockfile
pnpm tauri dev
pnpm tauri build --no-bundle
cargo test --workspace
```

The portable executable is generated at target/release/pinkbin.exe.

Package the portable ZIP with `./scripts/package-portable.ps1`. GitHub Releases publishes ZIP packages only.
