# 清理规则库

`cleanup-catalog-v7.json` 是本项目独立整理的 MIT 元数据规则，不是 Winapp2 导入文件。它覆盖 14 个常用缓存兼容项目、49 个应用配置、14 类特殊保留策略、6 个既有便携程序标记及 7 个便携应用配置和 3 个已安装更新副本配置。应用配置数不等于本机命中数量。

## 位置与更新

ZIP 将文件放在 exe 同级 `rules/cleanup-catalog-v7.json`。缺失时使用内置版本；外部文件存在但无法读取、解析或未通过验证时阻止清理扫描。首次调用清理功能时加载，随后进程内固定；更新规则需重启并重新扫描。已有扫描快照继续使用原规则，避免执行时扩大范围。规则不联网下载。

## 配置字段

- schema_version：目前为 7；revision 是维护版本，license 为 MIT。v5 必须包含完整策略；未知字段或格式拒绝。v4 仅保留解析兼容，不启用新增的 Hugging Face 语义。
- templates：固定的 Chromium/Electron 缓存子目录，只可缩小引擎白名单；不接受用户数据根或任意 glob。
- policies：临时文件、转储、下载缓存、Python/Rust 等的保留天数与进程。天数不可低于内置安全下限，进程别名不可删减。
- legacy：常用缓存兼容项目；路径和类型必须与内置边界一致，允许增加进程保护、提高保留天数与修改显示标题。
- profiles：应用 ID、名称、family、locations、processes、min_age_days、enabled、reviewed、exclude、markers_any、logs、sources。family 支持 electron/chromium/firefox/epic/jetbrains/cinebench/wegame；locations.root 只支持 local/roaming，path 为相对路径，不接受盘符、环境变量、通配符或父级跳转。新目录仍受引擎缓存模板限定，不能凭配置删除任意文件。
- partitions：Electron 可识别 Partitions/会话名 下相同的固定缓存叶目录；不会清空会话本身、IndexedDB、Cookie 或离线存储。
- portable_apps：已知程序、进程、产品标记和数据子目录；Electron 需产品标记，Chromium 需 Local State；缓存仍受引擎模板约束。
- exclude：相对应用根的大小写不敏感 glob，仅排除；markers_any 是相对应用目录的可选普通文件标记（任一存在）。JetBrains 另需 product_prefix 和至少 7 天；logs 仅识别旧时间戳会话目录中的 .log。
- portable_editors/portable_browsers：已知程序文件、名称和进程；引擎继续验证产品/Local State 标记及固定目录结构。

已有应用的进程别名不能移除，新增配置必须显式 enabled=true 且 reviewed=true 才参与推荐；reviewed 只是维护记录。还需核验官方目录资料、账号/工作资料相邻边界、运行状态和真实缓存行为，不把“默认选中”作为安全依据。

## 验证

```powershell
cargo test --release --workspace --no-fail-fast
cargo clippy --release --workspace --all-targets -- -D warnings
pnpm tauri build --no-bundle
./scripts/package-portable.ps1
```

在现有类型/模板内加应用可仅更新 JSON；新增清理语义仍需改引擎和边界测试。Python 对应源码、Rust 项目标记、系统目录有限路由、候选预算、文件身份、链接/占用和删除日志属于引擎保护，不能用 JSON 关闭。


v1/v2/v3/v4 文件冻结保留，供旧版程序使用。v0.11.0 只从 v5 文件路径加载外部规则。WeGame 和 Wallpaper Engine 使用引擎限定的界面缓存路径；Cargo 策略仅处理有已解压源码对应的旧 .crate 包，不处理 credentials.toml、bin 或 registry/src。显示名称按软件生成，推荐按体积排序。


v4 的 profiles/portable_apps 增加 maintenance 格式名单：crashpad、electron-rotated、cherry-daily、qq-launcher。目录继续来自已知配置和产品/会话标记，格式名单只能启用引擎固定的诊断模板，未知名称会被拒绝。全局 dumps/logs 至少 30 天；当前日志、报告状态和数据库日志不匹配。日期归档保留最新一份，目录枚举有界。系统归档和安装暂存由新 policy 参数控制，路径、文件名、目录年龄和系统路由由引擎固定，不能通过 JSON 关闭。

格式依据：[Electron 崩溃报告](https://www.electronjs.org/docs/latest/api/crash-reporter)、[electron-log 轮转](https://github.com/megahertz/electron-log/blob/master/docs/transports/file.md)、[Inno Setup 临时目录](https://jrsoftware.org/ishelp/topic_consts.htm)、[NSIS 插件暂存](https://nsis.sourceforge.io/Reference/$PLUGINSDIR)、[DISM 归档日志](https://learn.microsoft.com/en-us/powershell/module/dism/get-wimbootentry)、[CBS 归档](https://learn.microsoft.com/en-us/troubleshoot/azure/virtual-machines/windows/troubleshoot-cbs-component-store-corruption-azure-vm)、[npm logging](https://docs.npmjs.com/cli/v11/using-npm/logging)。Cherry Studio/QQ 的具体文件名另经本机元数据核对，原始本机路径只留 qa/v080，不进入发布包。格式可能随应用版本变化，未知结构保持未匹配。

## v5 Hugging Face 边界

- `hf-xet` 至少保留 45 天。只接受已配置的 HF Xet 根下 `https___环境标识/shard-cache/64位小写十六进制.mdb` 普通文件，且父链无链接。HF_XET_CACHE、HF_HOME、XDG_CACHE_HOME、USERPROFILE 按官方优先级解析；保留整个 staging、旧 chunk_cache、hub 模型/refs/blobs/snapshots 与账号文件。官方说明 shard 通常 3–4 周过期，过期后一周删除；本规则使用更保守的 45 天。
- `hf-partial` 至少保留 30 天。只接受 `local_dir/.cache/huggingface/download` 内源码定义的 `base64url(SHA1(metadata文件名)).etag.8位随机hex.incomplete`。必须有普通 `.gitignore`、`CACHEDIR.TAG`、对应 metadata 和已完成目标；读取软件生成 metadata 最多 512 字节，要求固定三行有效 commit/匹配 etag/有限且不在未来的 timestamp。残片与完成目标的修改时间均不得晚于 timestamp。目标、metadata、锁、upload 目录及没有随机段的可续传残片保留。
- 两类规则都在每次识别时重新检查文件与 marker；执行仍走统一的当前身份、父链、硬链接、运行进程和占用校验。不会把扫描 marker 缓存用作执行授权。Python/HF/git 相关进程别名和最低保留期不可从规则库移除或降低。

一手依据固定于 Hugging Face commit `798ed1d47813d236ea805e157a3973f98bb34dde`：[Xet 缓存说明](https://github.com/huggingface/huggingface_hub/blob/798ed1d47813d236ea805e157a3973f98bb34dde/docs/source/en/guides/manage-cache.md#xet-caching)、[local_dir 名称与三行 metadata](https://github.com/huggingface/huggingface_hub/blob/798ed1d47813d236ea805e157a3973f98bb34dde/src/huggingface_hub/_local_folder.py)、[随机临时文件与 finally 清理](https://github.com/huggingface/huggingface_hub/blob/798ed1d47813d236ea805e157a3973f98bb34dde/src/huggingface_hub/file_download.py#L1970-L2048)。旧式可续传文件和不认识的新格式一律不处理。

uv 不纳入直接删除规则：[官方 Cache safety](https://docs.astral.sh/uv/concepts/cache/#cache-safety) 明确禁止直接修改缓存，需用 uv 自身的 clean/prune。Pinkbin 不自动执行外部命令。

## v6 安装后更新副本与 Steam

`updater_caches` 为三条固定映射，关联既有 Electron profile；目录、关联软件和至少 30 天的门槛均由内置基线约束。只接受 LOCALAPPDATA 下对应更新器根目录中的 `installer.exe`，要求关联软件的普通 Local State 标记存在；当前软件/安装器进程保护与既有身份、父链、句柄校验继续生效。`pending`、更新元信息、其他 exe/7z、未知更新器不匹配。规则可关闭或延长保留期，不能通过 JSON 改成其他目录或关联软件。

NSIS 安装源码明确将完成安装时的 EXEPATH 复制为安装缓存，供下次差分更新使用；缺失时可退回完整下载。参考 [NSIS installer.nsh](https://github.com/electron-userland/electron-builder/blob/master/packages/app-builder-lib/templates/nsis/include/installer.nsh)、[DownloadedUpdateHelper](https://github.com/electron-userland/electron-builder/blob/master/packages/electron-updater/src/DownloadedUpdateHelper.ts)、[NsisUpdater](https://github.com/electron-userland/electron-builder/blob/master/packages/electron-updater/src/NsisUpdater.ts)。这类副本不作为待安装残留删除，产品名称明确为旧更新安装包。

Steam 只检查 LOCALAPPDATA/Steam/htmlcache 的既有 Chromium 缓存叶目录，保留至少 7 天并要求 Local State；账号、Cookie、本地存储、游戏、创意工坊、下载和存档不属于这些模板。CEF 根目录同时承载持久资料，因此不得清空 htmlcache，依据 [CEF settings](https://github.com/chromiumembedded/cef/blob/master/include/internal/cef_types.h) 及本机元数据核验限定子目录。

v6 沿用 v5 HF 语义；v4/v5 仍可解析，但不会获得新更新副本授权。新程序只加载 v6 外部文件；旧 v1–v5 冻结保留给旧程序。快速复查仅添加固定更新器根和模板缓存位置，候选及执行资格每次重新计算。

## v7 归档诊断日志

新增三个 diagnostics profile，目录、标记和格式由内置基线限定，外部文件只能关闭或延长保留期。OBS 仅 logs/YYYY-MM-DD HH-MM-SS.txt 与 crashes/Crash YYYY-MM-DD HH-MM-SS.txt，必须有普通 global.ini，并在同一目录存在格式有效的更新一份，保留最新。Stability Matrix 仅 Logs/app.YYYY-MM-DD HH_mm_ss.log，要求 global.json 与当前 Logs/app.log。Unreal CrashReportClient 仅 Saved/Logs/CrashReportClient-backup-YYYY.MM.DD-HH.MM.SS.log，要求当前 CrashReportClient.log 及 Saved/Config/Windows 或 WindowsEditor/Engine.ini。文件修改时间与名称日期均须至少 30 天。当前日志、畸形/未来日期、未知格式、配置、场景、存档与模型不匹配；相关软件运行时跳过。

来源：[OBS 日志与崩溃报告生成](https://github.com/obsproject/obs-studio/blob/master/frontend/obs-main.cpp)、[Stability Matrix 固定版本归档格式](https://github.com/LykosAI/StabilityMatrix/blob/87dced77c1980e73c467dfac67009a94a9aa3d90/StabilityMatrix.Avalonia/Logging/LogFileTarget.cs)、[Epic 日志说明](https://dev.epicgames.com/documentation/unreal-engine/logging-in-unreal-engine)。UE 的具体备份文件名及 Engine.ini 位置另以本机元数据核对，不读取日志或个人配置正文。

v7 沿用 v6 更新安装副本和 v5 HF 策略；只加载 v7 外部文件，v1–v6 留给旧程序。新日志位置在标准快速复查与全盘发现后保存的路由中均可重读，位置从不授予删除资格。
