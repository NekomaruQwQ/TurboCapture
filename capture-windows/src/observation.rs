//! Windows enumeration translated into the platform-neutral selector boundary.

use capture_core::{ObservationId, ObservedWindow, WindowBounds};
use enumerate_windows::{WindowInfo, enumerate_windows, get_foreground_window};
use windows::Win32::Foundation::HWND;

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

/// Translate one legacy enumerator record into an architecture-safe fact.
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
