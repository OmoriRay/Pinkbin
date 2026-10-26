# 直接删除的 Windows 接口依据

当前规则清理以已核验的文件句柄执行删除，替代 v0.4.1 及之前按 Shell 路径回收的方案。

- [SetFileInformationByHandle](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle)：FileDispositionInfo 需要 DELETE 权限，设置删除标记后关闭文件句柄，文件才由系统删除。官方示例使用独占访问。
- [FILE_DISPOSITION_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_disposition_info)：DeleteFile=true 表示删除目标句柄对应的文件。
- [CreateFile 的访问与共享方式](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew)：目标独占打开；父目录保留禁止共享 DELETE 的句柄，阻止执行中替换父目录。
- [FILE_ID_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info)：全盘扫描使用卷序号与文件标识核对目标。

本软件只对普通文件执行该规则删除原语；不会通过传入目录递归删除，也不会通过回收站复制或隔离保留另一份文件。操作完成以关闭目标句柄后路径确实消失作为成功条件。

年龄、扩展名和文件大小只是本软件的检查策略，不是文件无用的证明。直接删除模式中，全盘项目仍须用户勾选；应用不自动清空系统回收站。
