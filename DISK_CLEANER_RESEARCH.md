# 直接删除的 Windows 接口依据

当前规则清理以已核验的文件句柄执行删除，替代 v0.4.1 及之前按 Shell 路径回收的方案。

- [SetFileInformationByHandle](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle)：FileDispositionInfo 需要 DELETE 权限，设置删除标记后关闭文件句柄，文件才由系统删除。官方示例使用独占访问。
- [FILE_DISPOSITION_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_disposition_info)：DeleteFile=true 表示删除目标句柄对应的文件。
- [CreateFile 的访问与共享方式](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew)：目标独占打开；父目录保留禁止共享 DELETE 的句柄，阻止执行中替换父目录。
- [FILE_ID_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info)：全盘扫描使用卷序号与文件标识核对目标。

本软件只对普通文件执行该规则删除原语；不会通过传入目录递归删除，也不会通过回收站复制或隔离保留另一份文件。操作完成以关闭目标句柄后路径确实消失作为成功条件。

年龄、扩展名和文件大小只是本软件的检查策略，不是文件无用的证明。直接删除模式中，全盘项目仍须用户勾选；应用不自动清空系统回收站。

## v0.4.4 新规则依据

规则为本项目独立实现，没有复制第三方清理器代码或整套规则。目录事实来自下列官方文档，7/30 天阈值和推荐范围为 Pinkbin 的保守策略，不代表服务商认定文件已失效。

- [Python 编译缓存](https://docs.python.org/3/tutorial/modules.html#compiled-python-files)：__pycache__ 缓存模块字节码；本项目额外要求存在同名源码并等待至少 7 天，排除无源码分发。
- [pip 缓存](https://pip.pypa.io/en/stable/topics/caching/)：下载响应与 wheel 缓存及 PIP_CACHE_DIR；本项目限定 http/http-v2/wheels 子树和 30 天年龄。
- [npm 缓存](https://docs.npmjs.com/cli/v11/commands/npm-cache/)：_cacache 是下载缓存；本项目限定 content-v2/index-v5，不进入 node_modules。
- [Windows 临时目录](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-gettemppathw)：环境可配置且目录本身必须验证；本项目不接受磁盘根或用户目录作为泛化临时根。
- [Windows 用户态转储](https://learn.microsoft.com/en-us/windows/win32/wer/collecting-user-mode-dumps)：默认位于 LOCALAPPDATA/CrashDumps，属于排障数据。本项目只推荐指定位置至少 30 天的转储，明确提示旧记录将删除。
- [VS Code 便携布局](https://code.visualstudio.com/docs/setup/portable)：Windows ZIP 便携版通过邻近 data 目录保留用户数据；本项目只处理 data/user-data 中的明确缓存子树，同时要求程序与产品标记，不动 User、Backups、History 或工作区。
- [Electron 数据位置](https://www.electronjs.org/docs/latest/api/app#appgetpathname)：应用会话缓存与用户配置并非同一清理目标；本项目只匹配明确缓存子目录，不清空整个用户数据目录。

清理后缓存可能重建或重新下载，日志和转储不会恢复。本轮不纳入模型缓存、虚拟磁盘、安装介质、源码、依赖环境或未完成下载，也不清注册表。

## v0.4.5 扩展依据

以下文档用于确认目录用途，文件年龄、文件名白名单和一键推荐范围仍是 Pinkbin 自身策略：

- [Microsoft：WER 报告位置](https://learn.microsoft.com/en-us/windows-server/failover-clustering/troubleshooting-using-wer-reports)：ReportArchive 与 ReportQueue 用途不同，本轮只取至少 30 天的已归档标准报告，不清队列。
- [Microsoft：内核实时转储](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/kernel-live-dump-code-reference)：LiveKernelReports 属于诊断转储。结合既有 MEMORY.DMP 识别，只取旧诊断文件，不触及系统组件。
- [NuGet：缓存与全局包目录](https://learn.microsoft.com/en-us/nuget/consume-packages/managing-the-global-packages-and-cache-folders)：HTTP 缓存与直接用于项目的 global-packages 不同，本轮只取旧下载响应。
- [Cargo：构建缓存](https://doc.rust-lang.org/cargo/reference/build-cache.html)：incremental 用于后续编译加速；本轮要求 Cargo.toml，只取指定 incremental 子树，不删除最终构建产物。内部布局可能演变，未知布局不匹配。
- [Epic：清理启动器缓存](https://www.epicgames.com/help/en-US/c-Category_EpicAccounts/c-TechnicalSupport_GeneralSupport/a000086158)：webcache 与带版本后缀的 webcache 位于 Saved，本轮进一步限定网页/代码/GPU 缓存，不清整个目录。
- [JetBrains：IDE 目录](https://www.jetbrains.com/help/idea/directories-used-by-the-ide-to-store-settings-caches-plugins-and-logs.html)、[Local History](https://www.jetbrains.com/help/idea/local-history.html)：系统目录同时含缓存与本地历史，因此只取指定缓存/索引，不删整个系统目录。
- [Vivaldi：Windows 数据位置](https://help.vivaldi.com/desktop/privacy/preventing-vivaldi-profiles-from-being-uploaded-to-git-repositories/)、[独立安装](https://help.vivaldi.com/desktop/install-update/standalone-version-of-vivaldi/)：程序与数据的邻近布局可被核验；本轮仅清浏览器缓存子树，保留账号和配置。

这些规则不通过扩展名或文件大小单独判定垃圾，不导入第三方可执行清理脚本。实际删除仍需要身份、父链、占用与持久化日志检查。

Chrome/Edge/Brave 的同目录 Application/User Data 识别是本项目基于已有 Chromium 缓存结构增加的布局规则，并非对所有便携包的支持声明；程序或 Local State 标记缺失时不会推荐。


## v0.5.0 规则库与新增应用

采用独立整理的 MIT JSON 元数据规则，没有复制 Winapp2 数据、图吧 GPL 代码或其注册表/卸载定义。应用目录存在性和可重建缓存模板结合既有全盘发现；排除项与进程保护独立维护。规则来源 URL 仅供维护人员核对，运行时不访问网络。

- [Obsidian 数据存储](https://obsidian.md/help/data-storage)：Windows 系统数据位于 %APPDATA%/Obsidian；仅纳入 Chromium renderer 缓存子树，vault、IndexedDB、Local Storage 与备份不选。
- [Notion 官方重置说明](https://www.notion.com/en-gb/help/reset-notion)：Windows 应用数据位于 Roaming/Notion。官方重置可删除整个目录，但 Pinkbin 仅选择可重建界面缓存，不删除离线数据或整个应用目录。

目录资料不等于全目录可删除证明；清理范围为本项目受缓存模板约束的子集。保留天数为本项目策略。


## v0.6.0 本机元数据核查与规则依据

只读取本机应用目录和文件元数据，在 Local/Roaming、D:/AI 和 D:/PC 常用软件目录的有限深度内统计 294 个缓存形态目录。Cinebench 旧渲染缓存约 4.65 GB（逻辑大小），aDrive、123pan、百度网盘、WebView 和 Electron 分区存在旧规则未覆盖的缓存。此数量不是新版可删除体积；应用占用、保留天数、文件身份与候选预算仍影响最终结果。没有实际删除，也没有进行新版功能测试。详细本机路径证据留在工作区 qa/v060，不打包或上传。

- [Electron Session API](https://www.electronjs.org/docs/latest/api/session)：持久分区有独立 HTTP 缓存，与账户/DOM 存储分别处理；实现只扩大已知应用的固定缓存叶目录。
- [WebView2 用户数据](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/user-data-folder)：用户数据中同时含 Cookie、权限与磁盘缓存；只取已知应用根下可重建缓存，保留整个用户数据根。
- [Maxon Redshift 缓存设置](https://help.maxon.net/c4d/s26/en-us/Content/_REDSHIFT_/html/Redshift%2BPreferences.html)与[纹理缓存说明](https://support.maxon.net/hc/zh-cn/articles/25006553155484)：渲染器生成缓存加速重复渲染。结合本机 Cinebench 专用目录与 .rstexbin 结构，规则限定旧基准渲染缓存，不清普通工作项目。
- [Cherry Studio 存储位置](https://docs.cherry-studio-ai.com/pre-basic/personalization-settings/storage)：支持修改用户数据位置；新增常见便携目录识别，同时必须存在对应程序及产品标记。
- [VS Code 便携目录](https://github.com/microsoft/vscode-docs/blob/main/docs/setup/portable.md)：保留既有程序和产品标记校验。


## v0.7.0 本机漏项补充

独立扫描发现 MusicFree portable/userData 的 renderer 缓存、QQ 标准分区缓存、WeGame qbcore 数字版本的 CEF 缓存以及 Cargo 已有源码对应的旧下载包。正式实现仅采用限定路径/类型并补反例，不直接导入图吧/Winapp2 整库。Wallpaper Engine 的 projects、损坏备份和资源不作为界面缓存删除。

- [MusicFree 主程序](https://github.com/maotoumao/MusicFreeDesktop/blob/master/src/main/index.ts) 明确 Windows portable 文件夹下重设 appData/userData；本机程序、resources/app/package.json 与 Local State 标记已经核对，仅将 portable/userData 的固定缓存叶纳入。
- [Cargo Home 官方说明](https://doc.rust-lang.org/cargo/guide/cargo-home.html)区分 registry/cache 下载包与 registry/src 解压源码，允许重新获取。实现额外要求至少 30 天、对应源码标记和 Cargo/Rust 进程保护；未来离线重建仍可能需要下载包。
- [Electron session](https://www.electronjs.org/docs/latest/api/session)与[CEF 存储说明](https://bitbucket.org/chromiumembedded/cef/wiki/GeneralUsage)作为缓存语义依据；QQ/WeGame 的目录结构由本机只读元数据核查确认，执行范围为标准缓存叶，不清 Local Storage、IndexedDB 或用户资源。

统计空间与授予删除权限分别实现；广扫描不使系统目录、开发环境和回收站成为可清理列表。


## v0.8.0：非缓存类型核验（2026-10-27）

本机只读元数据核查发现 Cherry Studio/夸克旧 Crashpad 转储、QQ app_launcher-日期.log、Cherry app-error.日期.log、以及大量与数据库关联的 000003.log/LOG.old。前者限定明确结构后支持推荐，后者保留。当前 main.log 即使很旧也不作为轮转日志。Chrome/Edge 等进程保护沿用；诊断文件与缓存分别命名。

Electron 官方声明 crashDumps 目录内布局是实现细节，因此只承认本机核验过的 reports/UUID.dmp 与状态标记，不对未知布局递归删除。electron-log 明确 main.old.log 的轮转方式。Inno Setup 官方说明 {tmp} 为启动时创建的 is-*.tmp 暂存；NSIS 说明 PLUGINSDIR 的 ns*.tmp 结构。新解压文件可能沿用包内旧时间，故需要目录创建与修改时间同时满足保留期。

Microsoft 将 CbsPersist 文件描述为归档日志，DISM 文档描述 .bak 轮转；本项目仅匹配这些旧诊断文件，不删除组件库、Installer 修复缓存、更新包或报告队列。维护进程保护与只读路由在执行前再次验证。Windows Installer 缓存不可视作普通安装残留清空，依据：https://learn.microsoft.com/en-us/troubleshoot/windows-client/application-management/missing-windows-installer-cache 。

官方格式来源与链接集中在 rules/README.md；本机元数据与最终验证只留 qa/v080。
