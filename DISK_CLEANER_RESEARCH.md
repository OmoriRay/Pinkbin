# 全盘清理研究边界

本轮使用自有元数据遍历与规则分类，不移植 GPL 项目实现，不扫描文件内容，不实现重复文件清理。

- [BleachBit CleanerML](https://docs.bleachbit.org/cml/cleanerml/)：`deep` 适合散落的匹配文件，并合并同目录扫描；`walk.files` 适合已知缓存目录下的大多数文件。本轮一次全盘 walk 同时分类。
- [Czkawka 官方说明](https://github.com/qarmin/czkawka/blob/master/instructions/Instruction_Core.md)：重复检测先按大小分组，再首尾预哈希、完整哈希。仅作后续独立实现的原理参考。
- [Windows Installer](https://learn.microsoft.com/en-us/troubleshoot/windows-client/application-management/missing-windows-installer-cache) 与 [WinSxS](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/clean-up-the-winsxs-folder?view=windows-11)：必须剪枝，旧 `.msi` / `.msp` 也不能据年龄当作垃圾；组件清理由系统工具处理。
- [GetLogicalDrives](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getlogicaldrives) 配合 [GetDriveTypeW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getdrivetypew) 的 `DRIVE_FIXED`，枚举可访问的本地固定盘符；不要把逻辑盘符误当成所有无盘符卷。
- [FILE_ID_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info)：文件身份是卷序列号和文件 ID 的组合。文件、盘根和父目录均应检查身份，执行只接受本次扫描生成的候选 ID。
- [CreateFileW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew)：`FILE_FLAG_OPEN_REPARSE_POINT` 只保证最后一个分量；保留父链句柄并拒绝 `FILE_SHARE_DELETE` 可阻止目录替换，执行前后仍须复核。
- [GetFileInformationByHandle](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfileinformationbyhandle) 返回链接数与身份；多硬链接文件拒绝执行。[文件属性](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/ca28ec38-f155-4768-81d6-4bfeb8586fc9) 中 reparse、offline、recall 标志应在遍历目录和处理文件前检查，避免云下载。

普通目录中的旧临时文件、崩溃转储和日志依规则列候选；普通安装包和大文件属于用户审核项目，不能宣称为无用垃圾。全部默认不选；系统、软件安装和包管理目录先剪枝，但不把全部普通用户文件排除。扫描只报告实际检查数、目录、卷数和明确的截断状态；未预先完整计数不得伪造百分比。

执行需要共享互斥、15 分钟单次 token、取消状态协调、持久化 pending 日志、按文件复核和逐项结果；历史或日志写入失败必须真实报告，不把回收请求返回成功等同于已确认删除。

## Shell 回收凭据

- [PostDeleteItem](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-postdeleteitem) 的 `hrDelete` 是实际单项结果；非空 `psiNewlyCreated` 明确代表回收站内的新项目，永久删除则为空。原文件句柄在 Shell 采用复制后删除时可能无法再返回最终路径，不能用这一点否定已完成的回收。
- [PreDeleteItem](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-predeleteitem) 返回失败 HRESULT 会取消当前及后续操作；`S_FALSE` 不是失败。缺少回收模式时用 `E_ABORT` 拒绝。
- [TRANSFER_SOURCE_FLAGS](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/ne-shobjidl_core-_transfer_source_flags) 的 `TSF_DELETE_RECYCLE_IF_POSSIBLE` 仍是尽可能回收，必须结合实际单项凭据、操作未中止和原路径消失判断成功。
- [SIGDN_FILESYSPATH](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/ne-shobjidl_core-sigdn) 仅对具有文件系统路径的 Shell 项有效。虚拟回收站项不保证提供此路径；路径核实应为可选项，返回字符串用 `CoTaskMemFree` 释放。
- [微软 SDK 错误码](https://raw.githubusercontent.com/microsoft/win32metadata/main/generation/WinSDK/RecompiledIdlHeaders/um/sherrors.h) 中 `0x00270008` 是 `COPYENGINE_S_DONT_PROCESS_CHILDREN`，不是回收成功专用码。`USER_IGNORED`、`PENDING` 等正 HRESULT 不能仅因成功位而计为已回收。
