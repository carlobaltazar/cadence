use crate::config;
use crate::win32_helpers::lock_or_recover;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;
use winapi::shared::minwindef::*;
use winapi::shared::windef::*;
use winapi::um::winuser::*;

pub const HOTKEY_TOGGLE_RECORD: i32 = 1;
pub const HOTKEY_PLAY_STOP: i32 = 2;
pub const HOTKEY_PLAY_QUEUE: i32 = 3;
pub const HOTKEY_BURST_TOGGLE: i32 = 4;
pub const HOTKEY_PLAY_SEQUENCE: i32 = 100;
pub const HOTKEY_REMOTE_SEND: i32 = 200;

// Modifier bitmask constants (matching MOD_ALT, MOD_CONTROL, MOD_SHIFT)
pub const MOD_FLAG_ALT: u32 = 1;
pub const MOD_FLAG_CTRL: u32 = 2;
pub const MOD_FLAG_SHIFT: u32 = 4;

// Custom message posted to the main thread when a hotkey is detected
pub const WM_APP_HOTKEY: u32 = WM_APP + 1;

struct HotkeySet {
    record_vk: u16,
    stop_vk: u16,
    queue_vk: Option<u16>,
    burst_vk: Option<u16>,
    sequence_bindings: Vec<(u16, String)>, // (vk_code, sequence_name)
    remote_bindings: Vec<(u32, u16, String)>, // (modifiers, vk_code, sequence_name)
}

impl HotkeySet {
    fn new(record_vk: u16, stop_vk: u16) -> Self {
        HotkeySet {
            record_vk,
            stop_vk,
            queue_vk: None,
            burst_vk: None,
            sequence_bindings: Vec::new(),
            remote_bindings: Vec::new(),
        }
    }
}

impl Default for HotkeySet {
    fn default() -> Self {
        HotkeySet::new(config::DEFAULT_RECORD_VK, config::DEFAULT_STOP_VK)
    }
}

static CURRENT_HOTKEYS: Mutex<Option<HotkeySet>> = Mutex::new(None);
static MAIN_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static HOTKEY_HOOK: AtomicIsize = AtomicIsize::new(0);

/// Lock the shared hotkey set (lazily creating the default set on first use) and
/// run `f` against it. Centralizes the ensure + lock + access boilerplate.
fn with_set<R>(f: impl FnOnce(&mut HotkeySet) -> R) -> R {
    let mut hk = lock_or_recover(&CURRENT_HOTKEYS);
    let set = hk.get_or_insert_with(HotkeySet::default);
    f(set)
}

/// Install a persistent low-level keyboard hook to detect hotkeys globally.
/// This works even when fullscreen games have focus (unlike RegisterHotKey).
pub fn install_hook(record_vk: u16, stop_vk: u16) -> bool {
    *lock_or_recover(&CURRENT_HOTKEYS) = Some(HotkeySet::new(record_vk, stop_vk));

    // Store the calling thread's ID so the hook callback can post messages to it
    MAIN_THREAD_ID.store(
        unsafe { winapi::um::processthreadsapi::GetCurrentThreadId() },
        Ordering::Release,
    );

    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(hotkey_hook_proc),
            std::ptr::null_mut(),
            0,
        )
    };
    HOTKEY_HOOK.store(hook as isize, Ordering::Release);
    !hook.is_null()
}

pub fn uninstall_hook() {
    let hook = HOTKEY_HOOK.load(Ordering::Acquire) as HHOOK;
    if !hook.is_null() {
        unsafe { UnhookWindowsHookEx(hook); }
        HOTKEY_HOOK.store(0, Ordering::Release);
    }
}

pub fn reregister_hotkeys(record_vk: u16, stop_vk: u16) -> bool {
    with_set(|set| {
        set.record_vk = record_vk;
        set.stop_vk = stop_vk;
    });
    true
}

pub fn current_hotkeys() -> (u16, u16) {
    with_set(|set| (set.record_vk, set.stop_vk))
}

pub fn set_queue_vk(vk: Option<u16>) {
    with_set(|set| set.queue_vk = vk);
}

pub fn current_queue_vk() -> Option<u16> {
    with_set(|set| set.queue_vk)
}

pub fn set_burst_vk(vk: Option<u16>) {
    with_set(|set| set.burst_vk = vk);
}

pub fn current_burst_vk() -> Option<u16> {
    with_set(|set| set.burst_vk)
}

/// Rebuild sequence bindings from loaded sequences.
pub fn set_sequence_bindings(bindings: Vec<(u16, String)>) {
    with_set(|set| set.sequence_bindings = bindings);
}

/// Set remote hotkey bindings (modifier+key → sequence name).
pub fn set_remote_bindings(bindings: Vec<(u32, u16, String)>) {
    with_set(|set| set.remote_bindings = bindings);
}

/// Get the sequence name for a remote binding by index.
pub fn remote_binding_at(index: usize) -> Option<String> {
    with_set(|set| set.remote_bindings.get(index).map(|(_, _, name)| name.clone()))
}

/// Get the sequence name bound to a given VK, if any.
pub fn sequence_for_vk(vk: u16) -> Option<String> {
    with_set(|set| {
        set.sequence_bindings
            .iter()
            .find(|(v, _)| *v == vk)
            .map(|(_, name)| name.clone())
    })
}

/// True if `vk` is bound to any local hotkey (record/stop/queue/burst/sequence).
/// Used to filter our own hotkeys out of recordings without allocating a Vec on
/// every keystroke.
pub fn is_hotkey_vk(vk: u16) -> bool {
    with_set(|set| {
        vk == set.record_vk
            || vk == set.stop_vk
            || set.queue_vk == Some(vk)
            || set.burst_vk == Some(vk)
            || set.sequence_bindings.iter().any(|(v, _)| *v == vk)
    })
}

/// Get current sequence bindings for UI display.
pub fn current_sequence_bindings() -> Vec<(u16, String)> {
    with_set(|set| set.sequence_bindings.clone())
}

/// Get current modifier state using GetAsyncKeyState.
unsafe fn get_current_modifiers() -> u32 {
    let mut mods: u32 = 0;
    if GetAsyncKeyState(VK_MENU) < 0 {
        mods |= MOD_FLAG_ALT;
    }
    if GetAsyncKeyState(VK_CONTROL) < 0 {
        mods |= MOD_FLAG_CTRL;
    }
    if GetAsyncKeyState(VK_SHIFT) < 0 {
        mods |= MOD_FLAG_SHIFT;
    }
    mods
}

unsafe extern "system" fn hotkey_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 && w_param as u32 == WM_KEYDOWN {
        let info = &*(l_param as *const KBDLLHOOKSTRUCT);
        let is_injected = (info.flags & LLKHF_INJECTED) != 0;

        let vk = info.vkCode as u16;
        let thread_id = MAIN_THREAD_ID.load(Ordering::Acquire);

        // Use lock in hook callback
        if let Ok(hk) = CURRENT_HOTKEYS.lock() {
            if let Some(ref set) = *hk {
                // Remote bindings fire for both manual and injected events so that
                // recorded sequences containing modifier combos (e.g. Shift+Q) still
                // trigger remote sends during playback.
                let mods = get_current_modifiers();
                if mods != 0 {
                    for (i, (bind_mods, bind_vk, _)) in set.remote_bindings.iter().enumerate() {
                        if vk == *bind_vk && mods == *bind_mods {
                            PostThreadMessageW(
                                thread_id,
                                WM_APP_HOTKEY,
                                HOTKEY_REMOTE_SEND as WPARAM,
                                i as LPARAM,
                            );
                            return CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param);
                        }
                    }
                }

                // Local hotkeys only fire on real user input to prevent playback
                // from retriggering recording or cascading into other sequences.
                if is_injected {
                    return CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param);
                }

                // Then check record/stop/sequence hotkeys (no modifiers)
                if vk == set.record_vk {
                    PostThreadMessageW(thread_id, WM_APP_HOTKEY, HOTKEY_TOGGLE_RECORD as WPARAM, 0);
                } else if vk == set.stop_vk {
                    PostThreadMessageW(thread_id, WM_APP_HOTKEY, HOTKEY_PLAY_STOP as WPARAM, 0);
                } else if set.queue_vk == Some(vk) {
                    PostThreadMessageW(thread_id, WM_APP_HOTKEY, HOTKEY_PLAY_QUEUE as WPARAM, 0);
                } else if set.burst_vk == Some(vk) {
                    PostThreadMessageW(thread_id, WM_APP_HOTKEY, HOTKEY_BURST_TOGGLE as WPARAM, 0);
                } else {
                    for (bound_vk, _) in &set.sequence_bindings {
                        if vk == *bound_vk {
                            PostThreadMessageW(
                                thread_id,
                                WM_APP_HOTKEY,
                                HOTKEY_PLAY_SEQUENCE as WPARAM,
                                vk as LPARAM,
                            );
                            break;
                        }
                    }
                }
            }
        }
    }

    CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param)
}
