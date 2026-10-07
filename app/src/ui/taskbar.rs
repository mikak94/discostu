//! The taskbar shows a window's *large* icon, but winit (via iced) only sets
//! the small one, so Windows falls back to the icon it cached for the exe's
//! path, which goes stale when the exe is replaced in place. Setting the
//! large icon from our own embedded resource sidesteps that cache.

#[cfg(windows)]
pub fn set_large_icon(hwnd: u64) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ICON_BIG, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SendMessageW, WM_SETICON,
    };
    unsafe {
        // Resource id 1 in discostu.rc; 64 px scales down cleanly at any DPI.
        let icon = LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as _, IMAGE_ICON, 64, 64, LR_DEFAULTCOLOR);
        if !icon.is_null() {
            SendMessageW(hwnd as _, WM_SETICON, ICON_BIG as _, icon as _);
        }
    }
}

#[cfg(not(windows))]
pub fn set_large_icon(_: u64) {}
