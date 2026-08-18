use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryW};
use winapi::um::wingdi::*;
use winapi::um::winuser::*;

// ---- UTF-16 conversion ----

pub fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ---- Mutex safety ----

/// Lock a mutex, recovering from poison by accepting the poisoned guard.
/// Prevents panics when another thread panicked while holding the lock.
pub fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---- DPI scaling ----
//
// The process is Per-Monitor-V2 DPI aware (see main.rs), which is required so
// the HP/MP/SP pixel picker reads true physical screen pixels. The tradeoff is
// that Windows does NOT auto-scale our windows — every layout coordinate and
// font must be scaled to the monitor's DPI by hand. All layout literals in the
// codebase are authored at 96 DPI (100%); these helpers scale them up.

// Cached user32!GetDpiForWindow address: 0 = unresolved, -1 = unavailable.
static GET_DPI_FOR_WINDOW: AtomicIsize = AtomicIsize::new(0);

/// Effective DPI for a window. Falls back to the system DPI when `hwnd` is null
/// or GetDpiForWindow is unavailable, and to 96 (100%) as a last resort.
pub fn dpi_for_window(hwnd: HWND) -> u32 {
    unsafe {
        let mut addr = GET_DPI_FOR_WINDOW.load(Ordering::Acquire);
        if addr == 0 {
            let lib = LoadLibraryW(wide("user32.dll").as_ptr());
            addr = if lib.is_null() {
                -1
            } else {
                let p = GetProcAddress(lib, c"GetDpiForWindow".as_ptr());
                if p.is_null() { -1 } else { p as isize }
            };
            GET_DPI_FOR_WINDOW.store(addr, Ordering::Release);
        }
        if addr != -1 && !hwnd.is_null() {
            let f: unsafe extern "system" fn(HWND) -> u32 = std::mem::transmute(addr);
            let d = f(hwnd);
            if d != 0 {
                return d;
            }
        }
        // Fallback: system DPI from the screen DC.
        let hdc = GetDC(std::ptr::null_mut());
        if !hdc.is_null() {
            let d = GetDeviceCaps(hdc, LOGPIXELSX);
            ReleaseDC(std::ptr::null_mut(), hdc);
            if d > 0 {
                return d as u32;
            }
        }
        96
    }
}

/// Scale a 96-DPI design measurement to `dpi`, rounded to the nearest pixel.
pub fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

// One font per distinct DPI, created on demand and kept for the process
// lifetime (only a handful of DPIs ever occur in practice). Avoids per-window
// GDI-handle bookkeeping while still giving each window a correctly sized font.
static FONT_CACHE: Mutex<Vec<(u32, isize)>> = Mutex::new(Vec::new());

/// A Segoe UI font sized for `dpi`, matching the ~9pt stock GUI font at 100%.
/// Cached and shared across windows; never freed (lives for the process).
pub fn scaled_font(dpi: u32) -> HFONT {
    let mut cache = lock_or_recover(&FONT_CACHE);
    if let Some((_, h)) = cache.iter().find(|(d, _)| *d == dpi) {
        return *h as HFONT;
    }
    let face = wide("Segoe UI");
    let height = -(9 * dpi as i32) / 72; // 9pt in device pixels at this DPI
    let hfont = unsafe {
        CreateFontW(
            height, 0, 0, 0,
            FW_NORMAL,
            0, 0, 0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH | FF_DONTCARE,
            face.as_ptr(),
        )
    };
    cache.push((dpi, hfont as isize));
    hfont
}

// ---- Timed message box ----

/// Return value of user32!MessageBoxTimeoutW when the timer fired before any button.
#[allow(dead_code)] // callers currently only use the timeout as a dismissal, not the verdict
pub const MB_TIMEDOUT: i32 = 32000;

/// MessageBoxW that auto-dismisses after `timeout_ms`, returning MB_TIMEDOUT if nobody
/// answered in time. Uses the undocumented-but-stable-since-Windows-2000 user32 export
/// MessageBoxTimeoutW, resolved dynamically like GetDpiForWindow above. `timeout_ms == 0`
/// or a missing export degrade to a plain blocking MessageBoxW.
pub unsafe fn message_box_timeout(
    hwnd: HWND,
    text: &str,
    caption: &str,
    utype: u32,
    timeout_ms: u32,
) -> i32 {
    let wtext = wide(text);
    let wcaption = wide(caption);
    if timeout_ms != 0 {
        let lib = LoadLibraryW(wide("user32.dll").as_ptr());
        if !lib.is_null() {
            let p = GetProcAddress(lib, c"MessageBoxTimeoutW".as_ptr());
            if !p.is_null() {
                type MsgBoxTimeoutW = unsafe extern "system" fn(
                    HWND,
                    *const u16,
                    *const u16,
                    u32,
                    u16, // wLanguageId, 0 = neutral
                    u32, // milliseconds
                ) -> i32;
                let f: MsgBoxTimeoutW = std::mem::transmute(p);
                return f(hwnd, wtext.as_ptr(), wcaption.as_ptr(), utype, 0, timeout_ms);
            }
        }
    }
    MessageBoxW(hwnd, wtext.as_ptr(), wcaption.as_ptr(), utype)
}

// ---- Key options ----

pub const KEY_OPTIONS: &[(u16, &str)] = &[
    (0x70, "F1"),
    (0x71, "F2"),
    (0x72, "F3"),
    (0x73, "F4"),
    (0x74, "F5"),
    (0x75, "F6"),
    (0x76, "F7"),
    (0x77, "F8"),
    (0x78, "F9"),
    (0x79, "F10"),
    (0x7A, "F11"),
    (0x7B, "F12"),
    (0x24, "Home"),
    (0x23, "End"),
    (0x2D, "Insert"),
    (0x13, "Pause"),
    (0x91, "ScrollLock"),
];

pub fn vk_name(vk: u16) -> &'static str {
    KEY_OPTIONS
        .iter()
        .find(|(v, _)| *v == vk)
        .map(|(_, name)| *name)
        .unwrap_or("?")
}

/// Key options for the Burst Q hotkey. Excludes A-Z so a user can't bind it
/// to a key the game itself uses for skills/movement.
pub const BURST_KEY_OPTIONS: &[(u16, &str)] = &[
    (0x14, "CapsLock"),
    (0x2D, "Insert"),
    (0x24, "Home"),
    (0x23, "End"),
    (0x21, "PageUp"),
    (0x22, "PageDown"),
    (0x13, "Pause"),
    (0x91, "ScrollLock"),
    (0x70, "F1"),
    (0x71, "F2"),
    (0x72, "F3"),
    (0x73, "F4"),
    (0x74, "F5"),
    (0x75, "F6"),
    (0x76, "F7"),
    (0x77, "F8"),
    (0x78, "F9"),
    (0x79, "F10"),
    (0x7A, "F11"),
    (0x7B, "F12"),
];

/// Extended key options for remote hotkey bindings (A-Z, 0-9, F1-F12).
pub const REMOTE_KEY_OPTIONS: &[(u16, &str)] = &[
    (0x41, "A"), (0x42, "B"), (0x43, "C"), (0x44, "D"), (0x45, "E"),
    (0x46, "F"), (0x47, "G"), (0x48, "H"), (0x49, "I"), (0x4A, "J"),
    (0x4B, "K"), (0x4C, "L"), (0x4D, "M"), (0x4E, "N"), (0x4F, "O"),
    (0x50, "P"), (0x51, "Q"), (0x52, "R"), (0x53, "S"), (0x54, "T"),
    (0x55, "U"), (0x56, "V"), (0x57, "W"), (0x58, "X"), (0x59, "Y"),
    (0x5A, "Z"),
    (0x30, "0"), (0x31, "1"), (0x32, "2"), (0x33, "3"), (0x34, "4"),
    (0x35, "5"), (0x36, "6"), (0x37, "7"), (0x38, "8"), (0x39, "9"),
    (0x70, "F1"), (0x71, "F2"), (0x72, "F3"), (0x73, "F4"),
    (0x74, "F5"), (0x75, "F6"), (0x76, "F7"), (0x77, "F8"),
    (0x78, "F9"), (0x79, "F10"), (0x7A, "F11"), (0x7B, "F12"),
];

pub fn remote_vk_name(vk: u16) -> &'static str {
    REMOTE_KEY_OPTIONS
        .iter()
        .find(|(v, _)| *v == vk)
        .map(|(_, name)| *name)
        .unwrap_or("?")
}

// ---- Win32 control creation helpers ----

/// Create a child control with CreateWindowExW and set its font.
pub unsafe fn create_control(
    parent: HWND,
    hinstance: HINSTANCE,
    font: HFONT,
    class: &str,
    text: &str,
    style: u32,
    ex_style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: u16,
) -> HWND {
    // Layout literals are authored at 96 DPI; scale to the parent's monitor DPI.
    let dpi = dpi_for_window(parent);
    let wclass = wide(class);
    let wtext = wide(text);
    let hwnd = CreateWindowExW(
        ex_style,
        wclass.as_ptr(),
        wtext.as_ptr(),
        style,
        scale(x, dpi), scale(y, dpi), scale(w, dpi), scale(h, dpi),
        parent,
        id as usize as HMENU,
        hinstance,
        std::ptr::null_mut(),
    );
    SendMessageW(hwnd, WM_SETFONT, font as WPARAM, 1);
    hwnd
}

/// Register a window class and create a window.
pub unsafe fn register_and_create_dialog(
    class_name: &str,
    title: &str,
    wnd_proc: unsafe extern "system" fn(HWND, UINT, WPARAM, LPARAM) -> LRESULT,
    ex_style: u32,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: HWND,
    hinstance: HINSTANCE,
) -> HWND {
    let wclass = wide(class_name);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: std::ptr::null_mut(),
        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
        hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
        lpszMenuName: std::ptr::null(),
        lpszClassName: wclass.as_ptr(),
        hIconSm: std::ptr::null_mut(),
    };
    RegisterClassExW(&wc);

    // Scale the window size to the parent monitor's DPI; x/y are absolute screen
    // coordinates supplied by the caller and are left as-is.
    let dpi = dpi_for_window(parent);
    let wtitle = wide(title);
    CreateWindowExW(
        ex_style,
        wclass.as_ptr(),
        wtitle.as_ptr(),
        style,
        x, y, scale(w, dpi), scale(h, dpi),
        parent,
        std::ptr::null_mut(),
        hinstance,
        std::ptr::null_mut(),
    )
}

/// Populate a combobox with key options, selecting the one matching `selected_vk`.
pub unsafe fn populate_key_combo(h_combo: HWND, keys: &[(u16, &str)], selected_vk: Option<u16>) {
    for (i, (vk, name)) in keys.iter().enumerate() {
        let wname = wide(name);
        SendMessageW(h_combo, CB_ADDSTRING, 0, wname.as_ptr() as LPARAM);
        if selected_vk == Some(*vk) {
            SendMessageW(h_combo, CB_SETCURSEL, i, 0);
        }
    }
}
