use std::mem::{size_of, size_of_val};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenIntegrityLevel,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoTaskMemFree, IDispatch,
    IServiceProvider,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, FOLDERID_Desktop, GPFIDL_DEFAULT, IFolderView, IPersistFolder2, IShellBrowser,
    IShellView, IShellWindows, IWebBrowserApp, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    SHGetPathFromIDListEx, SID_STopLevelBrowser, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PWSTR, Result};

use crate::policy::ShellView;
use crate::uia::{Apartment, int_variant};

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Debug, Default)]
pub struct Process {
    pub exe: Option<String>,
    pub integrity: Option<u32>,
    pub errors: Vec<String>,
}

pub fn process(pid: u32) -> Process {
    let mut info = Process::default();
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => Handle(handle),
        Err(err) => {
            info.errors
                .push(format!("OpenProcess: {}", crate::error(&err)));
            return info;
        }
    };
    let mut path = [0u16; 32768];
    let mut len = path.len() as u32;
    match unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )
    } {
        Ok(()) => {
            info.exe = String::from_utf16_lossy(&path[..len as usize])
                .rsplit('\\')
                .next()
                .map(str::to_owned);
        }
        Err(err) => info
            .errors
            .push(format!("exe lookup: {}", crate::error(&err))),
    }
    match integrity(handle.0) {
        Ok(level) => info.integrity = Some(level),
        Err(err) => info
            .errors
            .push(format!("target integrity: {}", crate::error(&err))),
    }
    info
}

pub fn own_integrity() -> Result<u32> {
    integrity(unsafe { GetCurrentProcess() })
}

fn integrity(process: HANDLE) -> Result<u32> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token)?;
        let token = Handle(token);
        // The returned label contains a pointer to a SID stored in this aligned buffer.
        let mut buffer = [0usize; 16];
        let mut size = 0;
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            size_of_val(&buffer) as u32,
            &mut size,
        )?;
        debug_assert!(size as usize >= size_of::<TOKEN_MANDATORY_LABEL>());
        let label = &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sid = label.Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        Ok(*GetSidSubAuthority(sid, u32::from(count - 1)))
    }
}

pub fn window_at(point: POINT) -> (HWND, HWND, u32) {
    unsafe {
        let child = WindowFromPoint(point);
        let root = GetAncestor(child, GA_ROOT);
        let mut pid = 0;
        GetWindowThreadProcessId(root, Some(&mut pid));
        (child, root, pid)
    }
}

pub fn shell_kind(root: HWND) -> Option<ShellView> {
    let mut name = [0u16; 64];
    let len = unsafe { GetClassNameW(root, &mut name) } as usize;
    match String::from_utf16_lossy(&name[..len]).as_str() {
        "CabinetWClass" | "ExploreWClass" => Some(ShellView::Folder),
        "Progman" | "WorkerW" => Some(ShellView::Desktop),
        _ => None,
    }
}

/// None means that the click was in Explorer chrome or another view outside the active folder.
pub fn shell_path(
    kind: ShellView,
    child: HWND,
    root: HWND,
) -> Result<Option<std::result::Result<PathBuf, String>>> {
    let _apartment = Apartment::init(COINIT_APARTMENTTHREADED)?;
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
        let view = match kind {
            ShellView::Desktop => {
                let mut hwnd = 0;
                let dispatch = windows.FindWindowSW(
                    &int_variant(CSIDL_DESKTOP as i32),
                    &VARIANT::default(),
                    SWC_DESKTOP,
                    &mut hwnd,
                    SWFO_NEEDDISPATCH,
                )?;
                view_at(&dispatch, child)?
            }
            ShellView::Folder => {
                let mut found = None;
                for index in 0..windows.Count()? {
                    let Ok(dispatch) = windows.Item(&int_variant(index)) else {
                        continue;
                    };
                    let Ok(browser) = dispatch.cast::<IWebBrowserApp>() else {
                        continue;
                    };
                    if browser
                        .HWND()
                        .ok()
                        .is_some_and(|h| h.0 as usize == root.0 as usize)
                        && let Some(view) = view_at(&dispatch, child)?
                    {
                        found = Some(view);
                        break;
                    }
                }
                found
            }
        };
        let Some(view) = view else { return Ok(None) };
        let path = view
            .cast()
            .and_then(|view| folder_path(&view))
            .map_err(|err| crate::error(&err));
        if path.is_err() && matches!(kind, ShellView::Desktop) {
            let known = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None)?;
            let desktop = PathBuf::from(std::ffi::OsString::from_wide(known.as_wide()));
            CoTaskMemFree(Some(known.0.cast()));
            return Ok(Some(Ok(desktop)));
        }
        Ok(Some(path))
    }
}

fn view_at(dispatch: &IDispatch, child: HWND) -> Result<Option<IShellView>> {
    unsafe {
        let services: IServiceProvider = dispatch.cast()?;
        let browser: IShellBrowser = services.QueryService(&SID_STopLevelBrowser)?;
        let view = browser.QueryActiveShellView()?;
        let hwnd = view.GetWindow()?;
        Ok((hwnd == child || IsChild(hwnd, child).as_bool()).then_some(view))
    }
}

fn folder_path(view: &IFolderView) -> Result<PathBuf> {
    unsafe {
        let folder: IPersistFolder2 = view.GetFolder()?;
        let pidl = folder.GetCurFolder()?;
        let mut path = [0u16; 32768];
        let success = SHGetPathFromIDListEx(pidl, &mut path, GPFIDL_DEFAULT).as_bool();
        CoTaskMemFree(Some(pidl.cast()));
        if !success {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "shell folder has no filesystem path",
            ));
        }
        let len = path.iter().position(|&c| c == 0).unwrap_or(path.len());
        Ok(std::ffi::OsString::from_wide(&path[..len]).into())
    }
}
