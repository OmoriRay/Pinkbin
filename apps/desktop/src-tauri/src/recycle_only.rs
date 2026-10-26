//! Shell recycling with an explicit per-item receipt, never a permanent-delete mode.
use std::path::Path;

#[cfg(windows)]
mod win {
    use std::{
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use windows::{
        core::{implement, Error, Ref, HRESULT, PCWSTR},
        Win32::{
            Foundation::RPC_E_CHANGED_MODE,
            System::Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
            },
            UI::Shell::{
                FileOperation, IFileOperation, IFileOperationProgressSink,
                IFileOperationProgressSink_Impl, IShellItem, SHCreateItemFromParsingName,
                COPYENGINE_S_DONT_PROCESS_CHILDREN, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE,
                FOF_NO_CONNECTED_ELEMENTS, FOF_NO_UI, FOF_WANTNUKEWARNING, SIGDN_FILESYSPATH,
                TSF_DELETE_RECYCLE_IF_POSSIBLE,
            },
        },
    };
    type Receipt = Arc<Mutex<Option<Result<(), String>>>>;
    fn abort() -> Error {
        Error::from_hresult(HRESULT(0x80004004u32 as i32))
    }
    fn confirmed_status(status: HRESULT) -> bool {
        status == HRESULT(0) || status == COPYENGINE_S_DONT_PROCESS_CHILDREN
    }
    fn recycle_path(path: &Path) -> bool {
        path.is_absolute()
            && path.components().nth(2).is_some_and(|c| {
                c.as_os_str()
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("$Recycle.Bin"))
            })
    }
    #[implement(IFileOperationProgressSink)]
    struct Sink {
        receipt: Receipt,
    }
    #[allow(non_snake_case)]
    impl IFileOperationProgressSink_Impl for Sink_Impl {
        fn StartOperations(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn FinishOperations(&self, result: HRESULT) -> windows::core::Result<()> {
            result.ok()
        }
        fn PreRenameItem(
            &self,
            _: u32,
            _: Ref<'_, IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostRenameItem(
            &self,
            _: u32,
            _: Ref<'_, IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<'_, IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreMoveItem(
            &self,
            _: u32,
            _: Ref<'_, IShellItem>,
            _: Ref<'_, IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostMoveItem(
            &self,
            _: u32,
            _: Ref<'_, IShellItem>,
            _: Ref<'_, IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<'_, IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreCopyItem(
            &self,
            _: u32,
            _: Ref<'_, IShellItem>,
            _: Ref<'_, IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostCopyItem(
            &self,
            _: u32,
            _: Ref<'_, IShellItem>,
            _: Ref<'_, IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<'_, IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreDeleteItem(&self, flags: u32, _: Ref<'_, IShellItem>) -> windows::core::Result<()> {
            if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
                return Err(abort());
            }
            Ok(())
        }
        fn PostDeleteItem(
            &self,
            _: u32,
            _: Ref<'_, IShellItem>,
            deleted: HRESULT,
            newly_created: Ref<'_, IShellItem>,
        ) -> windows::core::Result<()> {
            // The documented non-null newly-created item is the item in Recycle Bin;
            // permanent deletion supplies null. Validate the actual bin path as well.
            let outcome = (|| -> Result<(), String> {
                // NO_CONNECTED_ELEMENTS can produce DONT_PROCESS_CHILDREN for a
                // plain file. This is accepted only with a non-null recycle receipt.
                if !confirmed_status(deleted) {
                    return Err(format!("Shell 单项回收未返回已确认成功码：{deleted:?}"));
                }
                let item = newly_created.as_ref().ok_or("Shell 未提供回收站项目凭据")?;
                // Virtual Recycle Bin items need not expose a filesystem path.
                // The non-null PostDeleteItem result itself is the documented receipt.
                if let Ok(allocated) = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) } {
                    let path = unsafe {
                        PathBuf::from(std::ffi::OsString::from_wide(allocated.as_wide()))
                    };
                    unsafe {
                        CoTaskMemFree(Some(allocated.0 as _));
                    }
                    if !recycle_path(&path) {
                        return Err("Shell 返回项目不在卷根回收站中".into());
                    }
                }
                Ok(())
            })();
            let failed = outcome.is_err();
            if let Ok(mut receipt) = self.receipt.lock() {
                *receipt = Some(outcome);
            } else {
                return Err(abort());
            }
            if failed {
                Err(abort())
            } else {
                Ok(())
            }
        }
        fn PreNewItem(
            &self,
            _: u32,
            _: Ref<'_, IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostNewItem(
            &self,
            _: u32,
            _: Ref<'_, IShellItem>,
            _: &PCWSTR,
            _: &PCWSTR,
            _: u32,
            _: HRESULT,
            _: Ref<'_, IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResetTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn PauseTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResumeTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
    }
    pub(super) fn recycle(path: &Path) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !path.is_absolute() || !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("只允许回收已确认的普通文件".into());
        }
        let canonical = path.canonicalize().map_err(|e| e.to_string())?;
        let wide: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
        let shell_path = wide.strip_prefix(&[92, 92, 63, 92]).unwrap_or(&wide);
        unsafe {
            let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if initialized.is_err() && initialized != RPC_E_CHANGED_MODE {
                return Err(format!("无法初始化回收操作：{initialized:?}"));
            }
            struct Apartment(bool);
            impl Drop for Apartment {
                fn drop(&mut self) {
                    if self.0 {
                        unsafe {
                            CoUninitialize();
                        }
                    }
                }
            }
            let _apartment = Apartment(initialized.is_ok());
            let receipt: Receipt = Arc::new(Mutex::new(None));
            let sink: IFileOperationProgressSink = Sink {
                receipt: receipt.clone(),
            }
            .into();
            let perform = || -> windows::core::Result<()> {
                let operation: IFileOperation =
                    CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)?;
                operation.SetOperationFlags(
                    FOFX_RECYCLEONDELETE
                        | FOFX_EARLYFAILURE
                        | FOF_NO_UI
                        | FOF_NO_CONNECTED_ELEMENTS
                        | FOF_WANTNUKEWARNING,
                )?;
                let item: IShellItem =
                    SHCreateItemFromParsingName(PCWSTR(shell_path.as_ptr()), None)?;
                operation.DeleteItem(&item, &sink)?;
                operation.PerformOperations()?;
                if operation.GetAnyOperationsAborted()?.as_bool() {
                    return Err(abort());
                }
                Ok(())
            };
            let performed = perform();
            let outcome = receipt.lock().map_err(|_| "回收凭据无法读取")?.take();
            if let Some(Err(error)) = outcome {
                return Err(error);
            }
            performed.map_err(|e| format!("无法完成回收操作，未启用永久删除：{e}"))?;
            if outcome.is_none() {
                return Err("Shell 未返回单项回收凭据，未计为成功".into());
            }
            match std::fs::symlink_metadata(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                _ => Err("原路径仍存在或无法核实，未计为回收成功".into()),
            }
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use windows::Win32::UI::Shell::{
            COPYENGINE_S_PENDING, COPYENGINE_S_PENDING_DELETE, COPYENGINE_S_USER_IGNORED,
        };
        #[test]
        fn only_confirmed_status_codes_are_accepted() {
            assert!(confirmed_status(HRESULT(0)));
            assert!(confirmed_status(COPYENGINE_S_DONT_PROCESS_CHILDREN));
            for status in [
                COPYENGINE_S_USER_IGNORED,
                COPYENGINE_S_PENDING,
                COPYENGINE_S_PENDING_DELETE,
                HRESULT(1),
                HRESULT(-1),
            ] {
                assert!(!confirmed_status(status));
            }
        }
        #[test]
        fn permanent_mode_and_missing_receipt_are_rejected() {
            let receipt: Receipt = Arc::new(Mutex::new(None));
            let sink: IFileOperationProgressSink = Sink {
                receipt: receipt.clone(),
            }
            .into();
            unsafe {
                assert!(sink.PreDeleteItem(0, None::<&IShellItem>).is_err());
                assert!(sink.PreDeleteItem(128, None::<&IShellItem>).is_ok());
                assert!(sink
                    .PostDeleteItem(128, None::<&IShellItem>, HRESULT(0), None::<&IShellItem>)
                    .is_err());
            }
            assert!(receipt.lock().unwrap().as_ref().unwrap().is_err());
        }
        #[test]
        fn bin_path_must_be_directly_under_volume_root() {
            assert!(recycle_path(Path::new(r"C:\$Recycle.Bin\sid\$Rfile")));
            assert!(recycle_path(Path::new(r"\\?\C:\$Recycle.Bin\sid\$Rfile")));
            assert!(!recycle_path(Path::new(r"C:\Users\me\$Recycle.Bin\fake")));
        }
    }
}
#[cfg(windows)]
pub(crate) fn recycle(path: &Path) -> Result<(), String> {
    win::recycle(path)
}
#[cfg(not(windows))]
pub(crate) fn recycle(_path: &Path) -> Result<(), String> {
    Err("当前只支持 Windows 回收站".into())
}
