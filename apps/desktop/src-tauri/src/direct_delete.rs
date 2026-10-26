//! Permanently delete the already validated, exclusively held ordinary file.
//! No Shell operation, Recycle Bin, directory recursion, or path-based reopen.
use std::{fs::File, path::Path};

pub(crate) fn open(path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .access_mode(0x10000 | 0x80 | 1)
            .share_mode(0)
            .custom_flags(0x00200000)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

pub(crate) fn hold_parents(path: &Path) -> std::io::Result<Vec<File>> {
    let mut handles = Vec::new();
    for parent in path
        .parent()
        .ok_or_else(|| std::io::Error::other("缺少父目录"))?
        .ancestors()
    {
        let meta = std::fs::symlink_metadata(parent)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(std::io::Error::other("父目录包含链接或已改变"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            if meta.file_attributes() & 0x400 != 0 {
                return Err(std::io::Error::other("父目录包含重解析点"));
            }
            handles.push(
                std::fs::OpenOptions::new()
                    .access_mode(0x80 | 1)
                    .share_mode(3)
                    .custom_flags(0x02000000 | 0x00200000)
                    .open(parent)?,
            );
        }
        #[cfg(not(windows))]
        handles.push(File::open(parent)?);
    }
    Ok(handles)
}

#[cfg(windows)]
pub(crate) fn delete(_path: &Path, file: &File) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("只允许直接删除已核验的普通文件".into());
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as _,
            FileDispositionInfo,
            &disposition as *const _ as _,
            std::mem::size_of_val(&disposition) as u32,
        )
    };
    if ok == 0 {
        return Err(format!(
            "文件未能直接删除：{}",
            std::io::Error::last_os_error()
        ));
    }
    // The caller drops this exclusive handle before confirming the path is gone.
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn delete(path: &Path, file: &File) -> Result<(), String> {
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("只允许直接删除已核验的普通文件".into());
    }
    std::fs::remove_file(path).map_err(|e| e.to_string())
}
