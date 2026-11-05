# Pinkbin

A portable Windows disk analyzer with an AI conversation interface and rule-based cleanup. Based on [cccyd2003-qwq/pinkbin](https://github.com/cccyd2003-qwq/pinkbin), with upstream history and the original [MIT license](LICENSE) preserved.

[中文说明](README.md) · [Download](https://github.com/OmoriRay/Pinkbin/releases/latest)

Current version: **0.11.7**, Windows 10 / 11 x64. Download `Pinkbin_0.11.7_windows_x64.zip` from Releases, extract it into a writable folder, and run `Pinkbin.exe`. Do not run it directly from the archive. WebView2 Runtime is required.

Pinkbin removes Studio, adds fixed-drive and known-cache checks, cleanup history, file information copying, scan reconnection after UI reload, pre-scan AI chat, response timing, a connection test and HTTP connection reuse.

Version 0.11.7 activates the existing window when launched again from the same portable directory, restores minimized windows and blocks a conflicting legacy version. Separate portable directories stay independent. Rules v7 add 30-day OBS session/crash reports, Stability Matrix archives and Unreal CrashReportClient backup logs while retaining current logs, the latest OBS reports and application data.

Version 0.11.6 removes lifted checkbox shadows and focus movement, uses compact rounded checkboxes for software choices, and preserves keyboard-only focus outlines.

Version 0.11.5 uses the fixed executable name `Pinkbin.exe` and adds old Steam web caches and installed Electron updater copies. Pending updates are preserved; removing a differential base may require a full download next time. Keep `PinkbinData` when replacing the program and rules.

Version 0.11.4 shows successful AI connection tests in a mint banner with a checkmark and elapsed time. Version 0.11.3 adds selectable software groups to Quick Cleanup, preserves exclusions across reloads of the same scan, and lets cleanup results recheck the original drives. Both execution and history contain only selected projects.

Version 0.11.2 fixes missing Quick Recheck routes for approved cache templates such as GraphiteDawnCache and application Crashpad/reports directories. Standard locations use the existing cache templates, and full scans can persist the newly recognized boundaries. Eligibility, live process, identity and ancestry checks remain required; arbitrary reports directories are excluded.

Version 0.11.1 removes repeated instructions, slogans and developer scan timings from the interface. Scan coverage, cleanup rules, permanent deletion and progress reporting are unchanged. AI waiting feedback is shown once; settings accurately describe messages, file metadata and attached images sent to the configured service.

Version 0.11.0 adds Quick Recheck in Disk Check. A full scan learns a bounded set of verified cleanup directories; later runs enumerate their current files rather than walking the whole drive. Standard locations are available on first use. Newly installed or moved software still requires a full scan. Quick results do not populate the homepage tree or manual file lists. Hints live in `PinkbinData/known-cleanup-locations-v1.json`, capped at 512 KiB and 2,048 directories, with per-volume quotas. They never authorize deletion; live rules, identity, ancestry, age and process guards remain mandatory.

The v5 catalog adds strictly identified expired Hugging Face Xet shards (45+ days) and non-resumable random download leftovers (30+ days), requiring a matching completed asset and software metadata. Models, upload staging, resumable downloads and unknown layouts are retained. Up to 512 bytes of software cache metadata may be read, never personal document/model bodies or sent to AI. The frozen v1-v4 catalogs and existing portable data remain available. uv caches are excluded from direct deletion per their official safety guidance.

Version 0.10.1 fixes large files disappearing from crowded space trees: sibling files and directories receive display slots before descending into details, retaining the existing size/tag priorities and budgets. Revisiting C after D reuses the same scan identity without another AI overview; an explicit fresh scan still produces one. Only a bounded set of identities is remembered in memory; cleanup rules and portable data remain unchanged.

Full-drive analysis and cleanup now share one guarded metadata traversal. Within the same process, a completed inventory can be reused for 15 minutes, concurrent requests join the current job, and expanding C to C/D/E only traverses D/E. The normal Scan button prefers reuse; the refresh icon and cleanup's Recheck action request fresh data. Cleanup invalidates old plans and trees, and deletion still verifies live file identity, ancestors and process state. Reusing an identical result does not repeat the AI overview. Shared inventories remain in memory and disappear on exit. Full-drive analysis now uses the complete walk rather than the MFT-only tree fast path; standalone folder analysis retains the original scanner. The v4 cleanup catalog is unchanged.

Settings and application data live in **PinkbinData beside the executable**. Move that folder and the rules directory together with the executable. It contains local AI configuration and must not be committed or shared.

After a full-drive scan, Quick cleanup is available in the recommendation card and pinned footer. It combines known browser caches with allowlisted files identified during the full-drive walk: expired temporary files, old dumps/logs, package download caches, Python bytecode with matching source, verified portable editor caches, old Windows error reports and kernel dumps, NuGet HTTP responses, Rust incremental artifacts, Epic web caches, JetBrains indexes and additional Chromium browser caches. It excludes manually selected files and does not call AI. Running or unverifiable applications are skipped; accounts, chat records, downloads and container data are excluded.

Large or old files are not automatically considered junk. Full-drive candidates require manual selection. Clicking Delete permanently immediately deletes the selected files without an extra confirmation screen. Deleted files do not enter the Recycle Bin and cannot be restored from it. AI file analysis uses metadata; images explicitly attached to a conversation are sent to the configured AI provider.

Version 0.8.0 uses a curated JSON catalog at `rules/cleanup-catalog-v7.json` beside the executable, with an embedded fallback for standalone use. The catalog holds application paths, process guards, retention and exclusions; invalid external catalogs block cleanup scanning. Restart after updating it. The catalog includes Obsidian and Notion renderer caches, preserving notes, offline data and backups. Engine-owned cache templates and existing guarded deletion remain in effect. See [catalog maintenance](rules/README.md). No Winapp2 database or registry/uninstaller rules are imported.

Version 0.6.0 added locally audited cloud-drive UI caches, Electron partitions, WebView profiles and old Cinebench render caches. Additional portable layouts are tied to executable and product markers. Quick cleanup runs as a backend job in batches, with progress reconnection after a UI reload and one history record. Snapshots are bounded to 200,000 items / 256 MiB, with a 15-minute overall execution limit. Manual selection remains limited to 10,000 files. The v2 catalog has its own filename so the v0.5.0 catalog can coexist.

Version 0.7.0 broadens read-only space inventory while preserving deletion boundaries. It adds QQ, WeGame, MusicFree, Wallpaper Engine UI caches and old Cargo archives backed by extracted sources, groups recommendations by application, and exposes a collapsed space overview. Existing v1/v2 catalogs coexist with v3. Automated deletion validation uses owned temporary fixtures; real-device auditing is read-only.

Version 0.8.0 expands cleanup types with expired Crashpad reports, rotated/dated application logs, npm diagnostic logs, archived CBS/DISM maintenance logs and known Inno Setup/NSIS temporary workspaces. Live logs, database logs, report state and repair data remain protected. Workspace creation/modification ages and process guards prevent fresh extraction of old payloads from becoming a cleanup plan. The v4 catalog coexists with the frozen v1/v2/v3 files. Real-device auditing is read-only.

Version 0.9.0 groups recommendations by cleanup type with collapsible application details. Scan-scoped reuse pre-resolves application roots and caches bounded product/source marker metadata; it expires with the scan. Execution always rechecks with an uncached probe. The v4 rule file is unchanged and existing data is preserved.

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
