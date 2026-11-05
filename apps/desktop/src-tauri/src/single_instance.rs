//! A lifetime lease scoped to the executable directory, before portable data or WebView opens.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, BOOL, ERROR_NO_MORE_FILES, FILETIME, HWND, INVALID_HANDLE_VALUE,
        LPARAM,
    },
    Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::WindowsAndMessaging::{
        EnumWindows, FlashWindow, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, SetForegroundWindow, ShowWindowAsync, SW_RESTORE,
    },
};

const FILE_NAME: &str = ".pinkbin-instance-v1.json";
pub(crate) struct Lease {
    _file: File,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    pid: u32,
    created: u64,
    version: String,
}

fn process_created(pid: u32) -> Option<u64> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut times = [std::mem::zeroed::<FILETIME>(); 4];
        let pointer = times.as_mut_ptr();
        let ok = GetProcessTimes(
            handle,
            pointer,
            pointer.add(1),
            pointer.add(2),
            pointer.add(3),
        );
        CloseHandle(handle);
        (ok != 0)
            .then(|| (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
    }
}
fn ordinary(file: &File) -> std::io::Result<()> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if info.nNumberOfLinks != 1
        || !file.metadata()?.is_file()
        || !crate::rule_probe::ordinary(&file.metadata()?)
    {
        return Err(std::io::Error::other("启动记录不是普通独立文件"));
    }
    Ok(())
}
fn open_lease(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(1)
        .attributes(0x2)
        .custom_flags(0x00200000)
        .open(path)?;
    ordinary(&file)?;
    Ok(file)
}
fn write_owner(file: &mut File) -> Result<(), String> {
    let owner = Owner {
        pid: std::process::id(),
        created: process_created(std::process::id()).ok_or("无法确认当前启动进程")?,
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let bytes = serde_json::to_vec(&owner).map_err(|_| "无法准备启动记录")?;
    file.set_len(0)
        .and_then(|_| file.write_all(&bytes))
        .and_then(|_| file.sync_all())
        .map_err(|_| "无法保存启动记录".to_string())
}
fn read_owner(path: &Path) -> Option<Owner> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(7)
        .custom_flags(0x00200000)
        .open(path)
        .ok()?;
    ordinary(&file).ok()?;
    if file.metadata().ok()?.len() > 512 {
        return None;
    }
    let mut bytes = vec![];
    file.take(513).read_to_end(&mut bytes).ok()?;
    let owner: Owner = serde_json::from_slice(&bytes).ok()?;
    (owner.version.len() <= 48 && process_created(owner.pid) == Some(owner.created))
        .then_some(owner)
}
struct WindowSearch {
    pid: u32,
    window: HWND,
}
unsafe extern "system" fn find_window(window: HWND, value: LPARAM) -> BOOL {
    let search = &mut *(value as *mut WindowSearch);
    let mut pid = 0;
    GetWindowThreadProcessId(window, &mut pid);
    if pid == search.pid && IsWindowVisible(window) != 0 {
        let mut title = [0u16; 128];
        let length = GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32);
        if length > 0 && String::from_utf16_lossy(&title[..length as usize]) == "Pinkbin" {
            search.window = window;
            return 0;
        }
    }
    1
}
fn activate(pid: u32) -> bool {
    let mut search = WindowSearch {
        pid,
        window: std::ptr::null_mut(),
    };
    unsafe {
        EnumWindows(
            Some(find_window),
            &mut search as *mut WindowSearch as LPARAM,
        );
        if search.window.is_null() {
            return false;
        }
        if IsIconic(search.window) != 0 {
            ShowWindowAsync(search.window, SW_RESTORE);
        }
        if SetForegroundWindow(search.window) == 0 {
            FlashWindow(search.window, 1);
        }
    }
    true
}
fn same_folder(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(a), Ok(b)) => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
        _ => false,
    }
}
fn supports_guard(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut());
        if size == 0 || size > 1024 * 1024 {
            return false;
        }
        let mut bytes = vec![0u8; size as usize];
        if GetFileVersionInfoW(path.as_ptr(), 0, size, bytes.as_mut_ptr().cast()) == 0 {
            return false;
        }
        let mut pointer = std::ptr::null_mut();
        let mut length = 0;
        if VerQueryValueW(
            bytes.as_ptr().cast(),
            [92u16, 0].as_ptr(),
            &mut pointer,
            &mut length,
        ) == 0
            || pointer.is_null()
            || length < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32
        {
            return false;
        }
        let version = std::ptr::read_unaligned(pointer.cast::<VS_FIXEDFILEINFO>());
        version.dwSignature == 0xfeef04bd
            && (
                version.dwProductVersionMS >> 16,
                version.dwProductVersionMS & 0xffff,
                version.dwProductVersionLS >> 16,
            ) >= (0, 11, 7)
    }
}
fn legacy_running(parent: &Path) -> Result<bool, String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err("无法检查已有 Pinkbin 进程".into());
        }
        struct Snapshot(windows_sys::Win32::Foundation::HANDLE);
        impl Drop for Snapshot {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let _snapshot = Snapshot(snapshot);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) == 0 {
            return Err("无法读取已有 Pinkbin 进程".into());
        }
        loop {
            let length = entry
                .szExeFile
                .iter()
                .position(|v| *v == 0)
                .unwrap_or(entry.szExeFile.len());
            if entry.th32ProcessID != std::process::id()
                && crate::portable_data::pinkbin_executable_name(&String::from_utf16_lossy(
                    &entry.szExeFile[..length],
                ))
            {
                let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
                if !handle.is_null() {
                    let mut path = [0u16; 32768];
                    let mut length = path.len() as u32;
                    let ok = QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length);
                    CloseHandle(handle);
                    if ok != 0 {
                        let image = std::path::PathBuf::from(String::from_utf16_lossy(
                            &path[..length as usize],
                        ));
                        if image.parent().is_some_and(|p| same_folder(parent, p))
                            && !supports_guard(&image)
                        {
                            return Ok(true);
                        }
                    }
                }
            }
            if Process32NextW(snapshot, &mut entry) == 0 {
                return if GetLastError() == ERROR_NO_MORE_FILES {
                    Ok(false)
                } else {
                    Err("已有进程检查未完成".into())
                };
            }
        }
    }
}
pub(crate) fn claim() -> Result<Option<Lease>, String> {
    let exe = std::env::current_exe().map_err(|_| "无法确定程序目录")?;
    let parent = exe.parent().ok_or("无法确定程序目录")?;
    let path = parent.join(FILE_NAME);
    let started = Instant::now();
    loop {
        match open_lease(&path) {
            Ok(mut file) => {
                if legacy_running(parent)? {
                    return Err("旧版 Pinkbin 仍在运行，请先退出旧版再打开新版。".into());
                }
                write_owner(&mut file)?;
                return Ok(Some(Lease { _file: file }));
            }
            Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => {
                if let Some(owner) = read_owner(&path) {
                    if owner.version != env!("CARGO_PKG_VERSION") {
                        let _ = activate(owner.pid);
                        return Err(format!(
                            "Pinkbin v{} 仍在运行，请先退出它再打开新版。",
                            owner.version
                        ));
                    }
                    if activate(owner.pid) {
                        return Ok(None);
                    }
                }
                if started.elapsed() >= Duration::from_secs(8) {
                    return Err("Pinkbin 正在启动或退出，请稍后再试。".into());
                }
                std::thread::sleep(Duration::from_millis(80));
            }
            Err(error) => return Err(format!("无法检查启动状态：{error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lease_is_exclusive_readable_and_reusable_after_exit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FILE_NAME);
        let mut first = open_lease(&path).unwrap();
        write_owner(&mut first).unwrap();
        assert_eq!(read_owner(&path).unwrap().pid, std::process::id());
        assert!(open_lease(&path).is_err());
        assert!(fs::rename(&path, temp.path().join("moved")).is_err());
        drop(first);
        let mut next = open_lease(&path).unwrap();
        write_owner(&mut next).unwrap();
        assert!(read_owner(&path).is_some());
        let other = temp.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(open_lease(&other.join(FILE_NAME)).is_ok());
    }
    #[test]
    fn malformed_stale_and_linked_records_do_not_authorize_or_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FILE_NAME);
        fs::write(&path, vec![b'x'; 513]).unwrap();
        assert!(read_owner(&path).is_none());
        let mut record = open_lease(&path).unwrap();
        write_owner(&mut record).unwrap();
        drop(record);
        let mut owner = read_owner(&path).unwrap();
        owner.created = owner.created.saturating_add(1);
        fs::write(&path, serde_json::to_vec(&owner).unwrap()).unwrap();
        assert!(read_owner(&path).is_none());
        let linked = temp.path().join("linked");
        fs::hard_link(&path, &linked).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(open_lease(&linked).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}
