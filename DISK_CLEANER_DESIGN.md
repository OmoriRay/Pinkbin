# Pinkbin 0.3.0 全盘扫描边界

扫描入口只接受后端枚举出的本地 fixed 卷盘符，不接受任意前端路径。每个所选盘从根目录开始一次流式深度优先遍历，一次元数据分类；不构建完整文件树，不读取文件内容，不要求管理员权限。权限不足、系统目录及软件环境的跳过数量会显示，不宣称覆盖受保护目录。

## 分类与选择

- `temp`：至少 7 天未修改的 `.tmp`、`.temp`、`.part`、`.crdownload`，仅人工复核。
- `crash`：至少 30 天未修改的 `.dmp`、`.mdmp`。
- `logs`：至少 30 天未修改的 `.log`、`.old`；`.bak` 不作为旧日志候选。
- `installers`：至少 30 天未修改的 `.msi`、`.msix`、`.appx`、`.iso`、`.cab`；不将任意 `.exe` 认作安装包。
- `large`：至少 500 MiB 的其他普通文件，明确说明大小不代表垃圾。

每个文件只归入最先匹配的类别；所有条目均为 `review`、`default_selected=false`。前端必须逐文件选择，不能凭类别默认删除。每类有用途与删除后果说明。

## 限额、计数与取消

总候选最多 20,000，每类最多 5,000；另设 64 MiB 保守候选内存预算。达到候选限额后继续遍历并累计真实文件、字节、目录数量，已保存的候选仍可执行，`limited=true` 提示结果只展示部分。目录用共享 `Arc` 父链，候选不会复制每一级完整父链。深度 256 或扫描 15 分钟为遍历保底限制，会明确提示部分扫描，不冒充全部扫描完成。

每个目录与条目边界检查取消，约每 150 ms 发布累计进度并在每盘结束发布一次。最多保留 32 个、15 分钟有效的预取消编号，关闭扫描窗口与扫描注册同时发生时也能取消。取消后清空条目和计划编号，绝不发布可执行计划。系统调用本身的等待不能由应用立即打断。

## 保护与执行

系统、回收站、Recovery、Windows/Installer/WinSxS/DriverStore、Program Files/ProgramData 及 `.git`、`node_modules`、虚拟环境等目录按名称剪枝。分页、休眠、交换、启动和用户注册表文件明确排除。重解析点、符号链接、多硬链接、offline/recall 云占位不保存为候选。跨卷挂载目录跳过。仅候选文件才打开只读取属性的身份句柄；普通扫描文件仅检查元数据。

计划保存在内存中，以扫描结束时刻计 15 分钟有效；重扫使旧计划失效，执行单次消费，拒绝无效/重复文件编号。缓存与全盘的扫描、执行共用进程级原子锁与 RAII 释放。每次执行最多 10,000 个文件、60 秒；其余显示跳过。

Windows 身份使用 `FILE_ID_INFO` 的 64 位卷序号与 128 位文件 ID，兼顾 NTFS 与 ReFS；文件同时保存大小、修改与创建时间，目录保存创建时间和身份。执行再次核对 fixed 卷列表，每个候选重新核对卷根到父目录链、文件身份与分类条件。父目录句柄持有 `FILE_LIST_DIRECTORY` 访问且不共享 DELETE，阻止执行期间目录替换；目标句柄持有 READ_DATA/ATTRIBUTES 访问且不共享 WRITE，阻止并发写入，不实际读取文件内容。保留句柄自身身份也与快照核对。

沿用 `cleanup-history-v1` 的隐私摘要格式：开始记录持久化失败时不执行任何动作；每个文件的 `pending` 动作日志同步落盘后再次核验，再调用仅回收的 Shell helper。Windows 使用 `FOFX_RECYCLEONDELETE`，不设置永久删除回退。`PreDeleteItem` 缺少回收标志即以 `E_ABORT` 拒绝；`PostDeleteItem` 必须返回 `S_OK` 或本机普通文件实测的 `COPYENGINE_S_DONT_PROCESS_CHILDREN`，同时提供非空的新回收站项目凭据，并且原路径必须消失，才计成功。若新项提供物理路径，再核对它位于卷根 `$Recycle.Bin`；虚拟回收站项没有物理路径不推翻其官方回调凭据。ignored/pending 等其他正 HRESULT 不计成功。结束记录失败不抹去真实成功统计，返回 `history_warning`。文件失败逐项处理；日志失败停止后续动作，返回至多 8 条单项回收错误。

调试夹具中，回收已由系统完成且能按自建文件原路径找到回收站条目，但原句柄的 `GetFinalPathNameByHandleW` 和 `FileNameInfo` 均返回 1168。原因未确定，不能将此现象断言为 Shell 一定复制再删除或 API 缺陷。最终实现使用微软文档明确保证的单项 `PostDeleteItem` 新项目作为回收证据，不依赖原句柄在回收后仍有可查询名称，也不为每个候选遍历用户回收站。

Shell 的删除接口最终仍以路径调用，目标必须允许 DELETE 以便移动入回收站。因此最终身份核验与 Shell 接收路径之间仍存在极短的目标重命名竞态，不能宣称拥有完全原子的按句柄删除语义；目录替换与内容改写已由持有句柄限制。此版本不引入中间隔离改名、永久删除或恢复管线。

## 验证范围

单元与集成夹具覆盖多卷根同生产扫描引擎、分类边界、全未选、保护目录、junction/硬链接/offline、候选与内存限额后继续计数、取消与启动竞态、过期/损坏/重复/重放 token、文件/父链变更、持句柄分享保护、pending 和历史失败，以及逐项回收失败。真实 Windows 回收测试只使用 `tempfile` 创建并由本测试持有的临时文件，覆盖中文、空格、非 BMP 字符及超过 260 字符路径，核对源消失、pending/success 日志和历史统计；普通 Unicode 文件还匹配精确回收站原路径并恢复。长路径测试允许正常报告失败，不允许凭 Shell 排队成功虚报回收；仅在回收站列表能精确匹配自建长路径时恢复。旧缓存自建夹具也通过新的相同仅回收 helper 验证。真实磁盘只运行只读盘列表枚举，不跑真实全盘扫描或清理用户数据。

## 官方接口依据

- [PostDeleteItem 单项结果与回收站新项目凭据](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-postdeleteitem)
- [PreDeleteItem 拒绝操作](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-predeleteitem)
- [TRANSFER_SOURCE_FLAGS](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/ne-shobjidl_core-_transfer_source_flags)
- [SIGDN：虚拟 Shell 项不保证文件系统路径](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/ne-shobjidl_core-sigdn)
- [微软 Windows SDK 回调结果常量](https://raw.githubusercontent.com/microsoft/win32metadata/main/generation/WinSDK/RecompiledIdlHeaders/um/sherrors.h)
- [BY_HANDLE_FILE_INFORMATION：ReFS 需要 128 位文件 ID](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/ns-fileapi-by_handle_file_information)
