use std::sync::OnceLock;

use windows::ApplicationModel::Activation::ActivationKind;
use windows::ApplicationModel::{AppInstance, StartupTask, StartupTaskState};
use windows::Win32::Foundation::{APPMODEL_ERROR_NO_PACKAGE, ERROR_FILE_NOT_FOUND};
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR, w};

pub const BACKGROUND_FLAG: &str = "--background";

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("RustPlay");
// Must match the desktop:StartupTask TaskId in packaging/msix/AppxManifest.xml.
const TASK_ID: &str = "RustPlayStartup";

// True when running from the MSIX package (Microsoft Store), where the Run key is virtualised
// and start-up goes through a StartupTask declared in the manifest instead.
pub fn is_packaged() -> bool {
    static PACKAGED: OnceLock<bool> = OnceLock::new();
    *PACKAGED.get_or_init(|| {
        let mut len = 0u32;
        unsafe { GetCurrentPackageFullName(&mut len, None) != APPMODEL_ERROR_NO_PACKAGE }
    })
}

// True when the MSIX package was installed from the Microsoft Store (not sideloaded by hand).
pub fn is_store_install() -> bool {
    use windows::ApplicationModel::{Package, PackageSignatureKind};
    is_packaged() && Package::Current().and_then(|p| p.SignatureKind()).is_ok_and(|kind| kind == PackageSignatureKind::Store)
}

pub fn is_portable() -> bool {
    rustplay_core::identity::is_portable()
}

// Whether this launch comes from signing in to Windows, so the window should stay hidden.
pub fn launched_at_startup() -> bool {
    if std::env::args().any(|a| a == BACKGROUND_FLAG) {
        return true;
    }
    is_packaged()
        && AppInstance::GetActivatedEventArgs()
            .and_then(|args| args.Kind())
            .is_ok_and(|kind| kind == ActivationKind::StartupTask)
}

pub fn is_enabled() -> bool {
    if is_portable() {
        return false;
    }
    if is_packaged() {
        return off_thread(|| {
            let state = startup_task()?.State()?;
            Ok(matches!(state, StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy))
        })
        .unwrap_or(false);
    }
    let Some(key) = open(KEY_QUERY_VALUE) else { return false };
    let found = unsafe { RegQueryValueExW(key, VALUE, None, None, None, None).is_ok() };
    unsafe {
        let _ = RegCloseKey(key);
    }
    found
}

// Registers RustPlay to start hidden in the notification area when the user signs in.
pub fn set(enabled: bool) -> bool {
    // The portable build never writes to the registry.
    if is_portable() {
        return false;
    }
    let ok = if is_packaged() { set_task(enabled) } else { set_run_key(enabled) };
    if !ok {
        tracing::warn!("no se pudo cambiar el inicio automático con Windows");
    }
    ok
}

// Keeps the Run entry pointing at the current executable (the classic installer may move it).
pub fn refresh() {
    if !is_portable() && !is_packaged() && is_enabled() {
        set_run_key(true);
    }
}

fn startup_task() -> windows::core::Result<StartupTask> {
    StartupTask::GetAsync(&HSTRING::from(TASK_ID))?.join()
}

fn set_task(enabled: bool) -> bool {
    off_thread(move || {
        let task = startup_task()?;
        if enabled {
            let state = task.RequestEnableAsync()?.join()?;
            if state == StartupTaskState::DisabledByUser {
                tracing::warn!("el inicio automático está desactivado en el Administrador de tareas");
            }
            Ok(matches!(state, StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy))
        } else {
            task.Disable()?;
            Ok(true)
        }
    })
    .unwrap_or(false)
}

// WinRT async calls are waited on from a worker thread so the UI thread's apartment never blocks on itself.
fn off_thread<T: Send + 'static>(f: impl FnOnce() -> windows::core::Result<T> + Send + 'static) -> windows::core::Result<T> {
    std::thread::spawn(f)
        .join()
        .unwrap_or_else(|_| Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL)))
}

fn open(access: REG_SAM_FLAGS) -> Option<HKEY> {
    let mut key = HKEY::default();
    unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, access, &mut key).is_ok().then_some(key) }
}

fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" {BACKGROUND_FLAG}", exe.display()))
}

fn set_run_key(enabled: bool) -> bool {
    let Some(key) = open(KEY_SET_VALUE) else { return false };
    let ok = unsafe {
        if enabled {
            match command() {
                Some(command) => {
                    let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
                    let bytes = std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2);
                    RegSetValueExW(key, VALUE, None, REG_SZ, Some(bytes)).is_ok()
                }
                None => false,
            }
        } else {
            let result = RegDeleteValueW(key, VALUE);
            result.is_ok() || result == ERROR_FILE_NOT_FOUND
        }
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok
}
