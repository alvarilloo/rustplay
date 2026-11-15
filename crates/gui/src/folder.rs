// Folder picker and "show in Explorer" helpers for captures.
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

// Asks for a folder, starting at `initial`; None when the dialog is cancelled.
pub fn pick(owner: HWND, title: &str, initial: &Path) -> Option<PathBuf> {
    // SAFETY: objetos COM propios usados en el hilo de la interfaz; la cadena devuelta se libera con CoTaskMemFree.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dialog.SetTitle(&HSTRING::from(title));
        if let Some(existing) = std::iter::successors(Some(initial), |p| p.parent()).find(|p| p.is_dir())
            && let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(existing.as_os_str()), None)
        {
            let _ = dialog.SetFolder(&item);
        }
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0 as *const _));
        path
    }
}

// Opens Explorer on `folder`, creating it first if needed.
pub fn open(folder: &Path) {
    let _ = std::fs::create_dir_all(folder);
    let wide = HSTRING::from(folder.as_os_str());
    // SAFETY: cadena terminada en nul válida durante la llamada.
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

// Opens Explorer with `file` selected (or its folder when the file is gone).
pub fn reveal(file: &Path) {
    if !file.exists() {
        if let Some(dir) = file.parent() {
            open(dir);
        }
        return;
    }
    let args = HSTRING::from(format!("/select,\"{}\"", file.display()));
    // SAFETY: cadenas terminadas en nul válidas durante la llamada.
    unsafe {
        ShellExecuteW(None, w!("open"), w!("explorer.exe"), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL);
    }
}

// Opens a file with its default app (nothing happens when it is gone).
pub fn open_file(file: &Path) {
    if !file.is_file() {
        return;
    }
    let wide = HSTRING::from(file.as_os_str());
    // SAFETY: cadena terminada en nul válida durante la llamada.
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}
