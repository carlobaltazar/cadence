use crate::hotkeys;
use crate::sequence::{InputEvent, InputEventType, MouseButton};
use crate::timing::PrecisionTimer;
use crate::win32_helpers::lock_or_recover;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use winapi::shared::minwindef::{LPARAM, LRESULT, WPARAM};
use winapi::shared::windef::HHOOK;
use winapi::um::winuser::*;

fn is_filtered_vk(vk: u32) -> bool {
    hotkeys::all_hotkey_vks().contains(&(vk as u16))
}

struct RecordingState {
    events: Vec<InputEvent>,
    timer: PrecisionTimer,
    last_tick: i64,
    active: bool,
}

static RECORDING: Mutex<Option<RecordingState>> = Mutex::new(None);
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);
static KB_HOOK: AtomicIsize = AtomicIsize::new(0);

pub fn start_recording() {
    let timer = PrecisionTimer::new();
    let now = timer.now_ticks();
    let state = RecordingState {
        events: Vec::with_capacity(10000),
        timer,
        last_tick: now,
        active: true,
    };
    *lock_or_recover(&RECORDING) = Some(state);

    unsafe {
        let mouse = SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook_proc),
            std::ptr::null_mut(),
            0,
        );
        MOUSE_HOOK.store(mouse as isize, Ordering::Release);

        let kb = SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook_proc),
            std::ptr::null_mut(),
            0,
        );
        KB_HOOK.store(kb as isize, Ordering::Release);
    }

    println!("[Cadence] Recording started... Press F8 to stop.");
}

pub fn stop_recording() -> Option<Vec<InputEvent>> {
    let mouse = MOUSE_HOOK.load(Ordering::Acquire) as HHOOK;
    if !mouse.is_null() {
        unsafe { UnhookWindowsHookEx(mouse); }
        MOUSE_HOOK.store(0, Ordering::Release);
    }
    let kb = KB_HOOK.load(Ordering::Acquire) as HHOOK;
    if !kb.is_null() {
        unsafe { UnhookWindowsHookEx(kb); }
        KB_HOOK.store(0, Ordering::Release);
    }

    let mut guard = lock_or_recover(&RECORDING);
    if let Some(state) = guard.take() {
        let count = state.events.len();
        let duration_ms = if !state.events.is_empty() {
            let total_micros: i64 = state.events.iter().map(|e| e.delay_micros).sum();
            total_micros / 1000
        } else {
            0
        };
        println!(
            "[Cadence] Recording stopped. {} events captured ({} ms)",
            count, duration_ms
        );
        Some(state.events)
    } else {
        None
    }
}

pub fn is_recording() -> bool {
    lock_or_recover(&RECORDING)
        .as_ref()
        .map_or(false, |s| s.active)
}

fn push_event(event_type: InputEventType) {
    if let Ok(mut guard) = RECORDING.lock() {
        if let Some(state) = guard.as_mut() {
            if !state.active {
                return;
            }
            let now = state.timer.now_ticks();
            let delay_micros = state.timer.ticks_to_micros(now - state.last_tick);
            state.last_tick = now;
            state.events.push(InputEvent {
                delay_micros,
                event_type,
            });
        }
    }
}

unsafe extern "system" fn mouse_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        let info = &*(l_param as *const MSLLHOOKSTRUCT);

        // Skip injected events (from playback)
        if (info.flags & LLMHF_INJECTED) != 0 {
            return CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param);
        }

        let msg = w_param as u32;
        let event = match msg {
            WM_MOUSEMOVE => Some(InputEventType::MouseMove {
                x: info.pt.x,
                y: info.pt.y,
            }),
            WM_LBUTTONDOWN => Some(InputEventType::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            }),
            WM_LBUTTONUP => Some(InputEventType::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            }),
            WM_RBUTTONDOWN => Some(InputEventType::MouseButton {
                button: MouseButton::Right,
                pressed: true,
            }),
            WM_RBUTTONUP => Some(InputEventType::MouseButton {
                button: MouseButton::Right,
                pressed: false,
            }),
            WM_MBUTTONDOWN => Some(InputEventType::MouseButton {
                button: MouseButton::Middle,
                pressed: true,
            }),
            WM_MBUTTONUP => Some(InputEventType::MouseButton {
                button: MouseButton::Middle,
                pressed: false,
            }),
            WM_MOUSEWHEEL => {
                let delta = ((info.mouseData >> 16) & 0xFFFF) as i16 as i32;
                Some(InputEventType::MouseWheel { delta })
            }
            WM_XBUTTONDOWN => {
                let xbutton = ((info.mouseData >> 16) & 0xFFFF) as u16;
                let button = if xbutton == 1 {
                    MouseButton::X1
                } else {
                    MouseButton::X2
                };
                Some(InputEventType::MouseButton {
                    button,
                    pressed: true,
                })
            }
            WM_XBUTTONUP => {
                let xbutton = ((info.mouseData >> 16) & 0xFFFF) as u16;
                let button = if xbutton == 1 {
                    MouseButton::X1
                } else {
                    MouseButton::X2
                };
                Some(InputEventType::MouseButton {
                    button,
                    pressed: false,
                })
            }
            _ => None,
        };

        if let Some(evt) = event {
            push_event(evt);
        }
    }

    CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param)
}

unsafe extern "system" fn keyboard_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        let info = &*(l_param as *const KBDLLHOOKSTRUCT);

        // Skip injected events
        if (info.flags & LLKHF_INJECTED) != 0 {
            return CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param);
        }

        // Filter out our hotkeys
        if is_filtered_vk(info.vkCode) {
            return CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param);
        }

        let msg = w_param as u32;
        let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let extended = (info.flags & LLKHF_EXTENDED) != 0;

        push_event(InputEventType::KeyPress {
            vk_code: info.vkCode as u16,
            scan_code: info.scanCode as u16,
            pressed,
            extended,
        });
    }

    CallNextHookEx(std::ptr::null_mut(), n_code, w_param, l_param)
}
