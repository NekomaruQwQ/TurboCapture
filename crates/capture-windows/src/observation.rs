//! Windows enumeration translated into the platform-neutral selector boundary.

use std::{
    ffi::OsString,
    mem::size_of,
    os::windows::ffi::OsStringExt as _,
    path::PathBuf,
    process,
};

use capture_core::{ObservationId, ObservedWindow, WindowBounds};
use windows::Win32::{
    Foundation::{CloseHandle, HWND, LPARAM, RECT, TRUE},
    Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
    System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    },
    UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetClientRect, GetForegroundWindow, GetWindow,
        GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    },
};
use windows_core::{BOOL, PWSTR};

/// Native values collected together before translation into selector facts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WindowInfo {
    /// Window handle represented without giving it cross-layer meaning.
    hwnd: usize,
    /// Process that owns the window.
    pid: u32,
    /// Lossily decoded top-level window title.
    title: String,
    /// Full executable path, or an empty path when access is denied.
    executable_path: PathBuf,
    /// Client-area width in physical pixels, or zero when unavailable.
    width: u32,
    /// Client-area height in physical pixels, or zero when unavailable.
    height: u32,
}

/// One selector fact paired with the native handle needed only after selection.
#[derive(Debug, Clone)]
pub struct NativeObservation {
    /// Plain facts consumed by `capture-core`'s deterministic selector.
    pub fact: ObservedWindow,
    /// Ephemeral native handle matching this exact observation snapshot.
    pub hwnd: HWND,
}

/// Observe one complete capturable-window snapshot without retaining OS objects.
pub fn observe_windows() -> Vec<NativeObservation> {
    let foreground = get_foreground_window().map(|window| window.hwnd);
    enumerate_windows()
        .into_iter()
        .map(|window| translate_window(window, foreground))
        .collect()
}

/// Translate one native observation record into an architecture-safe fact.
fn translate_window(window: WindowInfo, foreground: Option<usize>) -> NativeObservation {
    let executable_name = window
        .executable_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let executable_path = (!window.executable_path.as_os_str().is_empty())
        .then(|| window.executable_path.to_string_lossy().into_owned());
    NativeObservation {
        fact: ObservedWindow {
            id: ObservationId(window.hwnd as u64),
            process_id: window.pid,
            executable_name,
            executable_path,
            title: window.title,
            visible: true,
            foreground: foreground == Some(window.hwnd),
            bounds: WindowBounds {
                left: 0,
                top: 0,
                width: window.width,
                height: window.height,
            },
        },
        hwnd: HWND(window.hwnd as *mut core::ffi::c_void),
    }
}

/// Return the current foreground observation, if Windows exposes one.
fn get_foreground_window() -> Option<WindowInfo> {
    // SAFETY: The API has no caller preconditions and returns a null handle when absent.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    Some(read_window(hwnd))
}

/// Enumerate visible, uncloaked, unowned top-level windows with titles.
fn enumerate_windows() -> Vec<WindowInfo> {
    let mut windows = Vec::<WindowInfo>::new();
    let windows_ptr = &raw mut windows;
    let own_process_id = process::id();

    // SAFETY: Enumeration is synchronous, so the exclusive stack-local Vec
    // remains alive and unaliased for every callback invocation.
    let _enumeration_result = unsafe {
        EnumWindows(Some(enumerate_window), LPARAM(windows_ptr as _))
    };
    windows.retain(|window| window.pid != own_process_id);
    windows
}

/// Collect one eligible window and continue synchronous enumeration.
unsafe extern "system" fn enumerate_window(hwnd: HWND, state: LPARAM) -> BOOL {
    // SAFETY: `state` is the exclusive Vec pointer supplied by `enumerate_windows`
    // and remains valid until this synchronous callback returns.
    let windows = unsafe {
        (state.0 as *mut Vec<WindowInfo>).as_mut_unchecked()
    };
    if is_capturable(hwnd) {
        windows.push(read_window(hwnd));
    }
    TRUE
}

/// Decide whether a top-level handle belongs in the capture candidate set.
fn is_capturable(hwnd: HWND) -> bool {
    // SAFETY: The handle originates from `EnumWindows`; this query tolerates stale handles.
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    // SAFETY: The handle originates from `EnumWindows`; this query tolerates stale handles.
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) }.unwrap_or_default();
    if !visible || !owner.is_invalid() || is_cloaked(hwnd) {
        return false;
    }
    !get_window_title(hwnd).is_empty()
}

/// Read the native fields used by capture and platform-neutral selection.
fn read_window(hwnd: HWND) -> WindowInfo {
    let title = get_window_title(hwnd);
    let (pid, executable_path) = get_process_info(hwnd);
    let (width, height) = get_client_size(hwnd);
    WindowInfo {
        hwnd: hwnd.0 as usize,
        pid,
        title,
        executable_path,
        width,
        height,
    }
}

/// Read a window title, returning an empty string after an ordinary Win32 failure.
fn get_window_title(hwnd: HWND) -> String {
    // SAFETY: The enumerated handle may become stale; the API reports that as zero length.
    let buffer_length = unsafe { GetWindowTextLengthW(hwnd) } as usize + 1;
    let mut buffer = vec![0u16; buffer_length];
    // SAFETY: The allocated buffer includes the null terminator reported above.
    let _written = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if let Some(position) = buffer.iter().position(|character| *character == 0) {
        buffer.truncate(position);
    }
    String::from_utf16_lossy(&buffer)
}

/// Read the owning process and executable path without requiring elevated access.
fn get_process_info(hwnd: HWND) -> (u32, PathBuf) {
    let mut process_id = 0;
    // SAFETY: The process-ID pointer is a valid stack local and stale handles yield zero.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process_id)); }
    if process_id == 0 {
        return (0, PathBuf::new());
    }
    (process_id, get_executable_path(process_id).unwrap_or_default())
}

/// Query an executable path, returning `None` for protected or exited processes.
fn get_executable_path(process_id: u32) -> Option<PathBuf> {
    // SAFETY: The process ID came from Windows. The low-privilege handle is
    // closed on every path after opening, and the fixed buffer/length agree.
    #[expect(clippy::multiple_unsafe_ops_per_block, reason = "Windows API calls share one handle")]
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        if process.is_invalid() {
            return None;
        }

        let mut buffer = [0u16; 260];
        let mut length = buffer.len() as u32;
        let query_result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut length);
        let _close_result = CloseHandle(process);
        query_result.ok()?;
        Some(PathBuf::from(OsString::from_wide(&buffer[..length as usize])))
    }
}

/// Read the physical client-area dimensions, or zeros after an ordinary failure.
fn get_client_size(hwnd: HWND) -> (u32, u32) {
    let mut rectangle = RECT::default();
    // SAFETY: The rectangle pointer is a valid stack local; stale handles return an error.
    if unsafe { GetClientRect(hwnd, &raw mut rectangle) }.is_err() {
        return (0, 0);
    }
    (
        (rectangle.right - rectangle.left) as u32,
        (rectangle.bottom - rectangle.top) as u32)
}

/// Detect DWM-cloaked placeholders and windows on other virtual desktops.
fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: The output pointer names a correctly sized stack-local `u32`.
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32)
    };
    result.is_ok() && cloaked != 0
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn translation_should_preserve_native_identity_only_beside_plain_facts() {
        let translated = translate_window(WindowInfo {
            hwnd: 42,
            pid: 7,
            title: "Disposable target".to_owned(),
            executable_path: PathBuf::from(r"C:\Windows\System32\notepad.exe"),
            width: 800,
            height: 600,
        }, Some(42));

        assert_eq!(translated.hwnd.0 as usize, 42);
        assert_eq!(translated.fact.id, ObservationId(42));
        assert_eq!(translated.fact.executable_name, "notepad.exe");
        assert!(translated.fact.foreground);
    }
}
